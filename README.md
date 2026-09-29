# SereChat Desktop

A native, GPU-rendered desktop client and coding agent for [SereChat](https://serechat.com).
It runs on Windows, macOS and Linux, and it is pure Rust: no web view and no UI framework.

```sh
cargo run --release -p serechat-desktop     # binary name: serechat
cargo test --workspace
cargo clippy --workspace --all-targets
```

## What it does

- **Chat** with any SereChat model, with prices shown in the model picker and per-reply token
  and cost captions. Reasoning effort is selectable; a model's reasoning is shown on request.
- **Markdown replies**: headings, lists and task lists, quotes, tables, links, inline code, and
  code blocks with syntax highlighting and a copy button. Text can be selected across
  messages (drag, double-click for a word, triple-click for a paragraph) and copied.
- **Attachments**: drop files on the window or use the `+` button. Images go to vision models
  as images, PDFs as documents, and text files are inlined into the prompt.
- **Projects and the agent**: open a folder (Ctrl/Cmd+O, or drop it on the window) and chats in
  that project get tools to list, read, search and find files, and (with your approval) to
  write and edit files, run commands and fetch web pages. Every tool is confined to the
  project folder. The header shows the folder a chat works in.
- **Spotlight** (Ctrl/Cmd+K): one search over commands, projects, sessions, models, themes
  and the full text of every saved message.
- **Emoji and CJK**: colour emoji (Twemoji), and Chinese, Japanese and Korean text through the
  operating system's fonts, with input-method (IME) support for typing them.
- **Settings**: Dark, One Dark and Light schemes, usage totals, data folder and account.

## Layout

| Crate             | Purpose                                                                          |
|-------------------|----------------------------------------------------------------------------------|
| `crates/serechat` | API client: sign-in, models, streaming Responses API with attachments and tools, config, sessions, projects |
| `crates/desktop`  | The app: winit window, wgpu renderer, text engine, immediate-mode UI, agent tools |

`crates/desktop/src`:

- `gpu.rs` + `shader.wgsl`: a single instanced pipeline. Rounded rectangles, borders,
  soft shadows and glyphs are all SDF quads, drawn with one draw call per frame.
- `font.rs` + `raster.rs`: font faces (via `ttf-parser`), fallback to system fonts for other
  scripts, GPOS kerning, GSUB emoji ligatures, COLR colour layers, and an exact-area
  anti-aliasing rasteriser.
- `text.rs`: rich text layout (runs of fonts, sizes, colours, decorations and links), line
  breaking including CJK, hit-testing, and the glyph atlas.
- `markdown.rs` → `doc.rs`: a streaming-friendly Markdown parser, and laid-out documents with
  selection, links and incremental rebuilds while a reply streams. `highlight.rs` colours code.
- `paint.rs`, `ui.rs`, `editor.rs`: drawing API, widgets and input state, text editing.
- `chat/`: the main screen (sidebar, composer, messages, menus) and the agent loop.
- `spotlight.rs`, `settings.rs`, `login.rs`: the other surfaces.
- `tools.rs`, `attachments.rs`, `platform.rs`: agent tools, file attachments, OS integration
  (browser, file manager, native pickers).
- `theme.rs`: the colour schemes, sizes and text styles.
- `app.rs`: event routing, worker threads and the frame loop.

The app redraws only when something changes: input, network events, animations, or the
caret blink timer. Network calls, tools, imports, dialogs and searches run on worker threads
and post results back to the event loop.

## Sign-in and storage

The first time the app starts, it runs SereChat's device-code flow. The browser opens the
approval page, and the user types the 6-digit code into the app. Everything lives in
`~/.serechat/`:

- `config.toml`: token, model, reasoning effort, colour scheme and current project.
- `sessions/<id>.json`: one file per conversation, with each reply's tokens and cost at the
  prices of the time, attachments and tool calls. `.index.json` next to them holds titles and
  totals, so startup reads only the index; a session's messages load when it is opened.
- `attachments/`: the app's copies of attached files (deleted with their session).
- `projects.json`: project folders.

On Unix these files have mode `0600`, and they are written atomically. Signing out (or a `401`
from the API) removes the token but keeps sessions.

## Known limits (deliberate, for now)

- No complex-script shaping (Arabic joining, Indic reordering) and no bidi.
- Attached images show as chips, not thumbnails.
- Native file and folder pickers run the platform's helper (PowerShell, `osascript`,
  `zenity`/`kdialog`), so they take a moment to appear.
- Code blocks wrap long lines instead of scrolling sideways.

## Fonts

- Inter, © The Inter Project Authors, SIL Open Font License (`assets/Inter-LICENSE.txt`).
- JetBrains Mono, © The JetBrains Mono Project Authors, SIL Open Font License
  (`assets/JetBrainsMono-LICENSE.txt`).
- Twemoji Mozilla: code Apache 2.0, graphics © Twitter, Inc. and other contributors, CC-BY 4.0
  (`assets/Twemoji-LICENSE.txt`).
