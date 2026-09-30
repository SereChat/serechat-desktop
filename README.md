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
  and cost captions. Reasoning effort is selectable. While a thinking model works, the reply
  shows "Thinking for 4s"; afterwards "Thought for 12s" expands to its reasoning. Settings
  choose whether reasoning is hidden, collapsed or expanded.
- **Markdown replies**: headings, lists and task lists, quotes, tables, links, inline code, and
  code blocks with syntax highlighting and a copy button. Code never wraps: wide blocks
  scroll sideways (trackpad, Shift+wheel, or the scrollbar). Text can be selected across
  messages (drag, double-click for a word, triple-click for a paragraph) and copied.
- **Attachments**: drop files on the window or use the `+` button. Images go to vision models
  as images (PNG and JPEG show as thumbnails), PDFs as documents, and text files are inlined
  into the prompt.
- **Projects and the agent**: open a folder (Ctrl/Cmd+O, or drop it on the window) and chats in
  that project get tools to list, read, search and find files, and (with your approval) to
  write and edit files, run commands and fetch web pages. Every tool is confined to the
  project folder. Each chat has its own folder: pick or change it from the chip in the header. New chats start
  without one; the app reopens in the folder picked last.
- **Long tasks**: the agent keeps a plan (shown as a checklist) and works until it is done, up
  to 100 tool rounds before it pauses. Dropped connections, rate limits and server errors are
  retried automatically; when a conversation outgrows the model's context, the model
  summarises it and carries on from the summary (the full history stays visible). A run
  that stops early (an error, a pause, Esc or closing the app) shows a **Continue** button,
  and "Always allow" choices are remembered per session. Tool calls show as they are
  written (a file's content fills in live), and the line under the messages sums up the
  run's tool rounds and cost, failed attempts included.
- **Background processes**: the agent can start a dev server or watcher (with your approval),
  read its output later and stop it. Stopping ends the whole process tree; everything still
  running stops when its chat is deleted or the app exits.
- **Project instructions and skills**: a project's `AGENTS.md` goes with every request.
  [Agent Skills](https://agentskills.io) live in `.agents/skills/<name>/SKILL.md`: yours in
  your home folder work in every chat, even without a project; a project's work in its
  chats and take precedence over yours. The agent sees each skill's name and description
  and loads its instructions, and the files it bundles, only when a task needs them.
  Skills are found at startup and when a folder is picked, and looked at again when the
  window regains focus or the agent changes files, so edits apply without a restart.
  Settings lists the skills the open chat can use, and any problems reading them.
- **Reasoning**: the effort picker offers the levels the selected model supports.
- **Spotlight** (Ctrl/Cmd+K): one search over commands, projects, sessions, models, themes
  and the full text of every saved message.
- **Emoji and CJK**: colour emoji (Twemoji), and Chinese, Japanese and Korean text through the
  operating system's fonts, with input-method (IME) support for typing them.
- **Settings**, in tabs: Appearance (Dark, One Dark and Light schemes, reasoning display), Skills,
  Usage totals, and Account (data folder, sign out).

## Layout

| Crate             | Purpose                                                                          |
|-------------------|----------------------------------------------------------------------------------|
| `crates/serechat` | API client: sign-in, models, streaming Responses API with attachments and tools, config, sessions, projects |
| `crates/desktop`  | The app: winit window, wgpu renderer, text engine, immediate-mode UI, agent tools |

`crates/desktop/src`:

- `gpu.rs` + `shader.wgsl`: a single instanced pipeline. Rounded rectangles, borders,
  soft shadows, glyphs and images are all quads, drawn with one draw call per frame.
- `atlas.rs`: shelf packing for the glyph and image atlases, evicting the least recently
  drawn shelf when full.
- `image.rs`: thumbnails: PNG/JPEG decoding (with EXIF orientation) on worker threads,
  cover-cropping, and the image atlas.
- `font.rs` + `raster.rs`: font faces (via `ttf-parser`), fallback to system fonts for other
  scripts, GPOS kerning, GSUB emoji ligatures, COLR colour layers, and an exact-area
  anti-aliasing rasteriser.
- `text.rs`: rich text layout (runs of fonts, sizes, colours, decorations and links), line
  breaking including CJK, hit-testing, and the glyph atlas.
- `markdown.rs` → `doc.rs`: a streaming-friendly Markdown parser, and laid-out documents with
  selection, links and incremental rebuilds while a reply streams. `highlight.rs` colours code.
- `paint.rs`, `ui.rs`, `editor.rs`: drawing API, widgets and input state, text editing.
- `chat/`: the main screen (sidebar, composer, messages, menus); `chat/agent.rs` is the agent
  loop (tool rounds, retries, context compaction, Continue).
- `spotlight.rs`, `settings.rs`, `login.rs`: the other surfaces.
- `process.rs`: background processes the agent starts, reads and stops.
- `skills.rs`: Agent Skills scanning and loading, and the project's `AGENTS.md`;
  `chat/skills.rs` keeps the scanned catalogs and decides when to scan again.
- `tools.rs`, `attachments.rs`, `platform.rs`: agent tools, file attachments, OS integration
  (browser, file manager, native pickers).
- `theme.rs`: the colour schemes, sizes and text styles.
- `app.rs`: event routing, worker threads and the frame loop.

The app redraws only when something changes: input, network events, animations, or the
caret blink timer. Network calls, tools, imports, dialogs, searches and image decoding run on
worker threads and post results back to the event loop. Every file write goes through one
writer thread, in order, which finishes its queue before the app exits.

CI (`.github/workflows/ci.yml`) runs clippy and the tests on Windows, macOS and Linux. Pushing
a `v*` tag attaches release binaries to a GitHub release.

## Sign-in and storage

The first time the app starts, it runs SereChat's device-code flow. The browser opens the
approval page, and the user types the 6-digit code into the app. Everything lives in
`~/.serechat/`:

- `config.toml`: token, model, reasoning effort and display, colour scheme and current project.
- `sessions/<id>.json`: one file per conversation, with each reply's tokens and cost at the
  prices of the time, attachments and tool calls. `.index.json` next to them holds titles and
  totals, so startup reads only the index; a session's messages load when it is opened.
- `attachments/`: the app's copies of attached files (deleted with their session).
- `projects.json`: project folders.

On Unix these files have mode `0600`, and they are written atomically. Signing out (or a `401`
from the API) removes the token but keeps sessions.

## Known limits (deliberate, for now)

- No complex-script shaping (Arabic joining, Indic reordering) and no bidi.
- GIF and WebP attachments show as chips, not thumbnails.
- Native file and folder pickers run the platform's helper (PowerShell, `osascript`,
  `zenity`/`kdialog`), so they take a moment to appear.
- Release binaries are unsigned and not packaged as installers or a macOS `.app` yet.

## Fonts

- Inter, © The Inter Project Authors, SIL Open Font License (`assets/Inter-LICENSE.txt`).
- JetBrains Mono, © The JetBrains Mono Project Authors, SIL Open Font License
  (`assets/JetBrainsMono-LICENSE.txt`).
- Twemoji Mozilla: code Apache 2.0, graphics © Twitter, Inc. and other contributors, CC-BY 4.0
  (`assets/Twemoji-LICENSE.txt`).
