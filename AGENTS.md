# AGENTS.md

## What this is

SereChat Desktop is the native desktop app for [serechat.com](https://serechat.com). It gives people AI chat on their own desktop. It will grow into something more than "the website in a window": a multi-purpose agent. Today it has a sign-in screen, a chat screen (sidebar of saved sessions, streaming chat view, model and reasoning pickers) and a settings page (colour scheme, usage totals, data, account).

- API: [Responses API](https://serechat.com/docs/responses) (`POST /v1/responses`, SSE streaming).
- Auth: [device-code flow](https://serechat.com/docs/authentication). On first run the app opens the browser, the user approves, then types the 6-digit code into the app.
- Local data: everything lives in `~/.serechat/`: `config.toml` (token, model, reasoning effort, colour scheme) and `sessions/<id>.json` (one file per conversation, with per-reply token usage and cost).

See `README.md` for the crate layout and architecture.

## Hard requirements from the owner

- **Fully Rust-native, custom UI.** Use `winit` + `wgpu` and draw every pixel ourselves. No Tauri, no web views, and no UI frameworks (egui, iced, etc.).
- **Stunning, yet extremely fast.** Performance comes first. Redraw only when something changes, do minimal work per frame, and keep the single-pipeline, single-draw-call renderer.
- **Cross-platform:** it must work on Windows, macOS and Linux.
- **Dependencies are costly.** Add one only if it is truly required and can't reasonably be written by hand. No `thiserror`: write custom error types by hand (`Display` + `std::error::Error`). No `anyhow`, no async runtimes (`tokio`), no convenience crates. Disable default features where possible.
- **Always use the latest versions** of the dependencies we do use.
- **Cargo workspace:** `crates/serechat` is the API client with no GUI deps; `crates/desktop` is the app.
- **Military-grade code quality, properly documented.** Every public item has doc comments, `clippy::pedantic` stays clean (`cargo clippy --workspace --all-targets`), and there are no `unwrap`s on fallible runtime paths. Non-trivial logic gets a small unit test.
- **Secrets:** never log the token. Write config files atomically, with `0600` permissions on Unix.
- **Testing:** the owner tests the GUI themselves. Don't drive the app with computer use; `cargo build`, `clippy` and `test` are enough.

## Conventions

- Deliberate shortcuts are marked `ponytail:` with their limit and upgrade path. Current ones: all sessions load at startup and save on the UI thread, replies are plain text (no Markdown), there's no font fallback or IME, and the config file uses a flat, string-only TOML subset.
- Network calls run on worker threads and report back to the event loop via `WorkerEvent`. Never block the UI thread.
- Colours and sizes live in `crates/desktop/src/theme.rs`. Colours come from the active `Palette` (`p.theme`), never hard-coded, so all three schemes (Dark default, One Dark, Light) keep working. The look is Zed-inspired: flat surfaces, hairline borders, 4–6px radii, one accent, no gradients or glows.
