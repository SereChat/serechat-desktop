# SereChat Desktop

A native, GPU-rendered desktop client for [SereChat](https://serechat.com). It runs on
Windows, macOS and Linux, and it is pure Rust: no web view and no UI framework.

```sh
cargo run --release -p serechat-desktop     # binary name: serechat
cargo test --workspace
cargo clippy --workspace --all-targets
```

## Layout

| Crate            | Purpose                                                                  |
|------------------|--------------------------------------------------------------------------|
| `crates/serechat` | API client: device-code sign-in, model list, streaming Responses API, config, session files |
| `crates/desktop`  | The app: winit window, wgpu renderer, immediate-mode UI                   |

`crates/desktop/src`:

- `gpu.rs` + `shader.wgsl`: a single instanced pipeline. Rounded rectangles, borders,
  soft shadows and glyphs are all SDF quads, drawn with one draw call per frame.
- `text.rs`: Inter (embedded), a kerned word-wrapping layout, and an R8 glyph atlas.
- `paint.rs`: the drawing API (`Painter`) plus `Rect` and colours.
- `ui.rs`: input state, eased hover animations, buttons, keyboard editing.
- `editor.rs`: the text-editing model (caret, selection, word movement).
- `login.rs` / `chat.rs` / `settings.rs`: sign-in, the chat screen, and the settings page.
- `theme.rs`: the three colour schemes (Dark, One Dark, Light) plus sizes and text styles.
- `app.rs`: event routing, worker threads, and the frame loop.

The app redraws only when something changes: input, network events, animations, or the
caret blink timer. Network calls run on worker threads and post results back to the event loop.

## Sign-in and storage

The first time the app starts, it runs SereChat's device-code flow. The browser opens the
approval page, and the user types the 6-digit code into the app. The token and the chosen
model, reasoning effort and colour scheme are saved in `~/.serechat/config.toml`. Every
conversation is saved as `~/.serechat/sessions/<id>.json`, including each reply's tokens and
cost at the prices of the time. On Unix these files have mode `0600`, and they are
written atomically. Signing out (or a `401` from the API) removes the token but keeps sessions.

## Known limits (deliberate, for now)

- Replies are plain text, with no Markdown yet. All sessions load at startup.
- There is no text shaping and no font fallback, so emoji and CJK characters show as boxes.
- There is no IME support, and the caret moves per `char` rather than per grapheme cluster.

The font is Inter, © The Inter Project Authors, under the SIL Open Font License
(`crates/desktop/assets/Inter-LICENSE.txt`).
