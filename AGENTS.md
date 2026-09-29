# AGENTS.md

## What this is

SereChat Desktop is the native desktop app for [serechat.com](https://serechat.com): AI chat on the user's own desktop, growing into a multi-purpose agent. Today it has sign-in, a chat screen (sessions sidebar grouped by project, streaming Markdown replies, attachments, model and reasoning pickers), a coding agent with tools confined to a project folder, Spotlight search (Ctrl/Cmd+K), and a settings page.

- API: [Responses API](https://serechat.com/docs/responses) (`POST /v1/responses`, SSE streaming). Verified against the live API: event names arrive only in the SSE `event:` field; images must use `image_url: {"url": "data:…"}`; tool calls arrive as `function_call` items in `response.completed`; results go back as `function_call_output` items.
- Auth: [device-code flow](https://serechat.com/docs/authentication). On first run the app opens the browser, the user approves, then types the 6-digit code into the app.
- Local data: everything lives in `~/.serechat/`: `config.toml`, `sessions/<id>.json` + `sessions/.index.json`, `attachments/`, `projects.json`. See `README.md`.

See `README.md` for features, the crate layout and the architecture.

## Hard requirements from the owner

- **Fully Rust-native, custom UI.** Use `winit` + `wgpu` and draw every pixel ourselves. No Tauri, no web views, and no UI frameworks (egui, iced, etc.).
- **Stunning, yet extremely fast.** Performance comes first. Redraw only when something changes, do minimal work per frame, and keep the single-pipeline, single-draw-call renderer.
- **Cross-platform:** it must work on Windows, macOS and Linux.
- **Dependencies are costly.** Add one only if it is truly required and can't reasonably be written by hand. No `thiserror`: write custom error types by hand (`Display` + `std::error::Error`). No `anyhow`, no async runtimes (`tokio`), no convenience crates. Disable default features where possible. OS features go through the tools the platform ships with (see `platform.rs`) rather than new crates.
- **Always use the latest versions** of the dependencies we do use.
- **Cargo workspace:** `crates/serechat` is the API client with no GUI deps; `crates/desktop` is the app.
- **Military-grade code quality, properly documented.** Every public item has doc comments, `clippy::pedantic` stays clean (`cargo clippy --workspace --all-targets`), and there are no `unwrap`s on fallible runtime paths. Non-trivial logic gets a small unit test. Parsers must survive hostile input (bounded recursion, no slicing off char boundaries).
- **Secrets:** never log the token. Write config files atomically, with `0600` permissions on Unix.
- **Agent safety:** tools resolve every path inside the project root (symlinks included). Writing, editing, running commands and fetching URLs need the user's approval in the chat. Links from replies only open `http(s)`/`mailto`.
- **Testing:** the owner tests the GUI themselves. Don't drive the app with computer use; `cargo build`, `clippy` and `test` are enough.

## Conventions

- Deliberate shortcuts are marked `ponytail:` with their limit and upgrade path. Current ones: session files and their index are written on the UI thread; fallback fonts are read fully into memory; no complex-script shaping (Arabic, Indic) or bidi; grapheme clusters follow a practical subset of UAX #29; image attachments have no thumbnails; native pickers run helper processes; the config file uses a flat, string-only TOML subset; the glyph atlas is wiped when full.
- Screens never touch disk or network: they push `Action`s; the app runs them (on worker threads when slow) and reports back via `WorkerEvent`. Never block the UI thread.
- Colours and sizes live in `crates/desktop/src/theme.rs`. Colours come from the active `Palette` (`p.theme`), never hard-coded, so all three schemes (Dark default, One Dark, Light) keep working. The look is Zed-inspired: flat surfaces, hairline borders, 4–6px radii, one accent, no gradients or glows.
- Text never assumes a font covers it: layouts go through `Fonts::resolve`, which falls back to Twemoji and the system's CJK fonts.
