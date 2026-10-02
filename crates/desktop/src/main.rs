//! SereChat for the desktop: a native, GPU-rendered AI chat client.
//!
//! No web view and no UI framework: winit provides the window, wgpu draws
//! every pixel through a single instanced SDF pipeline (`gpu.rs`,
//! `shader.wgsl`), and the UI is immediate mode (`ui.rs`, `login.rs`,
//! `chat.rs`, `settings.rs`), redrawn only when something changes.

// Release builds are GUI apps on Windows: no console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod atlas;
mod attachments;
mod chat;
mod diff;
mod doc;
mod editor;
mod font;
mod gpu;
mod highlight;
mod image;
mod login;
mod markdown;
mod paint;
mod platform;
mod process;
mod raster;
mod settings;
mod skills;
mod spotlight;
mod text;
mod theme;
mod tools;
mod ui;

use std::process::ExitCode;

use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::WindowId;

use app::{App, WorkerEvent};

/// Bridges winit callbacks to the [`App`], which can only be created once
/// the event loop is running.
struct Handler {
    app: Option<App>,
    proxy: EventLoopProxy<WorkerEvent>,
    error: Option<app::StartupError>,
}

impl ApplicationHandler<WorkerEvent> for Handler {
    fn new_events(&mut self, _: &ActiveEventLoop, cause: StartCause) {
        if let (StartCause::ResumeTimeReached { .. }, Some(app)) = (cause, &mut self.app) {
            app.wake();
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.app.is_some() {
            return;
        }
        match App::new(event_loop, self.proxy.clone()) {
            Ok(app) => self.app = Some(app),
            Err(e) => {
                self.error = Some(e);
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, _: &ActiveEventLoop, event: WorkerEvent) {
        if let Some(app) = &mut self.app {
            app.worker_event(event);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if let Some(app) = &mut self.app {
            app.window_event(event_loop, event);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(app) = &self.app {
            event_loop.set_control_flow(app.control_flow());
        }
    }
}

fn main() -> ExitCode {
    let event_loop = match EventLoop::<WorkerEvent>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(e) => {
            eprintln!("serechat: cannot start the event loop: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut handler = Handler { app: None, proxy: event_loop.create_proxy(), error: None };
    let result = event_loop.run_app(&mut handler);
    // Dev servers and watchers the agent started must not outlive the app.
    process::stop_all();
    if let Err(e) = result {
        eprintln!("serechat: event loop failed: {e}");
        return ExitCode::FAILURE;
    }
    if let Some(e) = handler.error {
        eprintln!("serechat: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
