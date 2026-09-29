//! Application state, event routing and background work.
//!
//! Threading model: the winit thread owns all state and renders. Network
//! calls, tools, file imports, dialogs and searches run on short-lived
//! worker threads that report back through [`WorkerEvent`]s posted to the
//! event loop, so the UI never blocks.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arboard::Clipboard;
use serechat::{
    AccessToken, Attachment, Client, Config, Error, Model, Projects, ResponseRequest, SearchHit, Session, SessionStore, StreamEvent,
    ToolSpec,
};
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::window::{CursorIcon, Theme, Window};

use crate::chat::{self, Chat, Reasoning, SendJob, ToolJob};
use crate::gpu::{GpuError, Instance, Renderer};
use crate::login::Login;
use crate::paint::{Painter, Rect};
use crate::text::{Fonts, GlyphAtlas};
use crate::theme::{Palette, Scheme};
use crate::ui::{Ui, copy};
use crate::{attachments, platform, tools};

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
    /// Persist a model choice.
    SelectModel(String),
    /// Persist a reasoning effort.
    SetReasoning(Reasoning),
    /// Switch and persist the colour scheme.
    SetTheme(Scheme),
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
    instances: Vec<Instance>,
    proxy: EventLoopProxy<WorkerEvent>,
    clipboard: Option<Clipboard>,
    ui: Ui,
    start: Instant,
    last_frame: Instant,
    config: Config,
    client: Client,
    /// Saved sessions; `None` when there is no home directory.
    store: Option<SessionStore>,
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
        let window = Arc::new(event_loop.create_window(attributes).map_err(StartupError::Window)?);
        // Chinese, Japanese and Korean input methods deliver text through IME events.
        window.set_ime_allowed(true);
        let renderer = block_on(Renderer::new(Arc::clone(&window), event_loop.owned_display_handle())).map_err(StartupError::Gpu)?;

        let client = Client::new(config.token.clone());
        let store = SessionStore::open().inspect_err(|e| eprintln!("serechat: sessions will not be saved: {e}")).ok();
        let projects = Projects::load().inspect_err(|e| eprintln!("serechat: cannot read projects: {e}")).ok();
        let now = Instant::now();
        let mut app = Self {
            window,
            renderer,
            fonts: Fonts::load(),
            atlas: GlyphAtlas::default(),
            instances: Vec::new(),
            proxy,
            clipboard: None,
            ui: Ui::default(),
            start: now,
            last_frame: now,
            screen: Screen::Login(Login::new(None)),
            config,
            client,
            store,
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
        let sessions = match self.store.as_mut().map(SessionStore::list) {
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
        let reasoning = Reasoning::from_key(self.config.reasoning.as_deref());
        let projects = self.projects.as_ref().map(|p| p.list.clone()).unwrap_or_default();
        self.screen = Screen::Chat(Box::new(Chat::new(self.config.model.clone(), reasoning, sessions, projects, self.config.project.clone())));
        self.spawn(|client, _| WorkerEvent::Models(client.models()));
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

    fn save_config(&self) {
        if let Err(e) = self.config.save() {
            eprintln!("serechat: could not save config: {e}");
        }
    }

    fn save_projects(&self) {
        if let Some(Err(e)) = self.projects.as_ref().map(Projects::save) {
            eprintln!("serechat: could not save projects: {e}");
        }
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
            WindowEvent::Focused(focused) => self.ui.focused = focused,
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
                self.ui.scroll -= match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * 48.0,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / scale,
                };
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
            (WorkerEvent::SessionLoaded { conversation, result }, Screen::Chat(chat)) => chat.session_loaded(conversation, result),
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
            (WorkerEvent::ToolDone { conversation, call_id, result }, Screen::Chat(chat)) => {
                chat.tool_done(conversation, &call_id, result, &mut actions);
            }
            (WorkerEvent::Imported(results), Screen::Chat(chat)) => chat.attachments_imported(results),
            (WorkerEvent::FilesPicked(paths), Screen::Chat(chat)) => chat.attach(paths, &mut actions),
            (WorkerEvent::FolderPicked(Some(path)), Screen::Chat(_)) => actions.push(Action::OpenProject(Some(path))),
            (WorkerEvent::SearchResults { generation, hits }, Screen::Chat(chat)) => chat.search_results(generation, hits),
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
                        let specs: Vec<ToolSpec<'_>> = if job.tools {
                            tools::all().iter().map(|t| ToolSpec { name: t.name, description: &t.description, parameters: &t.parameters }).collect()
                        } else {
                            Vec::new()
                        };
                        let request = ResponseRequest {
                            model: &job.model,
                            instructions: Some(&job.instructions),
                            reasoning: job.reasoning,
                            input: &input,
                            tools: &specs,
                        };
                        client.stream_response(&request, &job.cancel, |event| {
                            let _ = proxy.send_event(WorkerEvent::Stream { conversation, stream, event });
                        })
                    }
                };
                WorkerEvent::StreamEnded { conversation, stream, result }
            }),
            Action::RunTool(job) => self.spawn(move |client, _| {
                let result = tools::run(&job.root, client, &job.call, &job.cancel);
                WorkerEvent::ToolDone { conversation: job.conversation, call_id: job.call.call_id, result }
            }),
            Action::SelectModel(model) => {
                self.config.model = Some(model);
                self.save_config();
            }
            Action::SetReasoning(reasoning) => {
                self.config.reasoning = Some(reasoning.key().to_owned());
                self.save_config();
            }
            Action::SetTheme(scheme) => self.set_scheme(scheme),
            Action::LoadSession { conversation, session } => {
                if let Some(store) = &self.store {
                    let store = SessionStore::at(store.dir().to_owned());
                    self.spawn(move |_, _| WorkerEvent::SessionLoaded { conversation, result: store.load(&session) });
                }
            }
            // ponytail: session files and the index are written on the UI
            // thread; they are small and only saved when a turn starts or
            // ends. Move to a writer thread if large histories stutter.
            Action::SaveSession(session) => {
                if let Some(Err(e)) = self.store.as_mut().map(|store| store.save(&session)) {
                    eprintln!("serechat: could not save session: {e}");
                }
            }
            Action::DeleteSession(id) => {
                if let Some(Err(e)) = self.store.as_mut().map(|store| store.delete(&id)) {
                    eprintln!("serechat: could not delete session: {e}");
                }
            }
            Action::Search { query, generation } => {
                if let Some(store) = &self.store {
                    let store = SessionStore::at(store.dir().to_owned());
                    self.spawn(move |_, _| WorkerEvent::SearchResults { generation, hits: store.search(&query, 30).unwrap_or_default() });
                }
            }
            Action::PickFiles => self.spawn(|_, _| WorkerEvent::FilesPicked(platform::pick_files().unwrap_or_default())),
            Action::ImportFiles(paths) => {
                let dir = self.store.as_ref().map_or_else(|| std::env::temp_dir().join("serechat-attachments"), SessionStore::attachments_dir);
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
                if let Some(store) = &self.store {
                    // The folder may not exist before the first save.
                    let _ = std::fs::create_dir_all(store.dir());
                    if let Err(e) = platform::open_folder(store.dir()) {
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
            let mut painter = Painter::new(&self.fonts, &mut self.atlas, &mut self.instances, scale, view, &self.palette);
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

        self.renderer.render(&self.instances, &mut self.atlas.uploads, scale, self.palette.bg);
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
        if self.ui.animating || busy {
            self.window.request_redraw();
        }
    }

    /// How long the event loop may sleep: until the caret blinks next.
    pub fn control_flow(&self) -> ControlFlow {
        if self.ui.focused {
            let wait = Duration::from_secs_f32(self.ui.next_blink().max(0.01));
            ControlFlow::WaitUntil(self.last_frame + wait)
        } else {
            ControlFlow::Wait
        }
    }

    /// Requests a redraw (used when a blink timer fires).
    pub fn redraw(&self) {
        self.window.request_redraw();
    }
}

/// The OS window decoration style matching `scheme`.
fn window_theme(scheme: Scheme) -> Theme {
    if scheme.is_light() { Theme::Light } else { Theme::Dark }
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
