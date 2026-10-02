# AGENTS.md

## What this is

SereChat Desktop is the native desktop app for [serechat.com](https://serechat.com): AI chat on the user's own desktop, growing into a multi-purpose agent. Today it has sign-in, a chat screen (sessions sidebar, streaming Markdown replies, attachments, model and reasoning pickers), a coding agent with tools confined to a project folder (long runs with retries and compaction, background processes, the project's `AGENTS.md` and Agent Skills from `.agents/skills`, diffs of file changes), slash commands (`/new`, `/clear`, `/compact`, `/model`, `/init`), image/video/audio generation (`/image`, `/video`, `/audio`), Spotlight search (Ctrl/Cmd+K), and a settings page.

- API: [Responses API](https://serechat.com/docs/responses) (`POST /v1/responses`, SSE streaming); the server's source is in `../serechat` (`app/inference/api/llms/`). Images must use `image_url: {"url": "data:…"}`; tool calls arrive as `function_call` items; results go back as `function_call_output` items (consecutive calls merge into one assistant turn server-side). A stream ends with `response.completed`, `response.incomplete` (`incomplete_details.reason`, e.g. `max_output_tokens`: never run that reply's tool calls) or `response.failed` (`error.code`: retry `server_error` and `rate_limit_exceeded`, compact on `context_length_exceeded`). `: ping` comments arrive every 15 s. Usage includes `input_tokens_details.cached_tokens` / `cache_write_tokens`; `/v1/models` reports `context_window` and cache prices; `max_output_tokens` defaults to the model's maximum. Reasoning input items are ignored, and there is no server-side conversation state.
- Media API: [images](https://serechat.com/docs/images), [videos](https://serechat.com/docs/videos), [audio](https://serechat.com/docs/audio) (`app/inference/api/{images,videos,audio}/`, shared code in `mediaShared.ts`). `GET /v1/{images,videos,audio}` lists models with per-mode `schemas` (`required` params) and a `pricing` string; `POST …/generations` with `"async": true` returns `{id, sparks_used}` (1 Spark = $0.01); `GET …/generations/:id` returns `status` (`queued`/`processing`/`succeeded`/`failed`) and `output.url` (`/api/files/:id` on our origin, needs the token). Failed jobs are refunded. App tokens cannot upload input files (`/api/files/upload` is cookie-only) or cancel jobs (`/api/assets/:id/cancel`), so generation is prompt-only and runs to completion.
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
- **Agent safety:** tools resolve every path inside the project root (symlinks included). Writing, editing, running commands and fetching URLs need the user's approval in the chat. Links from replies only open `http(s)`/`mailto`. The one tool outside the project: `use_skill` (offered in every chat, project or not) reads files inside skill folders (`.agents/skills` in the project or home folder), with the same path and symlink checks. Background processes (`start_process`) need approval like commands and are stopped when their chat is deleted or the app exits.
- **Testing:** the owner tests the GUI themselves. Don't drive the app with computer use; `cargo build`, `clippy` and `test` are enough. CI (`.github/workflows/ci.yml`) runs clippy with `-D warnings` and the tests on Windows, macOS and Linux; a `v*` tag publishes release binaries.

## Conventions

- Deliberate shortcuts are marked `ponytail:` with their limit and upgrade path. Current ones: fallback fonts are read fully into memory; no complex-script shaping (Arabic, Indic) or bidi; grapheme clusters follow a practical subset of UAX #29; GIF and WebP attachments have no thumbnails; native pickers run helper processes; the config file uses a flat, string-only TOML subset.
- Every file the app writes (sessions, config, projects) goes through the `Writer` thread in `app.rs`, in order; reads run on workers. Never write files from the UI thread.
- Glyphs and image thumbnails live in two 2048² atlases packed by `atlas.rs`, which evicts the least recently drawn shelf when full. Images are drawn with `Painter::image`, which decodes on a worker on first use.
- Screens never touch disk or network: they push `Action`s; the app runs them (on worker threads when slow) and reports back via `WorkerEvent`. Never block the UI thread.
- Colours and sizes live in `crates/desktop/src/theme.rs`. Colours come from the active `Palette` (`p.theme`), never hard-coded, so all three schemes (Dark default, One Dark, Light) keep working. The look is Zed-inspired: flat surfaces, hairline borders, 4–6px radii, one accent, no gradients or glows.
- Text never assumes a font covers it: layouts go through `Fonts::resolve`, which falls back to Twemoji and the system's CJK fonts.
