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
  into the prompt. Video and audio files (up to 100 MB) are for generations.
- **Projects and the agent**: open a folder (Ctrl/Cmd+O, or drop it on the window) and chats in
  that project get tools to list, read, search and find files, and (with your approval) to
  write and edit files, run commands and fetch web pages. Every tool is confined to the
  project folder. Each chat has its own folder: pick or change it from the chip in the header. New chats start
  without one; the app reopens in the folder picked last.
- **Diffs**: a file edit or write waiting for approval shows as a diff against the file on
  disk, with line numbers, three lines of context, the changed words picked out, and its
  `+added −removed` count in the card's header. Long diffs fold after 16 rows ("Show all").
  A finished change opens to the same diff, and hovering it shows **Revert**, which puts the
  file back as it was before (or deletes the file the change created, and the folders it made
  for it) and tells the model, through the conversation's summary if the change came before it.
  A file changed again since is left alone: revert the later changes first. `/undo` reverts
  the latest change; `/undo 3` the last three.
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
- **Browser**: in every chat, project or not, the agent can drive a browser to open pages,
  click and type (fetching a URL works everywhere too; files and commands need a project). It uses your installed Chrome, Brave, Helium, Edge or Chromium (Auto, or the one picked in Settings) in a window of its own,
  with a fresh throwaway profile, so none of your logins are involved. Each chat gets a
  tab. Pages reach the model as text outlines with numbered links, buttons and fields.
  Opening a page, clicking and typing need your approval; reading the page and taking
  screenshots don't. Screenshots show in the tool card, and only the latest three go
  with each request. The browser quits when the app exits.
- **Project instructions and skills**: a project's `AGENTS.md` goes with every request.
  [Agent Skills](https://agentskills.io) live in `.agents/skills/<name>/SKILL.md`: yours in
  your home folder work in every chat, even without a project; a project's work in its
  chats and take precedence over yours. The agent sees each skill's name and description
  and loads its instructions, and the files it bundles, only when a task needs them.
  Skills are found at startup and when a folder is picked, and looked at again when the
  window regains focus or the agent changes files, so edits apply without a restart.
  Settings lists the skills the open chat can use, and any problems reading them.
- **MCP servers**: tools from [Model Context Protocol](https://modelcontextprotocol.io) servers
  work in every chat, project or not, as `mcp__<server>__<tool>`. Add servers in Settings → MCP
  (a command such as `npx -y @modelcontextprotocol/server-memory`, or a URL), import the
  `mcpServers` JSON from a server's README straight from the clipboard, or edit
  `~/.serechat/mcp.json`, which takes the same shape as Claude's, Cursor's or VS Code's. The
  app speaks the current stateless protocol (2026-07-28) and the handshake-based ones before it,
  over stdio, Streamable HTTP and the old HTTP+SSE transport. Servers on the web that need an
  account sign in through the browser (OAuth 2.1 with PKCE, discovery and dynamic client
  registration); tokens refresh by themselves. Tools a server marks read-only run at once; the
  rest ask first, like the agent's own. Images they return reach the model.
- **Reasoning**: the effort picker offers the levels the selected model supports.
- **Edit and retry**: hovering a prompt shows **Edit**, which loads it into the composer (Esc
  cancels and brings back what you were typing); sending replaces it and every message after
  it. **Retry** under the last reply asks again, with the model now selected, or makes a
  generation again. What the replaced replies cost stays in the totals.
- **Find in chat** (Ctrl/Cmd+F): highlights every match in the open chat; Enter and Shift+Enter
  (or F3) step through them.
- **Updates**: the app checks GitHub releases at startup and every six hours, downloads the
  new version in the background (its size and SHA-256 checked), and runs it from the next start,
  or at once with "Restart to update". Settings can turn installing off. Development builds and
  copies in a build folder never update themselves.
- **Spotlight** (Ctrl/Cmd+K): one search over commands, projects, sessions, models, themes
  and the full text of every saved message.
- **Slash commands** in the composer, completed as you type (Tab completes, Enter runs, Esc
  dismisses): `/new` starts a chat in the same folder, `/clear` deletes the open chat and
  starts an empty one there, `/compact` summarises the chat to free up context, `/model`
  opens the model menu, `/init` asks the agent to write the project's `AGENTS.md`, and `/undo`
  reverts the agent's last file change (`/undo 3`: the last three).
- **Image, video and audio generation**: `/image`, `/video` and `/audio` followed by a prompt.
  While the composer holds one, the model button picks that kind's model (with its price).
  The chat stays usable while it runs; images show inline at their own aspect ratio, videos
  and audio as cards that open in the system player. The job lives on the server, so a
  generation still running when the app closes is picked up when its chat is opened again.
  Attach images, video or audio to work from them (edit a picture, animate it, remix a clip):
  the app uploads them and picks the model's mode that takes those files; `/image` alone with
  a file attached works from the file. A job still queued has a **Cancel** button, and is
  refunded.
- **Screen readers** (Windows and macOS): the window describes itself through AccessKit:
  messages, buttons, menus, fields and switches, with the focused field and clicks from the
  screen reader. Nothing is collected while no screen reader is running.
- **Emoji and CJK**: colour emoji (Twemoji), and Chinese, Japanese and Korean text through the
  operating system's fonts, with input-method (IME) support for typing them.
- **Settings**, in tabs: General (Dark, One Dark and Light schemes, reasoning display, the agent's browser), Skills,
  MCP servers, Usage (the account's Sparks and model allowance, and totals), and Account
  (updates, data folder, sign out). The media model menu shows the Sparks left too.

## Layout

| Crate             | Purpose                                                                          |
|-------------------|----------------------------------------------------------------------------------|
| `crates/serechat` | API client: OAuth tokens (exchange, refresh, revoke), models, streaming Responses API with attachments and tools, image/video/audio generation jobs, config, sessions, projects |
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
  loop (tool rounds, retries, context compaction, Continue); `chat/find.rs` the find bar.
- `mcp/`: MCP servers. `transport.rs` speaks JSON-RPC over stdio, Streamable HTTP and HTTP+SSE
  in both protocol eras, `oauth.rs` signs in, `config.rs` reads `mcp.json`, and `mod.rs` keeps
  the connections and offers their tools.
- `update.rs`: checks GitHub releases and swaps in new versions; `sha256.rs` for PKCE and
  checking downloads.
- `spotlight.rs`, `settings.rs`, `login.rs`: the other surfaces (`login.rs` also runs the
  browser sign-in). `keychain.rs` keeps the refresh token in the OS credential store.
- `process.rs`: background processes the agent starts, reads and stops.
- `browser.rs`, `browser.js`, `websocket.rs`: the agent's browser, driven over the
  DevTools protocol through a minimal WebSocket client; `browser.js` outlines a page.
- `skills.rs`: Agent Skills scanning and loading, and the project's `AGENTS.md`;
  `chat/skills.rs` keeps the scanned catalogs and decides when to scan again.
- `tools.rs`, `attachments.rs`, `platform.rs`: agent tools, file attachments (and downloads
  of generated files), OS integration (browser, file manager, native pickers).
- `diff.rs`: line diffs of the agent's file changes.
- `theme.rs`: the colour schemes, sizes and text styles.
- `a11y.rs`: screen readers. While one is connected, widgets describe themselves to `Ui` as
  they draw, and each frame's list becomes the AccessKit tree.
- `app.rs`: event routing, worker threads and the frame loop.

The app redraws only when something changes: input, network events, animations, or the
caret blink timer. Network calls, tools, imports, dialogs, searches and image decoding run on
worker threads and post results back to the event loop. Every file write goes through one
writer thread, in order, which finishes its queue before the app exits.

CI (`.github/workflows/ci.yml`) runs clippy and the tests on Windows, macOS and Linux. Pushing
a `v*` tag attaches release builds to a GitHub release: `serechat.exe` (icon embedded), `SereChat.app`
(`com.luvarly.serechat`) for macOS, and for Linux the binary with `serechat.desktop`, its icon and
`install.sh` (installs all three for the current user). The packaging files are in `packaging/`.

## Sign-in and storage

The first time the app starts, it signs in with OAuth 2.1: the browser opens SereChat's consent
page, and its answer comes back to the app on `127.0.0.1` (authorization code with PKCE). The
app asks for the `chat`, `media`, `files` and `account` (balances) scopes. Access tokens last an hour and are refreshed
as needed; the refresh token, which changes on every refresh, is kept in the OS keychain
(Credential Manager on Windows, the login keychain on macOS, the Secret Service through
`secret-tool` on Linux, or `~/.serechat/serechat-desktop.token` with mode `0600` where there is
none). Everything else lives in `~/.serechat/`:

- `config.toml`: model, reasoning effort and display, colour scheme, current project, and
  the image, video and audio models, and the window's size and whether it was maximized.
- `sessions/<id>.json`: one file per conversation, with each reply's tokens and cost at the
  prices of the time, attachments and tool calls (with each file the agent changed as it was
  before, to revert the change). `.index.json` next to them holds titles and
  totals, so startup reads only the index; a session's messages load when it is opened.
- `attachments/`: the app's copies of attached files, and generated files (deleted with their
  session).
- `projects.json`: project folders.
- `mcp.json`: MCP servers, in the `mcpServers` shape other clients use. Strings may name
  environment variables as `${NAME}` or `${NAME:-default}`, so secrets can stay out of the file.
  A server can carry `"oauth": { "clientId": …, "clientSecret": …, "scope": …, "callbackPort": … }`
  for providers that need a pre-registered OAuth client. Edits made elsewhere apply when the
  window regains focus.
- `mcp-auth.json`: MCP sign-in tokens, and the OAuth clients registered per authorization server.

On Unix these files have mode `0600`, and they are written atomically. Signing out revokes the
grant and removes the refresh token but keeps sessions; a grant that is revoked, unused for 60
days or missing a scope the app needs sends the user back to sign in.

## Known limits (deliberate, for now)

- No complex-script shaping (Arabic joining, Indic reordering) and no bidi.
- Screen readers work on Windows and macOS only (Linux's AT-SPI adapter needs an async
  runtime), and the tree they get is flat: no groups or headings within settings.
- GIF and WebP attachments show as chips, not thumbnails.
- Native file and folder pickers run the platform's helper (PowerShell, `osascript`,
  `zenity`/`kdialog`), so they take a moment to appear.
- Release builds are unsigned (the macOS `.app` is ad-hoc signed only) and there are no installers.
  A copy installed where it can't write (system-wide) only says an update is available.
- MCP: resources and prompts are not offered, only tools; a modern server's change
  notifications (`subscriptions/listen`) are not subscribed to, so its tool list refreshes on
  reconnect. Servers that need sampling, elicitation or roots get "not supported". OAuth uses
  dynamic client registration; Client ID Metadata Documents need an HTTPS page describing this
  app, which `serechat.com` would have to host.

## Fonts

- Inter, © The Inter Project Authors, SIL Open Font License (`assets/Inter-LICENSE.txt`).
- JetBrains Mono, © The JetBrains Mono Project Authors, SIL Open Font License
  (`assets/JetBrainsMono-LICENSE.txt`).
- Twemoji Mozilla: code Apache 2.0, graphics © Twitter, Inc. and other contributors, CC-BY 4.0
  (`assets/Twemoji-LICENSE.txt`).
