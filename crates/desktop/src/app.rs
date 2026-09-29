//! Application state, event routing and background work.
//!
//! Threading model: the winit thread owns all state and renders. Network
//! calls run on short-lived worker threads that report back through
//! [`WorkerEvent`]s posted to the event loop, so the UI never blocks.

use std::fmt;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arboard::Clipboard;
use serechat::{AccessToken, Client, Config, Error, Model, ResponseRequest, Session, SessionStore, StreamEvent};
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::window::{CursorIcon, Theme, Window};

use crate::chat::{Chat, Reasoning, SendJob};
use crate::gpu::{GpuError, Instance, Renderer};
use crate::login::Login;
use crate::paint::{Painter, Rect};
use crate::text::{Fonts, GlyphAtlas};
use crate::theme::{Palette, Scheme};
use crate::ui::{Ui, copy};

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
    /// Persist a model choice.
    SelectModel(String),
    /// Persist a reasoning effort.
    SetReasoning(Reasoning),
    /// Switch and persist the colour scheme.
    SetTheme(Scheme),
    /// Write a session to disk.
    SaveSession(Session),
    /// Delete a session file by id.
    DeleteSession(String),
    /// Show `~/.serechat/sessions` in the file manager.
    OpenDataDir,
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
    Chat(Chat),
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
        let renderer = block_on(Renderer::new(Arc::clone(&window), event_loop.owned_display_handle())).map_err(StartupError::Gpu)?;

        let client = Client::new(config.token.clone());
        let store = SessionStore::open().inspect_err(|e| eprintln!("serechat: sessions will not be saved: {e}")).ok();
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
            scheme,
            palette: *scheme.palette(),
            fade: None,
            shown: false,
            cursor: CursorIcon::Default,
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
        // ponytail: every session is read at startup on the UI thread; load
        // titles first and bodies on demand if histories grow large.
        let loaded = self.store.as_ref().map(SessionStore::load_all).transpose().unwrap_or_else(|e| {
            eprintln!("serechat: cannot list sessions: {e}");
            None
        });
        let sessions = loaded
            .into_iter()
            .flatten()
            .filter_map(|session| session.inspect_err(|e| eprintln!("serechat: skipping unreadable session: {e}")).ok())
            .collect();
        let reasoning = Reasoning::from_key(self.config.reasoning.as_deref());
        self.screen = Screen::Chat(Chat::new(self.config.model.clone(), reasoning, sessions));
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
                self.ui.down = state == ElementState::Pressed;
                if self.ui.down {
                    self.ui.pressed = true;
                    self.ui.press_pos = self.ui.mouse;
                } else {
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
                let mut actions = Vec::new();
                match &mut self.screen {
                    Screen::Login(login) => login.key(&event, self.ui.mods, &mut self.clipboard, &mut actions),
                    Screen::Chat(chat) => chat.key(&event, self.ui.mods, &mut self.clipboard, &mut actions),
                }
                self.ui.last_edit = self.ui.time;
                self.apply(actions);
            }
            _ => return,
        }
        self.window.request_redraw();
    }

    /// Applies a result from a worker thread.
    pub fn worker_event(&mut self, event: WorkerEvent) {
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
            (WorkerEvent::Stream { conversation, stream, event }, Screen::Chat(chat)) => {
                chat.stream_event(conversation, stream, event);
            }
            (WorkerEvent::StreamEnded { conversation, stream, result }, Screen::Chat(chat)) => {
                let mut actions = Vec::new();
                let expired = chat.stream_end(conversation, stream, result, &mut actions);
                self.apply(actions);
                if expired {
                    self.sign_out(Some("Your session has expired. Please sign in again.".into()));
                }
            }
            // Results for a screen that is no longer shown.
            _ => return,
        }
        self.window.request_redraw();
    }

    fn apply(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::StartLogin => {
                    self.spawn(|client, _| WorkerEvent::AuthRequested(client.request_authorization(APP_NAME)));
                }
                Action::OpenAuthPage(request_id) => self.open_auth_page(&request_id),
                Action::VerifyCode { request_id, code } => {
                    self.spawn(move |client, _| WorkerEvent::AuthExchanged(client.exchange_code(&request_id, &code)));
                }
                Action::Send(job) => self.spawn(move |client, proxy| {
                    let request =
                        ResponseRequest { model: &job.model, instructions: None, reasoning: job.reasoning, input: &job.input };
                    let (conversation, stream) = (job.conversation, job.stream);
                    let result = client.stream_response(&request, &job.cancel, |event| {
                        let _ = proxy.send_event(WorkerEvent::Stream { conversation, stream, event });
                    });
                    WorkerEvent::StreamEnded { conversation, stream, result }
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
                // ponytail: session files are written on the UI thread; they
                // are small and only saved when a turn starts or ends.
                Action::SaveSession(session) => {
                    if let Some(Err(e)) = self.store.as_ref().map(|store| store.save(&session)) {
                        eprintln!("serechat: could not save session: {e}");
                    }
                }
                Action::DeleteSession(id) => {
                    if let Some(Err(e)) = self.store.as_ref().map(|store| store.delete(&id)) {
                        eprintln!("serechat: could not delete session: {e}");
                    }
                }
                Action::OpenDataDir => {
                    if let Some(store) = &self.store {
                        // The folder may not exist before the first save.
                        let _ = std::fs::create_dir_all(store.dir());
                        if let Err(e) = open_folder(store.dir()) {
                            eprintln!("serechat: cannot open the data folder: {e}");
                        }
                    }
                }
                Action::Copy(text) => copy(&mut self.clipboard, &text),
                Action::SignOut => self.sign_out(None),
            }
        }
    }

    fn open_auth_page(&mut self, request_id: &str) {
        let url = self.client.authorize_url(request_id);
        if let Err(e) = open_url(&url) {
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
            let mut painter = Painter::new(&self.fonts, &mut self.atlas, &mut self.instances, scale, view, self.palette);
            match &mut self.screen {
                Screen::Login(login) => login.draw(&mut painter, &mut self.ui, view, &mut actions),
                Screen::Chat(chat) => chat.draw(&mut painter, &mut self.ui, view, self.scheme, &mut actions),
            }
            if !painter.atlas_full {
                break;
            }
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
        let streaming = matches!(&self.screen, Screen::Chat(chat) if chat.is_streaming());
        self.apply(actions);
        if self.ui.animating || streaming {
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

/// Opens `url` in the default browser without going through a shell.
fn open_url(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    // Reap the launcher so it does not linger as a zombie.
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Shows `path` in the platform's file manager.
fn open_folder(path: &std::path::Path) -> std::io::Result<()> {
    if cfg!(target_os = "windows") {
        let mut child = Command::new("explorer").arg(path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
        std::thread::spawn(move || child.wait());
        Ok(())
    } else {
        // `open` and `xdg-open` handle folders like URLs.
        open_url(&path.to_string_lossy())
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
