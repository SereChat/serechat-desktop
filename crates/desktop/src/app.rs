//! Application state, event routing and background work.
//!
//! Threading model: the winit thread owns all state and renders. Network
//! calls, tools, file imports, dialogs and searches run on short-lived
//! worker threads that report back through [`WorkerEvent`]s posted to the
//! event loop, and every file the app writes goes through one [`Writer`]
//! thread, so the UI never blocks.

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arboard::Clipboard;
use serechat::{
    AccessToken, Attachment, Client, Config, Error, MediaKind, MediaModel, MediaTicket, Model, Projects, ResponseRequest, SearchHit,
    Session, SessionStore, SessionSummary, StreamEvent, ToolCall, ToolSpec,
};
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::window::{CursorIcon, Theme, UserAttentionType, Window};

use crate::chat::{self, Chat, MediaRequest, Reasoning, ReasoningView, SendJob, ToolJob};
use crate::diff::{self, Diff};
use crate::gpu::{GpuError, Instance, Renderer};
use crate::image::{self, ImageAtlas, ImageKey};
use crate::login::Login;
use crate::paint::{Painter, Rect};
use crate::text::{Fonts, GlyphAtlas};
use crate::theme::{Palette, Scheme};
use crate::ui::{Ui, copy};
use crate::{attachments, platform, skills, tools};

/// Length of the cross-fade when the colour scheme changes.
const THEME_FADE_SECS: f32 = 0.25;

/// Name shown on the SereChat approval page.
const APP_NAME: &str = "SereChat Desktop";

/// Results delivered from worker threads.
pub enum WorkerEvent {
    /// Step 1 of sign-in finished.
    AuthRequested(Result<String, Error>),
    /// Step 2 of sign-in finished.
    AuthExchanged(Result<AccessToken, Error>),
    /// The model list arrived.
    Models(Result<Vec<Model>, Error>),
    /// A session's messages were read from disk.
    SessionLoaded {
        /// Conversation that requested them.
        conversation: u64,
        /// The session, or why it could not be read.
        result: Result<Session, Error>,
    },
    /// A streamed update for a reply.
    Stream {
        /// Conversation id.
        conversation: u64,
        /// Stream id.
        stream: u64,
        /// The update.
        event: StreamEvent,
    },
    /// A reply stream ended.
    StreamEnded {
        /// Conversation id.
        conversation: u64,
        /// Stream id.
        stream: u64,
        /// `Ok(true)` if completed, `Ok(false)` if cut short.
        result: Result<bool, Error>,
    },
    /// A tool call finished.
    ToolDone {
        /// Conversation that made the call.
        conversation: u64,
        /// The call's id.
        call_id: String,
        /// Output, or the error for the model.
        result: Result<String, String>,
        /// The image it returned (a screenshot), saved as an attachment.
        image: Option<Attachment>,
    },
    /// Files were copied in as attachments (or failed to).
    Imported(Vec<Result<Attachment, String>>),
    /// The file picker closed.
    FilesPicked(Vec<PathBuf>),
    /// The folder picker closed.
    FolderPicked(Option<PathBuf>),
    /// Message-content search results.
    SearchResults {
        /// Request generation, to drop stale results.
        generation: u64,
        /// Matching sessions.
        hits: Vec<SearchHit>,
    },
    /// A thumbnail was decoded (or could not be).
    Thumbnail(ImageKey, Result<Vec<u8>, String>),
    /// The generation models of one kind arrived.
    MediaModels(MediaKind, Result<Vec<MediaModel>, Error>),
    /// The server accepted a generation.
    MediaStarted {
        /// Conversation id.
        conversation: u64,
        /// Entry waiting for it.
        entry: u64,
        /// Its job and price.
        ticket: MediaTicket,
    },
    /// A generation finished (or failed, or nobody waits any more: `Ok(None)`).
    MediaDone {
        /// Conversation id.
        conversation: u64,
        /// Entry waiting for it.
        entry: u64,
        /// The downloaded file, or why there is none.
        result: Result<Option<Attachment>, String>,
    },
    /// The diff of a file change was computed.
    DiffReady {
        /// Conversation that made the call.
        conversation: u64,
        /// The call's id.
        call_id: String,
        /// The change.
        diff: Arc<Diff>,
    },
    /// The skills (and `AGENTS.md`) of a project were found.
    Skills {
        /// Project folder scanned; `None` for the user's skills.
        project: Option<String>,
        /// The scan's number, or `None` for one a request made on its own.
        generation: Option<u64>,
        /// What it found.
        catalog: Arc<skills::Catalog>,
    },
}

/// Something a screen wants done that needs app-level resources.
pub enum Action {
    /// Begin the device-code flow.
    StartLogin,
    /// Open the approval page for a request id.
    OpenAuthPage(String),
    /// Exchange a code for a token.
    VerifyCode {
        /// Request being approved.
        request_id: String,
        /// Six-digit code typed by the user.
        code: String,
    },
    /// Stream a reply.
    Send(SendJob),
    /// Run a tool call.
    RunTool(ToolJob),
    /// Diff a file change waiting for approval against its file.
    PreviewChange {
        /// Conversation that made the call.
        conversation: u64,
        /// Project folder the call is confined to.
        root: PathBuf,
        /// The `write_file` or `edit_file` call.
        call: ToolCall,
    },
    /// Run a generation and download its file.
    Generate(MediaRequest),
    /// Persist a model choice.
    SelectModel(String),
    /// Persist a generation model choice.
    SelectMediaModel(MediaKind, String),
    /// Persist a reasoning effort.
    SetReasoning(Reasoning),
    /// Switch and persist the colour scheme.
    SetTheme(Scheme),
    /// Switch and persist how replies show reasoning.
    SetReasoningView(ReasoningView),
    /// Read a session's messages on a worker thread.
    LoadSession {
        /// Conversation waiting for them.
        conversation: u64,
        /// Session id.
        session: String,
    },
    /// Write a session to disk.
    SaveSession(Session),
    /// Delete a session file by id.
    DeleteSession(String),
    /// Stop the background processes a conversation started and close its
    /// browser tab.
    StopProcesses(u64),
    /// Look for the skills of a project (`None`: the user's only).
    ScanSkills {
        /// Project folder, or `None` for the user's skills.
        project: Option<String>,
        /// Numbers the scan so an older result never replaces a newer one.
        generation: u64,
    },
    /// Search every session's messages.
    Search {
        /// Text to find.
        query: String,
        /// Request generation.
        generation: u64,
    },
    /// Show the file picker for attachments.
    PickFiles,
    /// Copy files in as attachments.
    ImportFiles(Vec<PathBuf>),
    /// Open a folder as a project (`None`: ask with the folder picker).
    OpenProject(Option<PathBuf>),
    /// Persist the current project (`None`: no project).
    SetProject(Option<String>),
    /// Remove a project from the list.
    ForgetProject(String),
    /// Show `~/.serechat/sessions` in the file manager.
    OpenDataDir,
    /// Open a link from a reply in the browser.
    OpenLink(String),
    /// Open an attachment with its default application.
    OpenPath(String),
    /// Put text on the clipboard.
    Copy(String),
    /// A run finished or needs the user: flash the window if it is in the background.
    Attention,
    /// Forget the token and return to sign-in.
    SignOut,
}

/// Failure to start the app.
#[derive(Debug)]
pub enum StartupError {
    /// The OS refused to create a window.
    Window(winit::error::OsError),
    /// GPU initialisation failed.
    Gpu(GpuError),
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Window(e) => write!(f, "cannot create the window: {e}"),
            Self::Gpu(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for StartupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Window(e) => Some(e),
            Self::Gpu(e) => Some(e),
        }
    }
}

enum Screen {
    Login(Login),
    // Boxed: the chat screen is far larger than the login screen.
    Chat(Box<Chat>),
}

/// The running application.
pub struct App {
    window: Arc<Window>,
    renderer: Renderer,
    fonts: Fonts,
    atlas: GlyphAtlas,
    images: ImageAtlas,
    instances: Vec<Instance>,
    proxy: EventLoopProxy<WorkerEvent>,
    clipboard: Option<Clipboard>,
    ui: Ui,
    start: Instant,
    last_frame: Instant,
    config: Config,
    client: Client,
    /// Where sessions are saved; `None` when there is no home directory.
    sessions_dir: Option<PathBuf>,
    /// Performs every file write, in order, off the UI thread.
    writer: Writer,
    /// Project folders; `None` when there is no home directory.
    projects: Option<Projects>,
    screen: Screen,
    scheme: Scheme,
    /// Colours drawn this frame; differs from `scheme` during a cross-fade.
    palette: Palette,
    /// Palette a cross-fade started from, and its progress in `0..=1`.
    fade: Option<(Palette, f32)>,
    /// Whether the window has been made visible (after the first frame).
    shown: bool,
    /// Cursor currently set on the window.
    cursor: CursorIcon,
    /// Input-method candidate area last sent to the window.
    ime_area: Option<Rect>,
}

impl App {
    /// Creates the window and GPU state and picks the first screen.
    ///
    /// # Errors
    /// See [`StartupError`].
    pub fn new(event_loop: &ActiveEventLoop, proxy: EventLoopProxy<WorkerEvent>) -> Result<Self, StartupError> {
        let config = Config::load().unwrap_or_else(|e| {
            eprintln!("serechat: ignoring unreadable config: {e}");
            Config::default()
        });
        let scheme = Scheme::from_key(config.theme.as_deref());
        let attributes = Window::default_attributes()
            .with_title("SereChat")
            .with_inner_size(LogicalSize::new(1200.0, 800.0))
            .with_min_inner_size(LogicalSize::new(760.0, 520.0))
            .with_theme(Some(window_theme(scheme)))
            // Shown after the first frame so the user never sees a blank window.
            .with_visible(false);
        // Let the sidebar run under a transparent title bar on macOS.
        #[cfg(target_os = "macos")]
        let attributes = {
            use winit::platform::macos::WindowAttributesExtMacOS;
            attributes.with_titlebar_transparent(true).with_fullsize_content_view(true).with_title_hidden(true)
        };
        // Wayland's app id and X11's WM_CLASS: desktops match it to the
        // installed `serechat.desktop` for the name and icon.
        #[cfg(target_os = "linux")]
        let attributes = winit::platform::wayland::WindowAttributesExtWayland::with_name(attributes, "serechat", "serechat");
        let window = Arc::new(event_loop.create_window(attributes).map_err(StartupError::Window)?);
        set_icon(&window);
        // Chinese, Japanese and Korean input methods deliver text through IME events.
        window.set_ime_allowed(true);
        let renderer = block_on(Renderer::new(Arc::clone(&window), event_loop.owned_display_handle())).map_err(StartupError::Gpu)?;

        let client = Client::new(config.token.clone());
        let sessions_dir = SessionStore::open()
            .inspect_err(|e| eprintln!("serechat: sessions will not be saved: {e}"))
            .ok()
            .map(|store| store.dir().to_owned());
        let projects = Projects::load().inspect_err(|e| eprintln!("serechat: cannot read projects: {e}")).ok();
        let now = Instant::now();
        let mut app = Self {
            window,
            renderer,
            fonts: Fonts::load(),
            atlas: GlyphAtlas::default(),
            images: ImageAtlas::default(),
            instances: Vec::new(),
            proxy,
            clipboard: None,
            ui: Ui::default(),
            start: now,
            last_frame: now,
            screen: Screen::Login(Login::new(None)),
            config,
            client,
            writer: Writer::start(sessions_dir.clone()),
            sessions_dir,
            projects,
            scheme,
            palette: *scheme.palette(),
            fade: None,
            shown: false,
            cursor: CursorIcon::Default,
            ime_area: None,
        };
        app.ui.focused = true;
        if app.config.token.is_some() {
            app.enter_chat();
        }
        // Draw the first frame directly: hidden windows never receive
        // `RedrawRequested` on Windows, so waiting for one would keep the
        // window invisible forever. `frame` shows it once drawn.
        app.frame();
        Ok(app)
    }

    fn enter_chat(&mut self) {
        // Only the index is read here; messages load when a session opens.
        let sessions = self.writer.list_sessions();
        let reasoning = Reasoning::from_key(self.config.reasoning.as_deref());
        let projects = self.projects.as_ref().map(|p| p.list.clone()).unwrap_or_default();
        let mut chat = Chat::new(self.config.model.clone(), reasoning, sessions, projects, self.config.project.clone());
        chat.set_reasoning_view(ReasoningView::from_key(self.config.reasoning_view.as_deref()));
        let chosen = [&self.config.image_model, &self.config.video_model, &self.config.audio_model];
        for (kind, model) in MediaKind::ALL.into_iter().zip(chosen) {
            chat.set_media_choice(kind, model.clone());
        }
        // Skills are known before the first message: the user's and the open project's.
        let mut actions = Vec::new();
        chat.refresh_skills(true, &mut actions);
        self.screen = Screen::Chat(Box::new(chat));
        self.spawn(|client, _| WorkerEvent::Models(client.models()));
        for kind in MediaKind::ALL {
            self.spawn(move |client, _| WorkerEvent::MediaModels(kind, client.media_models(kind)));
        }
        self.apply(actions);
    }

    /// Starts a cross-fade to `scheme` and persists it.
    fn set_scheme(&mut self, scheme: Scheme) {
        self.fade = Some((self.palette, 0.0));
        self.scheme = scheme;
        self.window.set_theme(Some(window_theme(scheme)));
        self.config.theme = Some(scheme.key().to_owned());
        self.save_config();
    }

    fn sign_out(&mut self, reason: Option<String>) {
        if let Screen::Chat(chat) = &mut self.screen {
            chat.cancel_all();
        }
        self.config.token = None;
        self.save_config();
        self.client = Client::new(None);
        self.screen = Screen::Login(Login::new(reason));
    }

    fn save_config(&mut self) {
        self.writer.send(Job::SaveConfig(self.config.clone()));
    }

    fn save_projects(&mut self) {
        if let Some(projects) = &self.projects {
            self.writer.send(Job::SaveProjects(projects.clone()));
        }
    }

    /// A store for reading sessions on a worker thread. Reads never see a
    /// half-written file: the writer replaces files by atomic rename.
    fn reader(&self) -> Option<SessionStore> {
        self.sessions_dir.clone().map(SessionStore::at)
    }

    /// Runs `job` on a worker thread and posts its result to the event loop.
    fn spawn(&self, job: impl FnOnce(&Client, &EventLoopProxy<WorkerEvent>) -> WorkerEvent + Send + 'static) {
        let client = self.client.clone();
        let proxy = self.proxy.clone();
        let spawned = std::thread::Builder::new().name("serechat-worker".into()).spawn(move || {
            let event = job(&client, &proxy);
            // Fails only if the event loop is gone, i.e. the app is exiting.
            let _ = proxy.send_event(event);
        });
        if let Err(e) = spawned {
            eprintln!("serechat: cannot spawn worker thread: {e}");
        }
    }

    /// Routes a window event.
    pub fn window_event(&mut self, event_loop: &ActiveEventLoop, event: WindowEvent) {
        let scale = self.window.scale_factor() as f32;
        let mut actions = Vec::new();
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                self.frame();
                return;
            }
            WindowEvent::Resized(size) => self.renderer.resize(size.width, size.height),
            WindowEvent::Focused(focused) => {
                self.ui.focused = focused;
                // Skills or AGENTS.md may have been edited in another app.
                if let (true, Screen::Chat(chat)) = (focused, &mut self.screen) {
                    chat.refresh_skills(false, &mut actions);
                }
            }
            WindowEvent::ModifiersChanged(mods) => self.ui.mods = mods.state(),
            WindowEvent::CursorMoved { position, .. } => {
                self.ui.mouse = (position.x as f32 / scale, position.y as f32 / scale);
            }
            WindowEvent::CursorLeft { .. } => self.ui.mouse = (f32::MIN, f32::MIN),
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                if state == ElementState::Pressed {
                    self.ui.press(self.start.elapsed().as_secs_f32());
                } else {
                    self.ui.down = false;
                    self.ui.released = true;
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x * 100.0, y * 100.0),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32 / scale, p.y as f32 / scale),
                };
                // Shift turns a plain wheel sideways, as everywhere else.
                let (dx, dy) = if self.ui.mods.shift_key() && dx == 0.0 { (dy, 0.0) } else { (dx, dy) };
                self.ui.scroll -= dy;
                self.ui.scroll_x -= dx;
            }
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } if event.state == ElementState::Pressed => {
                match &mut self.screen {
                    Screen::Login(login) => login.key(&event, self.ui.mods, &mut self.clipboard, &mut actions),
                    Screen::Chat(chat) => chat.key(&event, self.ui.mods, &mut self.clipboard, &mut actions),
                }
                self.ui.last_edit = self.ui.time;
            }
            WindowEvent::Ime(ime) => {
                match (ime, &mut self.screen) {
                    (Ime::Commit(text), Screen::Chat(chat)) => chat.ime_commit(&text),
                    (Ime::Commit(text), Screen::Login(login)) => login.commit(&text, &mut actions),
                    (Ime::Preedit(text, cursor), Screen::Chat(chat)) => chat.ime_preedit(text, cursor),
                    (Ime::Disabled, Screen::Chat(chat)) => chat.ime_preedit(String::new(), None),
                    _ => return,
                }
                self.ui.last_edit = self.ui.time;
            }
            WindowEvent::HoveredFile(path) => {
                if let Screen::Chat(chat) = &mut self.screen {
                    chat.drag_hover(Some(&path));
                }
            }
            WindowEvent::HoveredFileCancelled => {
                if let Screen::Chat(chat) = &mut self.screen {
                    chat.drag_hover(None);
                }
            }
            WindowEvent::DroppedFile(path) => {
                if let Screen::Chat(chat) = &mut self.screen {
                    chat.dropped(path, &mut actions);
                }
            }
            _ => return,
        }
        self.apply(actions);
        self.window.request_redraw();
    }

    /// Applies a result from a worker thread.
    pub fn worker_event(&mut self, event: WorkerEvent) {
        let mut actions = Vec::new();
        match (event, &mut self.screen) {
            (WorkerEvent::Thumbnail(key, pixels), _) => self.images.insert(key, pixels),
            (WorkerEvent::AuthRequested(result), Screen::Login(login)) => {
                if let Some(request_id) = login.requested(result) {
                    self.open_auth_page(&request_id);
                }
            }
            (WorkerEvent::AuthExchanged(result), Screen::Login(login)) => {
                if let Some(token) = login.exchanged(result) {
                    self.client = self.client.with_token(token.access_token.clone());
                    self.config.token = Some(token.access_token);
                    self.save_config();
                    self.enter_chat();
                }
            }
            (WorkerEvent::Models(Ok(models)), Screen::Chat(chat)) => chat.set_models(models),
            (WorkerEvent::Models(Err(e)), _) => eprintln!("serechat: could not load models: {e}"),
            (WorkerEvent::SessionLoaded { conversation, result }, Screen::Chat(chat)) => chat.session_loaded(conversation, result, &mut actions),
            (WorkerEvent::Stream { conversation, stream, event }, Screen::Chat(chat)) => {
                chat.stream_event(conversation, stream, event);
            }
            (WorkerEvent::StreamEnded { conversation, stream, result }, Screen::Chat(chat)) => {
                if chat.stream_end(conversation, stream, result, &mut actions) {
                    self.apply(actions);
                    self.sign_out(Some("Your session has expired. Please sign in again.".into()));
                    self.window.request_redraw();
                    return;
                }
            }
            (WorkerEvent::ToolDone { conversation, call_id, result, image }, Screen::Chat(chat)) => {
                chat.tool_done(conversation, &call_id, result, image, &mut actions);
            }
            (WorkerEvent::MediaModels(kind, Ok(models)), Screen::Chat(chat)) => chat.set_media_models(kind, models),
            (WorkerEvent::MediaModels(kind, Err(e)), _) => eprintln!("serechat: could not load {} models: {e}", kind.noun()),
            (WorkerEvent::MediaStarted { conversation, entry, ticket }, Screen::Chat(chat)) => chat.media_started(conversation, entry, ticket, &mut actions),
            (WorkerEvent::MediaDone { conversation, entry, result }, Screen::Chat(chat)) => chat.media_done(conversation, entry, result, &mut actions),
            (WorkerEvent::DiffReady { conversation, call_id, diff }, Screen::Chat(chat)) => chat.diff_ready(conversation, &call_id, diff),
            (WorkerEvent::Imported(results), Screen::Chat(chat)) => chat.attachments_imported(results),
            (WorkerEvent::FilesPicked(paths), Screen::Chat(chat)) => chat.attach(paths, &mut actions),
            (WorkerEvent::FolderPicked(Some(path)), Screen::Chat(_)) => actions.push(Action::OpenProject(Some(path))),
            (WorkerEvent::SearchResults { generation, hits }, Screen::Chat(chat)) => chat.search_results(generation, hits),
            (WorkerEvent::Skills { project, generation, catalog }, Screen::Chat(chat)) => chat.skills_scanned(project, generation, catalog),
            // Results for a screen that is no longer shown.
            _ => return,
        }
        self.apply(actions);
        self.window.request_redraw();
    }

    fn apply(&mut self, actions: Vec<Action>) {
        for action in actions {
            self.run(action);
        }
    }

    fn run(&mut self, action: Action) {
        match action {
            Action::StartLogin => {
                self.spawn(|client, _| WorkerEvent::AuthRequested(client.request_authorization(APP_NAME)));
            }
            Action::OpenAuthPage(request_id) => self.open_auth_page(&request_id),
            Action::VerifyCode { request_id, code } => {
                self.spawn(move |client, _| WorkerEvent::AuthExchanged(client.exchange_code(&request_id, &code)));
            }
            Action::Send(job) => self.spawn(move |client, proxy| {
                let (conversation, stream) = (job.conversation, job.stream);
                let result = match chat::input_items(&job.history) {
                    Err(message) => Err(Error::Io(std::io::Error::other(message))),
                    Ok(input) => {
                        // Catalogs not scanned yet are scanned here, and cached for next time.
                        let scanned = |project: Option<String>, catalog: skills::Catalog| {
                            let catalog = Arc::new(catalog);
                            let _ = proxy.send_event(WorkerEvent::Skills { project, generation: None, catalog: Arc::clone(&catalog) });
                            catalog
                        };
                        let user = job.user_skills.clone().unwrap_or_else(|| scanned(None, skills::scan_user()));
                        let project = match (&job.project, &job.project_skills) {
                            (Some(root), None) => Some(scanned(Some(root.clone()), skills::scan_project(std::path::Path::new(root)))),
                            (_, known) => known.clone(),
                        };
                        let (list, _) = skills::merge(project.as_deref(), Some(&user));
                        let agents_md = project.as_ref().and_then(|c| c.agents_md.as_deref());
                        let instructions = format!("{}{}", job.instructions, skills::prompt(agents_md, &list));
                        let skill_tool = skills::tool(&list);
                        let mut specs: Vec<ToolSpec<'_>> = Vec::new();
                        if job.tools {
                            specs.extend(tools::all().iter().map(|t| ToolSpec { name: t.name, description: &t.description, parameters: &t.parameters }));
                        }
                        // Skills work in every chat, with or without a project.
                        if let Some((description, parameters)) = &skill_tool {
                            specs.push(ToolSpec { name: "use_skill", description, parameters });
                        }
                        let request = ResponseRequest {
                            model: &job.model,
                            instructions: Some(&instructions),
                            reasoning: job.reasoning,
                            input: &input,
                            tools: &specs,
                            tool_choice: job.tool_choice,
                        };
                        client.stream_response(&request, &job.cancel, |event| {
                            let _ = proxy.send_event(WorkerEvent::Stream { conversation, stream, event });
                        })
                    }
                };
                WorkerEvent::StreamEnded { conversation, stream, result }
            }),
            Action::RunTool(job) => {
                let dir = self.reader().map_or_else(|| std::env::temp_dir().join("serechat-attachments"), |store| store.attachments_dir());
                self.spawn(move |client, proxy| {
                    // A file change is diffed against the file as it was just before.
                    let change = job.root.as_deref().filter(|_| tools::changes_file(&job.call.name)).and_then(|root| tools::file_change(root, &job.call));
                    let result = match &job.root {
                        _ if job.call.name == "use_skill" => skills::run(&job.skills, &job.call.arguments).map(tools::Output::text),
                        Some(root) => tools::run(root, job.conversation, client, &job.call, &job.cancel),
                        None => Err("Tools are only available in a project.".to_owned()),
                    };
                    if let (Ok(_), Some((before, after, whole))) = (&result, change) {
                        let diff = Arc::new(diff::diff(&before, &after, whole));
                        let _ = proxy.send_event(WorkerEvent::DiffReady { conversation: job.conversation, call_id: job.call.call_id.clone(), diff });
                    }
                    // A screenshot is kept with the session's attachments.
                    let (result, image) = match result {
                        Ok(tools::Output { text, image: Some(jpeg) }) => match attachments::save_screenshot(&jpeg, &dir) {
                            Ok(image) => (Ok(text), Some(image)),
                            Err(e) => (Err(e), None),
                        },
                        Ok(output) => (Ok(output.text), None),
                        Err(e) => (Err(e), None),
                    };
                    WorkerEvent::ToolDone { conversation: job.conversation, call_id: job.call.call_id, result, image }
                });
            }
            Action::PreviewChange { conversation, root, call } => self.spawn(move |_, _| {
                let (before, after, whole) = tools::file_change(&root, &call).unwrap_or_default();
                WorkerEvent::DiffReady { conversation, call_id: call.call_id, diff: Arc::new(diff::diff(&before, &after, whole)) }
            }),
            Action::Generate(request) => {
                let dir = self.reader().map_or_else(|| std::env::temp_dir().join("serechat-attachments"), |store| store.attachments_dir());
                self.spawn(move |client, proxy| {
                    let MediaRequest { conversation, entry, kind, model, mode, prompt, job, cancel } = request;
                    let job = match job {
                        Some(job) => job,
                        None => match client.generate_media(kind, &model, mode.as_deref(), &prompt) {
                            Ok(ticket) => {
                                let id = ticket.id.clone();
                                let _ = proxy.send_event(WorkerEvent::MediaStarted { conversation, entry, ticket });
                                id
                            }
                            Err(e) => return WorkerEvent::MediaDone { conversation, entry, result: Err(e.to_string()) },
                        },
                    };
                    let result = attachments::fetch_generated(client, kind, &job, &prompt, &dir, &cancel);
                    WorkerEvent::MediaDone { conversation, entry, result }
                });
            }
            Action::SelectModel(model) => {
                self.config.model = Some(model);
                self.save_config();
            }
            Action::SelectMediaModel(kind, model) => {
                let slot = match kind {
                    MediaKind::Image => &mut self.config.image_model,
                    MediaKind::Video => &mut self.config.video_model,
                    MediaKind::Audio => &mut self.config.audio_model,
                };
                *slot = Some(model);
                self.save_config();
            }
            Action::SetReasoning(reasoning) => {
                self.config.reasoning = Some(reasoning.key().to_owned());
                self.save_config();
            }
            Action::SetTheme(scheme) => self.set_scheme(scheme),
            Action::SetReasoningView(view) => {
                if let Screen::Chat(chat) = &mut self.screen {
                    chat.set_reasoning_view(view);
                }
                self.config.reasoning_view = Some(view.key().to_owned());
                self.save_config();
            }
            Action::LoadSession { conversation, session } => {
                if let Some(store) = self.reader() {
                    self.spawn(move |_, _| WorkerEvent::SessionLoaded { conversation, result: store.load(&session) });
                }
            }
            Action::SaveSession(session) => self.writer.send(Job::SaveSession(session)),
            Action::DeleteSession(id) => self.writer.send(Job::DeleteSession(id)),
            // Stopping waits for the processes to exit, so off the UI thread.
            Action::ScanSkills { project, generation } => self.spawn(move |_, _| {
                let catalog = match &project {
                    Some(root) => skills::scan_project(std::path::Path::new(root)),
                    None => skills::scan_user(),
                };
                WorkerEvent::Skills { project, generation: Some(generation), catalog: Arc::new(catalog) }
            }),
            Action::StopProcesses(owner) => std::mem::drop(std::thread::Builder::new().spawn(move || {
                crate::process::stop_owner(owner);
                crate::browser::close(owner);
            })),
            Action::Search { query, generation } => {
                if let Some(store) = self.reader() {
                    self.spawn(move |_, _| WorkerEvent::SearchResults { generation, hits: store.search(&query, 30).unwrap_or_default() });
                }
            }
            Action::PickFiles => self.spawn(|_, _| WorkerEvent::FilesPicked(platform::pick_files().unwrap_or_default())),
            Action::ImportFiles(paths) => {
                let dir = self.reader().map_or_else(|| std::env::temp_dir().join("serechat-attachments"), |store| store.attachments_dir());
                self.spawn(move |_, _| WorkerEvent::Imported(paths.iter().map(|path| attachments::import(path, &dir)).collect()));
            }
            Action::OpenProject(None) => self.spawn(|_, _| WorkerEvent::FolderPicked(platform::pick_folder().ok().flatten())),
            Action::OpenProject(Some(path)) => {
                let project = match &mut self.projects {
                    Some(projects) => projects.touch(&path).clone(),
                    None => serechat::Project::new(&path),
                };
                self.save_projects();
                let mut more = Vec::new();
                if let Screen::Chat(chat) = &mut self.screen {
                    chat.project_opened(project, &mut more);
                }
                self.apply(more);
            }
            Action::SetProject(project) => {
                if let (Some(projects), Some(path)) = (&mut self.projects, &project) {
                    projects.touch(std::path::Path::new(path));
                }
                self.save_projects();
                self.config.project = project;
                self.save_config();
            }
            Action::ForgetProject(path) => {
                if let Some(projects) = &mut self.projects {
                    projects.remove(&path);
                }
                self.save_projects();
            }
            Action::OpenDataDir => {
                if let Some(dir) = &self.sessions_dir {
                    // The folder may not exist before the first save.
                    let _ = std::fs::create_dir_all(dir);
                    if let Err(e) = platform::open_folder(dir) {
                        eprintln!("serechat: cannot open the data folder: {e}");
                    }
                }
            }
            Action::OpenLink(url) => {
                // Only web and mail links: a reply must never launch local programs.
                let safe = ["https://", "http://", "mailto:"].iter().any(|scheme| url.starts_with(scheme));
                if safe && let Err(e) = platform::open_url(&url) {
                    eprintln!("serechat: cannot open link: {e}");
                }
            }
            Action::OpenPath(path) => {
                if let Err(e) = platform::open_folder(std::path::Path::new(&path)) {
                    eprintln!("serechat: cannot open {path}: {e}");
                }
            }
            Action::Copy(text) => copy(&mut self.clipboard, &text),
            Action::Attention if !self.ui.focused => self.window.request_user_attention(Some(UserAttentionType::Informational)),
            Action::Attention => {}
            Action::SignOut => self.sign_out(None),
        }
    }

    fn open_auth_page(&mut self, request_id: &str) {
        let url = self.client.authorize_url(request_id);
        if let Err(e) = platform::open_url(&url) {
            eprintln!("serechat: cannot open browser: {e}");
            copy(&mut self.clipboard, &url);
            if let Screen::Login(login) = &mut self.screen {
                login.browser_failed();
            }
        }
    }

    /// Builds and presents one frame.
    fn frame(&mut self) {
        self.tick();
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().min(0.05);
        self.last_frame = now;
        self.ui.begin(dt, (now - self.start).as_secs_f32());

        // Cross-fade between colour schemes with a smoothstep ease.
        let target = self.scheme.palette();
        self.palette = match &mut self.fade {
            Some((from, progress)) if *progress < 1.0 => {
                *progress = (*progress + dt / THEME_FADE_SECS).min(1.0);
                let eased = *progress * *progress * (3.0 - 2.0 * *progress);
                self.ui.animating = true;
                from.mix(target, eased)
            }
            _ => {
                self.fade = None;
                *target
            }
        };

        let scale = self.window.scale_factor() as f32;
        let size = self.window.inner_size();
        let view = Rect::new(0.0, 0.0, size.width as f32 / scale, size.height as f32 / scale);
        let mut actions = Vec::new();
        // A second pass runs only if the glyph atlas overflowed mid-frame.
        for _ in 0..2 {
            let mut painter = Painter::new(&self.fonts, (&mut self.atlas, &mut self.images), &mut self.instances, scale, view, &self.palette);
            match &mut self.screen {
                Screen::Login(login) => login.draw(&mut painter, &mut self.ui, view, &mut actions),
                Screen::Chat(chat) => chat.draw(&mut painter, &mut self.ui, view, self.scheme, &mut actions),
            }
            if !painter.atlas_full {
                break;
            }
            // The first pass already acted on this frame's input; the redraw
            // must not act on it twice.
            self.atlas.clear();
            self.ui.end();
        }

        self.renderer.render(&self.instances, &mut self.atlas.uploads, &mut self.images.uploads, scale, self.palette.bg);
        self.ui.end();
        if !self.shown {
            self.shown = true;
            self.window.set_visible(true);
            // Some platforms refuse to present to a hidden surface; draw
            // again now that the window is visible.
            self.window.request_redraw();
        }
        if self.cursor != self.ui.cursor {
            self.cursor = self.ui.cursor;
            self.window.set_cursor(self.cursor);
        }
        // Keep the input method's candidate window next to the caret.
        let area = match &self.screen {
            Screen::Chat(chat) => chat.ime_area(),
            Screen::Login(_) => None,
        };
        if area != self.ime_area {
            self.ime_area = area;
            if let Some(r) = area {
                self.window.set_ime_cursor_area(LogicalPosition::new(r.x, r.y), LogicalSize::new(r.w, r.h));
            }
        }
        let busy = matches!(&self.screen, Screen::Chat(chat) if chat.is_busy());
        self.apply(actions);
        // One worker per frame's misses, decoding them in turn, so a page
        // of images never decodes dozens at once.
        let mut wanted = self.images.take_wanted();
        if let Some(last) = wanted.pop() {
            let proxy = self.proxy.clone();
            self.spawn(move |_, _| {
                for key in wanted {
                    let pixels = image::thumbnail(&key);
                    let _ = proxy.send_event(WorkerEvent::Thumbnail(key, pixels));
                }
                let pixels = image::thumbnail(&last);
                WorkerEvent::Thumbnail(last, pixels)
            });
        }
        if self.ui.animating || busy {
            self.window.request_redraw();
        }
    }

    /// How long the event loop may sleep: until the caret blinks next or the
    /// agent has something to do (a retry, a stream to check on).
    pub fn control_flow(&self) -> ControlFlow {
        let blink = self.ui.focused.then(|| self.last_frame + Duration::from_secs_f32(self.ui.next_blink().max(0.01)));
        let agent = match &self.screen {
            Screen::Chat(chat) => chat.next_deadline(),
            Screen::Login(_) => None,
        };
        match blink.into_iter().chain(agent).min() {
            Some(at) => ControlFlow::WaitUntil(at),
            None => ControlFlow::Wait,
        }
    }

    /// A timer fired: runs the agent's due work, even while the window is
    /// hidden and gets no frames, and redraws for the caret.
    pub fn wake(&mut self) {
        self.tick();
        self.window.request_redraw();
    }

    /// Sends due retries and drops silent streams.
    fn tick(&mut self) {
        let mut actions = Vec::new();
        if let Screen::Chat(chat) = &mut self.screen {
            chat.tick(Instant::now(), &mut actions);
        }
        self.apply(actions);
    }
}

/// A file write for the [`Writer`] thread.
enum Job {
    SaveSession(Session),
    DeleteSession(String),
    SaveConfig(Config),
    SaveProjects(Projects),
    /// List the saved sessions and send them back.
    ListSessions(mpsc::Sender<Vec<SessionSummary>>),
}

impl Job {
    /// Performs the job against `store` (`None`: sessions are not saved).
    fn run(self, store: Option<&mut SessionStore>) {
        let result = match (self, store) {
            (Self::SaveSession(session), Some(store)) => store.save(&session).map_err(|e| ("save the session", e)),
            (Self::DeleteSession(id), Some(store)) => store.delete(&id).map_err(|e| ("delete the session", e)),
            (Self::SaveSession(_) | Self::DeleteSession(_), None) => Ok(()),
            (Self::SaveConfig(config), _) => config.save().map_err(|e| ("save the config", e)),
            (Self::SaveProjects(projects), _) => projects.save().map_err(|e| ("save the projects", e)),
            (Self::ListSessions(reply), store) => {
                let sessions = match store.map(SessionStore::list) {
                    Some(Ok((sessions, errors))) => {
                        for e in errors {
                            eprintln!("serechat: skipping unreadable session: {e}");
                        }
                        sessions
                    }
                    Some(Err(e)) => {
                        eprintln!("serechat: cannot list sessions: {e}");
                        Vec::new()
                    }
                    None => Vec::new(),
                };
                // The receiver only disappears if the app is exiting.
                let _ = reply.send(sessions);
                Ok(())
            }
        };
        if let Err((what, e)) = result {
            eprintln!("serechat: could not {what}: {e}");
        }
    }
}

/// Owns the session store on a background thread and performs every file
/// write in the order it was queued, so the UI never waits on the disk.
/// Dropping it finishes the queue first, so nothing is lost on exit.
struct Writer {
    queue: Option<mpsc::Sender<Job>>,
    thread: Option<JoinHandle<()>>,
    /// Used on the calling thread if the writer thread could not start.
    inline: Option<SessionStore>,
}

impl Writer {
    /// Starts the thread for sessions in `dir` (`None`: not saved).
    fn start(dir: Option<PathBuf>) -> Self {
        let (queue, jobs) = mpsc::channel::<Job>();
        let fallback = dir.clone();
        let spawned = std::thread::Builder::new().name("serechat-writer".into()).spawn(move || {
            let mut store = dir.map(SessionStore::at);
            for job in jobs {
                job.run(store.as_mut());
            }
        });
        match spawned {
            Ok(thread) => Self { queue: Some(queue), thread: Some(thread), inline: None },
            Err(e) => {
                eprintln!("serechat: cannot start the writer thread, saving on the UI thread: {e}");
                Self { queue: None, thread: None, inline: fallback.map(SessionStore::at) }
            }
        }
    }

    /// Queues `job`.
    fn send(&mut self, job: Job) {
        let job = match &self.queue {
            Some(queue) => match queue.send(job) {
                Ok(()) => return,
                // The thread is gone; still don't lose the write.
                Err(mpsc::SendError(job)) => job,
            },
            None => job,
        };
        job.run(self.inline.as_mut());
    }

    /// The saved sessions, newest first. Waits for the writes queued before
    /// it, which only happens when the chat screen opens.
    fn list_sessions(&mut self) -> Vec<SessionSummary> {
        let (reply, sessions) = mpsc::channel();
        self.send(Job::ListSessions(reply));
        sessions.recv().unwrap_or_default()
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Closing the queue ends the thread once everything is written.
        self.queue = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The OS window decoration style matching `scheme`.
fn window_theme(scheme: Scheme) -> Theme {
    if scheme.is_light() { Theme::Light } else { Theme::Dark }
}

/// Gives the window the app icon for its title bar and taskbar entry.
///
/// Windows loads the sizes `build.rs` embedded, picked for the display's
/// scale; X11 gets the 256px PNG. macOS ignores window icons and Wayland
/// takes them from a `.desktop` file, so both keep the default.
fn set_icon(window: &Window) {
    #[cfg(windows)]
    {
        use winit::platform::windows::{IconExtWindows, WindowExtWindows};
        let scale = window.scale_factor();
        let load = |side: f64| {
            let side = (side * scale).round() as u32;
            winit::window::Icon::from_resource(1, Some(winit::dpi::PhysicalSize::new(side, side))).ok()
        };
        window.set_window_icon(load(16.0));
        window.set_taskbar_icon(load(32.0));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Ok((rgba, w, h)) = image::decode_png(include_bytes!("../assets/icon/256.png"))
        && let Ok(icon) = winit::window::Icon::from_rgba(rgba, w, h)
    {
        window.set_window_icon(Some(icon));
    }
    #[cfg(target_os = "macos")]
    let _ = window;
}

/// Minimal executor for wgpu's initialisation futures.
fn block_on<F: Future>(future: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Arc::new(Unpark(std::thread::current())).into();
    let mut cx = std::task::Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let std::task::Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_keeps_order_and_flushes_on_drop() {
        let dir = std::env::temp_dir().join(format!("serechat-writer-{}", std::process::id())).join("sessions");
        let session = |id: &str, updated| Session { id: id.into(), title: id.into(), updated, ..Session::default() };
        let mut writer = Writer::start(Some(dir.clone()));
        writer.send(Job::SaveSession(session("a", 1)));
        writer.send(Job::SaveSession(session("b", 2)));
        writer.send(Job::DeleteSession("a".into()));
        // Listing waits for the writes queued before it.
        let ids: Vec<String> = writer.list_sessions().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["b"]);
        writer.send(Job::SaveSession(session("c", 3)));
        drop(writer);
        assert!(dir.join("c.json").exists(), "dropping the writer finishes its queue");
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }
}
