//! The agent's tools, confined to the session's project folder.
//!
//! Reading tools (list, read, search, find) run on their own. Anything that
//! changes files, runs a program or reaches the network waits for the user's
//! approval in the chat. Every path is resolved inside the project root,
//! symlinks included, so the model cannot touch files outside it.
//!
//! Tools block; the app runs them on worker threads.

use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use serechat::{Client, ToolCall};

use crate::platform::no_window;

/// Longest tool output returned to the model, in bytes.
const MAX_OUTPUT: usize = 32 * 1024;
/// Directories never listed or searched.
const IGNORED: &[&str] = &[".git", "node_modules", "target", ".venv", "venv", "__pycache__", ".next", ".cache", ".idea", ".vs", ".gradle"];
/// Most files a walk visits, so a huge tree cannot stall a tool.
const MAX_WALK: usize = 50_000;

/// A tool definition.
pub struct Tool {
    /// Name the model calls.
    pub name: &'static str,
    /// Description for the model.
    pub description: String,
    /// JSON Schema of the arguments.
    pub parameters: Value,
    /// Needs the user's approval before running.
    pub approval: bool,
}

/// Every tool, built once.
pub fn all() -> &'static [Tool] {
    static TOOLS: OnceLock<Vec<Tool>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        let shell = if cfg!(target_os = "windows") { "Windows PowerShell" } else { "sh" };
        let tool = |name, description: &str, parameters, approval| Tool { name, description: description.to_owned(), parameters, approval };
        let string = |d: &str| json!({ "type": "string", "description": d });
        let integer = |d: &str| json!({ "type": "integer", "description": d });
        vec![
            tool(
                "list_directory",
                "List files and folders in a project directory. Folders end with '/'. Skips .git, node_modules and build output.",
                json!({ "type": "object", "properties": {
                    "path": string("Directory relative to the project root; '.' for the root."),
                    "depth": integer("How many levels deep to list, 1 to 4 (default 1).") }, "required": ["path"] }),
                false,
            ),
            tool(
                "read_file",
                "Read a UTF-8 text file from the project. Long files can be read in parts with offset and limit, counted in lines.",
                json!({ "type": "object", "properties": {
                    "path": string("File path relative to the project root."),
                    "offset": integer("First line to read, starting at 1 (default 1)."),
                    "limit": integer("Number of lines to read (default 2000).") }, "required": ["path"] }),
                false,
            ),
            tool(
                "search_files",
                "Search the project's text files for a literal string. Returns matching lines as 'path:line: text'.",
                json!({ "type": "object", "properties": {
                    "query": string("Text to look for."),
                    "path": string("Folder to search, relative to the project root (default: whole project)."),
                    "case_sensitive": { "type": "boolean", "description": "Match case exactly (default false)." } }, "required": ["query"] }),
                false,
            ),
            tool(
                "find_files",
                "Find files by path pattern: '*' matches within a name, '**' across folders, e.g. '*.rs' or 'src/**/test_*.py'.",
                json!({ "type": "object", "properties": { "pattern": string("Glob-style pattern.") }, "required": ["pattern"] }),
                false,
            ),
            tool(
                "write_file",
                "Create or overwrite a file in the project with the given content; parent folders are created. The user must approve.",
                json!({ "type": "object", "properties": {
                    "path": string("File path relative to the project root."),
                    "content": string("The complete new file content.") }, "required": ["path", "content"] }),
                true,
            ),
            tool(
                "edit_file",
                "Replace an exact snippet of a file with new text. old_string must occur exactly once; include surrounding lines to make it unique. The user must approve.",
                json!({ "type": "object", "properties": {
                    "path": string("File path relative to the project root."),
                    "old_string": string("Exact text to replace."),
                    "new_string": string("Replacement text.") }, "required": ["path", "old_string", "new_string"] }),
                true,
            ),
            tool(
                "run_command",
                &format!("Run a command with {shell} in the project folder and return its exit code and output. The user must approve."),
                json!({ "type": "object", "properties": {
                    "command": string("The command line."),
                    "timeout_seconds": integer("Give up after this many seconds (default 120, at most 600).") }, "required": ["command"] }),
                true,
            ),
            tool(
                "fetch_url",
                "Fetch a web page or text file over HTTP(S) and return its text; HTML is reduced to readable text. The user must approve.",
                json!({ "type": "object", "properties": { "url": string("An http:// or https:// URL.") }, "required": ["url"] }),
                true,
            ),
        ]
    })
}

/// Whether `name` needs approval (unknown tools always do).
#[must_use]
pub fn needs_approval(name: &str) -> bool {
    all().iter().find(|t| t.name == name).is_none_or(|t| t.approval)
}

/// How a call reads in the chat: a verb, its target, and an optional preview.
pub struct CallView {
    /// "Read", "Run", …
    pub verb: &'static str,
    /// Path, command, query or URL.
    pub target: String,
    /// Content to show before approving (file content, edit, …).
    pub preview: Option<String>,
}

fn args(call: &ToolCall) -> Value {
    serde_json::from_str(&call.arguments).unwrap_or(Value::Null)
}

fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// Describes a call for display.
#[must_use]
pub fn view(call: &ToolCall) -> CallView {
    let a = args(call);
    let s = |key| arg(&a, key).unwrap_or_default().to_owned();
    let (verb, target, preview) = match call.name.as_str() {
        "list_directory" => ("List", s("path"), None),
        "read_file" => ("Read", s("path"), None),
        "search_files" => ("Search", format!("\"{}\"", s("query")), None),
        "find_files" => ("Find", s("pattern"), None),
        "write_file" => ("Write", s("path"), Some(s("content"))),
        "edit_file" => {
            let diff = |text: String, sign: char| text.lines().map(|l| format!("{sign} {l}")).collect::<Vec<_>>().join("\n");
            ("Edit", s("path"), Some(format!("{}\n{}", diff(s("old_string"), '-'), diff(s("new_string"), '+'))))
        }
        "run_command" => ("Run", s("command"), None),
        "fetch_url" => ("Fetch", s("url"), None),
        other => ("Call", other.to_owned(), Some(call.arguments.clone())),
    };
    CallView { verb, target, preview }
}

/// Runs `call` inside `root`. `cancel` aborts long-running commands.
///
/// # Errors
/// A message for the model: bad arguments, a path outside the project, or
/// the operation's own failure.
pub fn run(root: &Path, client: &Client, call: &ToolCall, cancel: &AtomicBool) -> Result<String, String> {
    let a = args(call);
    if !a.is_object() {
        return Err("Arguments must be a JSON object.".into());
    }
    let required = |key: &str| arg(&a, key).ok_or_else(|| format!("Missing the '{key}' argument."));
    let number = |key: &str| a.get(key).and_then(Value::as_u64);
    let output = match call.name.as_str() {
        "list_directory" => list_directory(root, &resolve(root, arg(&a, "path").unwrap_or("."))?, number("depth").unwrap_or(1).clamp(1, 4) as usize),
        "read_file" => read_file(&resolve(root, required("path")?)?, number("offset").unwrap_or(1), number("limit").unwrap_or(2000)),
        "search_files" => {
            let dir = resolve(root, arg(&a, "path").unwrap_or("."))?;
            search_files(root, &dir, required("query")?, a.get("case_sensitive").and_then(Value::as_bool).unwrap_or(false))
        }
        "find_files" => find_files(root, required("pattern")?),
        "write_file" => {
            let path = resolve(root, required("path")?)?;
            let content = required("content")?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            fs::write(&path, content).map_err(|e| e.to_string())?;
            Ok(format!("Wrote {} bytes to {}.", content.len(), relative(root, &path)))
        }
        "edit_file" => {
            let path = resolve(root, required("path")?)?;
            let (old, new) = (required("old_string")?, required("new_string")?);
            let text = fs::read_to_string(&path).map_err(|e| e.to_string())?;
            match text.matches(old).count() {
                0 => Err("old_string was not found in the file.".into()),
                1 => {
                    fs::write(&path, text.replacen(old, new, 1)).map_err(|e| e.to_string())?;
                    Ok(format!("Edited {}.", relative(root, &path)))
                }
                n => Err(format!("old_string occurs {n} times; include more context so it is unique.")),
            }
        }
        "run_command" => run_command(root, required("command")?, number("timeout_seconds").unwrap_or(120).clamp(1, 600), cancel),
        "fetch_url" => fetch_url(client, required("url")?),
        other => Err(format!("There is no tool named '{other}'.")),
    }?;
    Ok(truncate_middle(output, MAX_OUTPUT))
}

/// Resolves `path` (relative to `root`, or absolute inside it) and makes sure
/// it cannot escape the project, even through `..` or symlinks.
fn resolve(root: &Path, path: &str) -> Result<PathBuf, String> {
    let requested = Path::new(path.trim());
    let joined = if requested.is_absolute() { requested.to_path_buf() } else { root.join(requested) };
    let mut normal = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                normal.pop();
            }
            Component::CurDir => {}
            other => normal.push(other),
        }
    }
    let outside = || format!("{path} is outside the project folder.");
    if !normal.starts_with(root) {
        return Err(outside());
    }
    // Compare real locations: the deepest existing ancestor must be inside
    // the real root, so a symlink cannot lead out.
    let real_root = root.canonicalize().map_err(|e| format!("The project folder is unavailable: {e}"))?;
    let mut probe = normal.clone();
    while !probe.exists() {
        if !probe.pop() {
            return Err(outside());
        }
    }
    let real = probe.canonicalize().map_err(|e| e.to_string())?;
    if real.starts_with(&real_root) { Ok(normal) } else { Err(outside()) }
}

/// `path` relative to `root`, with `/` separators.
fn relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let text = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
    if text.is_empty() { ".".into() } else { text }
}

/// Visits files and folders under `dir` breadth-first (sorted, ignored
/// folders and symlinks skipped), calling `visit(path, is_dir, depth)`.
/// Stops when `visit` returns `false` or after [`MAX_WALK`] entries.
fn walk(dir: &Path, max_depth: usize, mut visit: impl FnMut(&Path, bool, usize) -> bool) {
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    let mut seen = 0;
    while let Some((folder, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&folder) else { continue };
        let mut entries: Vec<_> = entries.flatten().filter_map(|e| Some((e.path(), e.file_type().ok()?))).collect();
        entries.sort_by(|a, b| (!a.1.is_dir(), &a.0).cmp(&(!b.1.is_dir(), &b.0)));
        let mut subfolders = Vec::new();
        for (path, kind) in entries {
            if kind.is_symlink() {
                continue;
            }
            let is_dir = kind.is_dir();
            if is_dir && path.file_name().and_then(|n| n.to_str()).is_some_and(|n| IGNORED.contains(&n)) {
                continue;
            }
            seen += 1;
            if seen > MAX_WALK || !visit(&path, is_dir, depth) {
                return;
            }
            if is_dir && depth + 1 < max_depth {
                subfolders.push((path, depth + 1));
            }
        }
        // Reverse so the stack pops them in sorted order.
        stack.extend(subfolders.into_iter().rev());
    }
}

fn list_directory(root: &Path, dir: &Path, depth: usize) -> Result<String, String> {
    if !dir.is_dir() {
        return Err(format!("{} is not a folder.", relative(root, dir)));
    }
    let mut lines = Vec::new();
    walk(dir, depth, |path, is_dir, level| {
        let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
        let size = if is_dir { String::new() } else { fs::metadata(path).map(|m| format!("  ({})", crate::attachments::human_size(m.len()))).unwrap_or_default() };
        lines.push(format!("{}{name}{}{size}", "  ".repeat(level), if is_dir { "/" } else { "" }));
        lines.len() < 500
    });
    if lines.is_empty() {
        return Ok(format!("{} is empty.", relative(root, dir)));
    }
    if lines.len() >= 500 {
        lines.push("… (listing truncated)".into());
    }
    Ok(lines.join("\n"))
}

fn read_file(path: &Path, offset: u64, limit: u64) -> Result<String, String> {
    let meta = fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        return Err("That is a folder; use list_directory.".into());
    }
    if meta.len() > 8 << 20 {
        return Err("The file is larger than 8 MB.".into());
    }
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return Err("The file is binary.".into());
    }
    let text = String::from_utf8_lossy(&bytes);
    let total = text.lines().count();
    let first = offset.max(1) as usize;
    let lines: Vec<&str> = text.lines().skip(first - 1).take(limit.max(1) as usize).collect();
    let last = first + lines.len().saturating_sub(1);
    let mut out = lines.join("\n");
    if first > 1 || last < total {
        let _ = write!(out, "\n\n[lines {first}-{last} of {total}]");
    }
    Ok(out)
}

fn search_files(root: &Path, dir: &Path, query: &str, case_sensitive: bool) -> Result<String, String> {
    if query.is_empty() {
        return Err("The query is empty.".into());
    }
    let needle = if case_sensitive { query.to_owned() } else { query.to_lowercase() };
    let mut hits = Vec::new();
    walk(dir, usize::MAX, |path, is_dir, _| {
        if is_dir || fs::metadata(path).is_ok_and(|m| m.len() > 1 << 20) {
            return true;
        }
        let Ok(bytes) = fs::read(path) else { return true };
        if bytes.iter().take(1024).any(|b| *b == 0) {
            return true;
        }
        for (n, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
            let found = if case_sensitive { line.contains(&needle) } else { line.to_lowercase().contains(&needle) };
            if found {
                let line: String = line.trim().chars().take(200).collect();
                hits.push(format!("{}:{}: {line}", relative(root, path), n + 1));
                if hits.len() >= 200 {
                    return false;
                }
            }
        }
        true
    });
    Ok(match hits.len() {
        0 => format!("No matches for \"{query}\"."),
        200 => hits.join("\n") + "\n… (stopped after 200 matches)",
        _ => hits.join("\n"),
    })
}

/// Glob match over `/`-separated paths: `*` and `?` stay within a name,
/// `**` spans folders.
fn glob(pattern: &[u8], path: &[u8]) -> bool {
    match pattern {
        [] => path.is_empty(),
        [b'*', b'*', rest @ ..] => {
            let rest = rest.strip_prefix(b"/").unwrap_or(rest);
            (0..=path.len()).any(|i| (i == 0 || path[i - 1] == b'/') && glob(rest, &path[i..]))
        }
        [b'*', rest @ ..] => (0..=path.len()).take_while(|&i| i == 0 || path[i - 1] != b'/').any(|i| glob(rest, &path[i..])),
        [b'?', rest @ ..] => path.first().is_some_and(|c| *c != b'/') && glob(rest, &path[1..]),
        [c, rest @ ..] => path.first().is_some_and(|p| p.eq_ignore_ascii_case(c)) && glob(rest, &path[1..]),
    }
}

fn find_files(root: &Path, pattern: &str) -> Result<String, String> {
    let pattern = pattern.trim().trim_start_matches("./");
    if pattern.is_empty() {
        return Err("The pattern is empty.".into());
    }
    // A pattern without folders matches file names anywhere.
    let pattern = if pattern.contains('/') { pattern.to_owned() } else { format!("**/{pattern}") };
    let mut found = Vec::new();
    walk(root, usize::MAX, |path, is_dir, _| {
        let rel = relative(root, path);
        if !is_dir && glob(pattern.as_bytes(), rel.as_bytes()) {
            found.push(rel);
        }
        found.len() < 300
    });
    Ok(if found.is_empty() { format!("No files match {pattern}.") } else { found.join("\n") })
}

fn run_command(root: &Path, command: &str, timeout: u64, cancel: &AtomicBool) -> Result<String, String> {
    let mut process = if cfg!(target_os = "windows") {
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-NonInteractive", "-Command", command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", command]);
        c
    };
    let mut child = no_window(&mut process)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("The command could not start: {e}"))?;
    // Drain both pipes on their own threads so a chatty command never blocks.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.by_ref().take(4 << 20).read_to_end(&mut out);
                // Keep draining past the cap so the child cannot block on a full pipe.
                let _ = std::io::copy(&mut pipe, &mut std::io::sink());
            }
            out
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if cancel.load(Ordering::Relaxed) || started.elapsed() > Duration::from_secs(timeout) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(40)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let (out, err) = (stdout.join().unwrap_or_default(), stderr.join().unwrap_or_default());
    let mut text = String::from_utf8_lossy(&out).into_owned();
    if !err.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&String::from_utf8_lossy(&err));
    }
    let head = match status {
        Some(status) => format!("exit code: {}", status.code().map_or_else(|| "killed by a signal".into(), |c| c.to_string())),
        None if cancel.load(Ordering::Relaxed) => "stopped by the user".into(),
        None => format!("timed out after {timeout} s and was stopped"),
    };
    Ok(if text.trim().is_empty() { format!("{head}\n(no output)") } else { format!("{head}\n{}", text.trim_end()) })
}

fn fetch_url(client: &Client, url: &str) -> Result<String, String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("Only http:// and https:// URLs can be fetched.".into());
    }
    let (content_type, body) = client.fetch_text(url, 4 << 20).map_err(|e| e.to_string())?;
    Ok(if content_type.contains("html") { html_to_text(&body) } else { body })
}

/// Reduces HTML to readable text: drops scripts, styles and tags, decodes
/// common entities and collapses blank space.
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 3);
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    while i < html.len() {
        if html[i..].starts_with('<') {
            let rest = &lower[i..];
            let skip_to = ["script", "style", "noscript", "svg"].iter().find_map(|tag| {
                rest.starts_with(&format!("<{tag}")).then(|| rest.find(&format!("</{tag}>")).map_or(html.len(), |n| i + n + tag.len() + 3))
            });
            let end = skip_to.unwrap_or_else(|| rest.find('>').map_or(html.len(), |n| i + n + 1));
            let tag = &rest[..(end - i).min(rest.len())];
            if ["<p", "<br", "<div", "<li", "<h1", "<h2", "<h3", "<h4", "<tr", "</p", "</div", "</h"].iter().any(|t| tag.starts_with(t)) {
                out.push('\n');
            }
            i = end;
            continue;
        }
        let next = html[i..].find('<').map_or(html.len(), |n| i + n);
        out.push_str(&html[i..next]);
        i = next;
    }
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    let mut text = String::new();
    let mut blank = 0;
    for line in decoded.lines().map(|l| l.split_whitespace().collect::<Vec<_>>().join(" ")) {
        if line.is_empty() {
            blank += 1;
            continue;
        }
        if !text.is_empty() {
            text.push_str(if blank > 0 { "\n\n" } else { "\n" });
        }
        text.push_str(&line);
        blank = 0;
    }
    text
}

/// Keeps the start and end of an over-long output.
fn truncate_middle(text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut head = max / 2;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - max / 2;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}\n\n… ({} bytes omitted) …\n\n{}", &text[..head], tail - head, &text[tail..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("serechat-tools-{}-{}", std::process::id(), serechat::new_session_id()));
        fs::create_dir_all(dir.join("src/nested")).unwrap();
        fs::create_dir_all(dir.join("node_modules/x")).unwrap();
        fs::write(dir.join("src/main.rs"), "fn main() {\n    println!(\"Hello\");\n}\n").unwrap();
        fs::write(dir.join("src/nested/lib.rs"), "pub fn hello() {}\n").unwrap();
        fs::write(dir.join("node_modules/x/index.js"), "hello").unwrap();
        fs::write(dir.join("README.md"), "# Demo\n").unwrap();
        dir
    }

    fn call(name: &str, arguments: &Value) -> ToolCall {
        ToolCall { call_id: "c".into(), name: name.into(), arguments: arguments.to_string() }
    }

    fn exec(root: &Path, name: &str, arguments: &Value) -> Result<String, String> {
        run(root, &Client::new(None), &call(name, arguments), &AtomicBool::new(false))
    }

    #[test]
    fn paths_stay_inside_the_project() {
        let root = project();
        assert!(resolve(&root, "src/../README.md").is_ok());
        assert!(resolve(&root, "../outside").unwrap_err().contains("outside"));
        assert!(resolve(&root, "src/../../x").is_err());
        let absolute = std::env::temp_dir().join("elsewhere.txt");
        assert!(resolve(&root, &absolute.to_string_lossy()).is_err());
        assert!(resolve(&root, "new/dir/file.txt").is_ok(), "new files may be created inside");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn reading_tools() {
        let root = project();
        let listing = exec(&root, "list_directory", &json!({ "path": ".", "depth": 2 })).unwrap();
        assert!(listing.contains("src/") && listing.contains("  main.rs") && !listing.contains("node_modules"), "{listing}");
        assert_eq!(exec(&root, "read_file", &json!({ "path": "src/main.rs", "offset": 2, "limit": 1 })).unwrap(), "    println!(\"Hello\");\n\n[lines 2-2 of 3]");
        let found = exec(&root, "search_files", &json!({ "query": "HELLO" })).unwrap();
        assert!(found.contains("src/main.rs:2:") && found.contains("src/nested/lib.rs:1:") && !found.contains("node_modules"), "{found}");
        assert_eq!(exec(&root, "find_files", &json!({ "pattern": "*.rs" })).unwrap(), "src/main.rs\nsrc/nested/lib.rs");
        assert_eq!(exec(&root, "find_files", &json!({ "pattern": "src/*.rs" })).unwrap(), "src/main.rs");
        assert!(exec(&root, "read_file", &json!({ "path": "../x" })).is_err());
        assert!(exec(&root, "read_file", &json!({})).unwrap_err().contains("path"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn editing_tools() {
        let root = project();
        exec(&root, "write_file", &json!({ "path": "out/new.txt", "content": "a b a" })).unwrap();
        assert!(exec(&root, "edit_file", &json!({ "path": "out/new.txt", "old_string": "a", "new_string": "x" })).unwrap_err().contains("2 times"));
        exec(&root, "edit_file", &json!({ "path": "out/new.txt", "old_string": "b", "new_string": "c" })).unwrap();
        assert_eq!(fs::read_to_string(root.join("out/new.txt")).unwrap(), "a c a");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn commands_report_exit_codes() {
        let root = project();
        let out = exec(&root, "run_command", &json!({ "command": "echo hi" })).unwrap();
        assert!(out.starts_with("exit code: 0") && out.contains("hi"), "{out}");
        let cancelled = AtomicBool::new(true);
        let slow = if cfg!(target_os = "windows") { "Start-Sleep 30" } else { "sleep 30" };
        let out = run(&root, &Client::new(None), &call("run_command", &json!({ "command": slow })), &cancelled).unwrap();
        assert!(out.starts_with("stopped by the user"), "{out}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn helpers() {
        assert!(glob(b"**/*.rs", b"a/b/c.rs") && glob(b"**/*.rs", b"c.rs") && !glob(b"*.rs", b"a/c.rs"));
        assert_eq!(html_to_text("<p>Hi &amp; <b>bye</b></p><script>x()</script><p>next</p>"), "Hi & bye\n\nnext");
        let long = "é".repeat(100);
        let cut = truncate_middle(long, 21);
        assert!(cut.contains("omitted") && cut.is_char_boundary(cut.len()));
        assert!(needs_approval("run_command") && !needs_approval("read_file") && needs_approval("unknown"));
    }
}
