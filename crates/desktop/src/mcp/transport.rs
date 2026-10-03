//! One connection to an MCP server: JSON-RPC 2.0 over stdio, Streamable
//! HTTP or the deprecated HTTP+SSE transport.
//!
//! The client is dual-era. It speaks the stateless protocol (`2026-07-28`:
//! every request carries its version, identity and capabilities in `_meta`)
//! and the earlier revisions that open with an `initialize` handshake
//! (`2024-11-05` to `2025-11-25`). It probes with `server/discover` and falls
//! back as the spec's backward-compatibility sections describe: on stdio,
//! any error that is not a recognised modern one (or silence) means a legacy
//! server; on HTTP, a 4xx without a modern error body does, and if the
//! legacy `initialize` POST is refused too, the old HTTP+SSE transport is
//! tried.
//!
//! The client declares no capabilities (no sampling, elicitation or roots),
//! so servers have nothing to ask of it; server requests that arrive anyway
//! get a "method not found" error, and `ping` an empty result.
//!
//! Calls block; the app makes them on worker threads. Requests on one
//! connection run concurrently: stdio and SSE replies are matched to their
//! requests by id on a reader thread; HTTP requests are separate POSTs.
//!
//! ponytail: an abandoned HTTP request (cancelled or timed out) keeps its
//! thread until the server answers or the socket times out, because ureq
//! cannot abort a blocking read from another thread. Likewise the GET
//! stream of a closed HTTP+SSE connection.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::config::{ServerConfig, Transport, expand};
use crate::platform::no_window;

/// The stateless protocol revision.
pub const MODERN: &str = "2026-07-28";
/// Handshake-based revisions, newest first.
pub const LEGACY: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// Error codes only a modern server sends: header mismatch, missing client
/// capability, unsupported protocol version.
const MODERN_ERRORS: [i64; 3] = [-32020, -32021, -32022];
/// Longest a line or message from a server may be.
const MAX_MESSAGE: u64 = 64 << 20;
/// Server stderr kept for error messages.
const STDERR_KEEP: usize = 4096;
/// How long a stdio server may stay silent after `server/discover` before
/// it counts as a legacy server that drops unknown requests.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest a handshake or a listing may take (a first `npx` run downloads).
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(120);

/// What went wrong with a request.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// The server answered with a JSON-RPC error.
    Rpc {
        /// Error code.
        code: i64,
        /// Its message.
        message: String,
        /// Extra data.
        data: Value,
    },
    /// The server wants a (new) access token: `401`, or `403` with
    /// `insufficient_scope`. Holds the `WWW-Authenticate` header.
    Auth {
        /// HTTP status.
        status: u16,
        /// The challenge, possibly empty.
        challenge: String,
    },
    /// A non-success HTTP status with no JSON-RPC error in its body.
    Http {
        /// Status code.
        status: u16,
        /// Start of the body, for the error message.
        body: String,
    },
    /// The connection failed or the server sent garbage.
    Transport(String),
    /// No answer in time.
    Timeout,
    /// The caller gave up.
    Cancelled,
    /// The server is gone (its process exited, or the stream closed).
    Closed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rpc { message, code, .. } => write!(f, "{message} (error {code})"),
            Self::Auth { status: 403, .. } => f.write_str("The server needs more permissions; sign in again."),
            Self::Auth { .. } => f.write_str("The server needs you to sign in."),
            Self::Http { status, body } if body.is_empty() => write!(f, "The server answered HTTP {status}."),
            Self::Http { status, body } => write!(f, "The server answered HTTP {status}: {body}"),
            Self::Transport(e) => f.write_str(e),
            Self::Timeout => f.write_str("The server did not answer in time."),
            Self::Cancelled => f.write_str("Stopped by the user."),
            Self::Closed(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for Error {}

/// What the server said about itself when the connection opened.
#[derive(Debug, Clone, Default)]
pub struct Hello {
    /// The protocol revision in use.
    pub version: String,
    /// Guidance for the model, if the server gives any.
    pub instructions: Option<String>,
    /// Whether it offers tools.
    pub tools: bool,
}

/// Called with the method of every notification a server sends on its own
/// (e.g. `notifications/tools/list_changed`).
pub type OnNotify = Arc<dyn Fn(&str) + Send + Sync>;

/// Writes one message to a server.
type Sender = Box<dyn Fn(&Value) -> Result<(), String> + Send + Sync>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An open connection.
pub struct Connection {
    link: Link,
    /// `None` for the modern protocol; the negotiated revision otherwise.
    legacy: Option<String>,
    next_id: AtomicU64,
}

enum Link {
    Stdio(Stdio_),
    Http(Http),
    Sse(Sse),
}

impl Connection {
    /// Connects to `config`'s server, sending `token` as a bearer token
    /// over HTTP, and finds out which protocol it speaks.
    ///
    /// # Errors
    /// The server could not be started or reached, wants authorization
    /// ([`Error::Auth`]), or speaks no revision this client knows.
    pub fn open(config: &ServerConfig, token: Option<String>, notify: &OnNotify, cancel: &AtomicBool) -> Result<(Self, Hello), Error> {
        match &config.transport {
            Transport::Stdio { command, args, env, cwd } => {
                let link = Stdio_::spawn(command, args, env, cwd.as_deref(), notify)?;
                let mut connection = Self { link: Link::Stdio(link), legacy: None, next_id: AtomicU64::new(1) };
                // What the server printed says why it never answered.
                let hello = connection.handshake(PROBE_TIMEOUT, cancel).map_err(|e| match (e, connection.stderr()) {
                    (Error::Timeout, tail) if !tail.is_empty() => {
                        Error::Transport(format!("The server did not answer in time. It printed: {}", tail.lines().last().unwrap_or_default()))
                    }
                    (e, _) => e,
                })?;
                Ok((connection, hello))
            }
            Transport::Http { url, headers, sse } => {
                let url = expand(url);
                let headers: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.clone(), expand(v))).collect();
                let mut refused = None;
                if !*sse {
                    let http = Http::new(url.clone(), headers.clone(), token.clone());
                    let mut connection = Self { link: Link::Http(http), legacy: None, next_id: AtomicU64::new(1) };
                    match connection.handshake(SETUP_TIMEOUT, cancel) {
                        // A server that refuses even `initialize` may only
                        // speak the old HTTP+SSE transport.
                        Err(e @ Error::Http { status: 400 | 404 | 405, .. }) if connection.legacy.is_some() => refused = Some(e),
                        other => return other.map(|hello| (connection, hello)),
                    }
                }
                let link = Sse::open(&url, headers, token, notify, cancel).map_err(|e| match (e, refused) {
                    // Neither transport: the first refusal says more.
                    (Error::Http { .. }, Some(first)) => first,
                    (e, _) => e,
                })?;
                let mut connection = Self { link: Link::Sse(link), legacy: Some(LEGACY[0].to_owned()), next_id: AtomicU64::new(1) };
                let hello = connection.initialize(cancel)?;
                Ok((connection, hello))
            }
        }
    }

    /// Probes with `server/discover`, falling back to `initialize`.
    fn handshake(&mut self, probe_timeout: Duration, cancel: &AtomicBool) -> Result<Hello, Error> {
        match self.request("server/discover", json!({}), &[], probe_timeout, cancel) {
            Ok(result) => {
                let versions: Vec<&str> = result.get("supportedVersions").and_then(Value::as_array).map(|v| v.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                if versions.is_empty() || versions.contains(&MODERN) {
                    return Ok(hello(MODERN, &result));
                }
                // A modern server may still offer a legacy revision.
                self.fall_back(&versions, cancel)
            }
            Err(Error::Rpc { code: -32022, data, .. }) => {
                let supported: Vec<&str> = data.get("supported").and_then(Value::as_array).map(|v| v.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                self.fall_back(&supported, cancel)
            }
            Err(e @ Error::Rpc { code, .. }) if MODERN_ERRORS.contains(&code) => Err(e),
            // Anything else (an error, a 4xx without a modern body, or
            // silence) means a server from before the stateless revision.
            Err(Error::Rpc { .. } | Error::Http { status: 400..=499, .. } | Error::Timeout) => {
                self.legacy = Some(LEGACY[0].to_owned());
                self.initialize(cancel)
            }
            Err(e) => Err(e),
        }
    }

    /// Switches to a legacy revision from `versions`, if one is shared.
    fn fall_back(&mut self, versions: &[&str], cancel: &AtomicBool) -> Result<Hello, Error> {
        let Some(version) = LEGACY.iter().find(|v| versions.contains(v)) else {
            return Err(Error::Transport(format!("The server speaks MCP {}, which this app does not support.", versions.join(", "))));
        };
        self.legacy = Some((*version).to_owned());
        self.initialize(cancel)
    }

    /// The legacy handshake: `initialize`, then `notifications/initialized`.
    fn initialize(&mut self, cancel: &AtomicBool) -> Result<Hello, Error> {
        let asked = self.legacy.clone().unwrap_or_else(|| LEGACY[0].to_owned());
        let params = json!({ "protocolVersion": asked, "capabilities": {}, "clientInfo": client_info() });
        let result = self.request("initialize", params, &[], SETUP_TIMEOUT, cancel)?;
        let version = result.get("protocolVersion").and_then(Value::as_str).unwrap_or_default().to_owned();
        if !LEGACY.contains(&version.as_str()) {
            return Err(Error::Transport(format!("The server speaks MCP {version}, which this app does not support.")));
        }
        self.legacy = Some(version.clone());
        if let Link::Http(http) = &self.link {
            *lock(&http.0.version) = Some(version.clone());
        }
        self.notify("notifications/initialized", &json!({}));
        Ok(hello(&version, &result))
    }

    /// Sends `method` with `params` and waits up to `timeout` for its
    /// result. `headers` are added to HTTP requests (`Mcp-Param-*`).
    ///
    /// # Errors
    /// See [`Error`]. A result that asks for input (`input_required`) is an
    /// error: this client declares no capabilities to answer it with.
    pub fn request(&self, method: &str, mut params: Value, headers: &[(String, String)], timeout: Duration, cancel: &AtomicBool) -> Result<Value, Error> {
        if self.legacy.is_none() {
            add_meta(&mut params);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let reply = match &self.link {
            Link::Stdio(link) => link.pipe.call(id, &message, timeout, cancel),
            Link::Sse(link) => link.pipe.call(id, &message, timeout, cancel),
            Link::Http(link) => link.call(id, &message, self.legacy.is_none(), headers, timeout, cancel),
        }?;
        if let Some(error) = reply.get("error") {
            return Err(Error::Rpc {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: error.get("message").and_then(Value::as_str).unwrap_or("The server reported an error.").to_owned(),
                data: error.get("data").cloned().unwrap_or(Value::Null),
            });
        }
        let result = reply.get("result").cloned().ok_or_else(|| Error::Transport("The server's reply has no result.".into()))?;
        if result.get("resultType").and_then(Value::as_str) == Some("input_required") {
            return Err(Error::Transport("The server asked for input (a sampling, elicitation or roots request), which this app does not provide.".into()));
        }
        Ok(result)
    }

    /// Sends a notification; failures are ignored (nothing waits for it).
    fn notify(&self, method: &str, params: &Value) {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        match &self.link {
            Link::Stdio(link) => {
                let _ = link.pipe.send(&message);
            }
            Link::Sse(link) => {
                let _ = link.pipe.send(&message);
            }
            Link::Http(link) => {
                let _ = link.post(&message, false, &[], None);
            }
        }
    }

    /// Sets the bearer token HTTP requests carry.
    pub fn set_token(&self, token: Option<String>) {
        match &self.link {
            Link::Http(http) => *lock(&http.0.token) = token,
            Link::Sse(sse) => *lock(&sse.token) = token,
            Link::Stdio(_) => {}
        }
    }

    /// Whether requests still have somewhere to go.
    #[must_use]
    pub fn alive(&self) -> bool {
        match &self.link {
            Link::Stdio(link) => lock(&link.pipe.closed).is_none(),
            Link::Sse(link) => lock(&link.pipe.closed).is_none(),
            Link::Http(_) => true,
        }
    }

    /// Whether requests go over HTTP (so tool arguments may become headers).
    #[must_use]
    pub fn is_http(&self) -> bool {
        matches!(self.link, Link::Http(_))
    }

    /// Recent stderr output of a stdio server.
    fn stderr(&self) -> String {
        match &self.link {
            Link::Stdio(link) => String::from_utf8_lossy(&lock(&link.stderr)).trim().to_owned(),
            _ => String::new(),
        }
    }

    /// Starts ending the connection: closes a server's stdin (its signal to
    /// exit), ends an HTTP session, or stops reading an event stream.
    /// Requests still waiting fail. Safe to call more than once.
    pub fn release(&self) {
        match &self.link {
            Link::Stdio(link) => {
                lock(&link.stdin).take();
                link.pipe.close("Closed.");
            }
            Link::Http(http) => http.end_session(),
            Link::Sse(sse) => sse.pipe.close("Closed."),
        }
    }

    /// Whether a stdio server's process has exited (other links: always).
    #[must_use]
    pub fn exited(&self) -> bool {
        match &self.link {
            Link::Stdio(link) => !matches!(lock(&link.child).try_wait(), Ok(None)),
            _ => true,
        }
    }

    /// Ends a stdio server's process tree if it is still running.
    pub fn kill(&self) {
        if let Link::Stdio(link) = &self.link {
            let mut child = lock(&link.child);
            // Only a process still running: an exited one's id may be reused.
            if matches!(child.try_wait(), Ok(None)) {
                crate::process::kill_tree(&mut child);
            }
        }
    }
}

impl Drop for Connection {
    /// Releases the connection and gives a stdio server two seconds to exit
    /// before ending it.
    fn drop(&mut self) {
        self.release();
        if let Link::Stdio(link) = &self.link {
            let child = Arc::clone(&link.child);
            spawn_thread("serechat-mcp-stop", move || {
                let deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < deadline && matches!(lock(&child).try_wait(), Ok(None)) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                let mut child = lock(&child);
                if matches!(child.try_wait(), Ok(None)) {
                    crate::process::kill_tree(&mut child);
                }
            });
        }
    }
}

/// The client's name and version, sent with every modern request.
fn client_info() -> Value {
    json!({ "name": "serechat-desktop", "title": "SereChat Desktop", "version": env!("CARGO_PKG_VERSION") })
}

/// Adds the modern per-request metadata to `params`.
fn add_meta(params: &mut Value) {
    if !params.is_object() {
        *params = json!({});
    }
    let meta = params.as_object_mut().map(|p| p.entry("_meta").or_insert_with(|| json!({})));
    if let Some(Value::Object(meta)) = meta {
        meta.insert("io.modelcontextprotocol/protocolVersion".into(), MODERN.into());
        meta.insert("io.modelcontextprotocol/clientInfo".into(), client_info());
        meta.insert("io.modelcontextprotocol/clientCapabilities".into(), json!({}));
    }
}

/// What a `server/discover` or `initialize` result says.
fn hello(version: &str, result: &Value) -> Hello {
    Hello {
        version: version.to_owned(),
        instructions: result.get("instructions").and_then(Value::as_str).map(str::to_owned).filter(|s| !s.trim().is_empty()),
        // Servers that list no capabilities at all may still have tools.
        tools: result.get("capabilities").is_none_or(|c| c.get("tools").is_some()),
    }
}

/// Answers a request a server sent us: `ping` with an empty result,
/// anything else as unknown (we declared no capabilities).
fn answer(request: &Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    if request.get("method").and_then(Value::as_str) == Some("ping") {
        json!({ "jsonrpc": "2.0", "id": id, "result": {} })
    } else {
        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Method not found" } })
    }
}

/// Waits on `rx` for up to `timeout`, giving up early when `cancel` is raised.
fn wait<T>(rx: &mpsc::Receiver<T>, timeout: Duration, cancel: &AtomicBool) -> Result<T, Error> {
    let deadline = Instant::now() + timeout;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Error::Timeout);
        }
        match rx.recv_timeout(left.min(Duration::from_millis(100))) {
            Ok(value) => return Ok(value),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(Error::Closed("The connection closed.".into())),
        }
    }
}

/// Replies routed to waiting requests by id: the stdio and HTTP+SSE links.
struct Pipe {
    pending: Mutex<HashMap<u64, mpsc::Sender<Result<Value, Error>>>>,
    /// Why the link closed, once it has.
    closed: Mutex<Option<String>>,
    /// Writes one message.
    sender: Sender,
    notify: OnNotify,
}

impl Pipe {
    fn new(sender: Sender, notify: OnNotify) -> Self {
        Self { pending: Mutex::new(HashMap::new()), closed: Mutex::new(None), sender, notify }
    }

    fn send(&self, message: &Value) -> Result<(), Error> {
        if let Some(why) = lock(&self.closed).clone() {
            return Err(Error::Closed(why));
        }
        (self.sender)(message).map_err(Error::Transport)
    }

    /// Sends request `id` and waits for its reply. A request given up on is
    /// cancelled at the server.
    fn call(&self, id: u64, message: &Value, timeout: Duration, cancel: &AtomicBool) -> Result<Value, Error> {
        let (tx, rx) = mpsc::channel();
        lock(&self.pending).insert(id, tx);
        let result = self.send(message).and_then(|()| wait(&rx, timeout, cancel).and_then(|r| r));
        lock(&self.pending).remove(&id);
        if matches!(result, Err(Error::Timeout | Error::Cancelled)) {
            let reason = if matches!(result, Err(Error::Cancelled)) { "The user stopped it." } else { "It took too long." };
            let _ = self.send(&json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": id, "reason": reason } }));
        }
        result
    }

    /// Handles one message from the server.
    fn dispatch(&self, message: Value) {
        let method = message.get("method").and_then(Value::as_str);
        match (method, message.get("id")) {
            (Some(_), Some(_)) => {
                let _ = self.send(&answer(&message));
            }
            (Some(method), None) => (self.notify)(method),
            (None, Some(id)) => {
                if let Some(tx) = id.as_u64().and_then(|id| lock(&self.pending).remove(&id)) {
                    let _ = tx.send(Ok(message));
                }
            }
            (None, None) => {}
        }
    }

    /// Marks the link closed and fails every waiting request.
    fn close(&self, why: &str) {
        lock(&self.closed).get_or_insert_with(|| why.to_owned());
        for (_, tx) in lock(&self.pending).drain() {
            let _ = tx.send(Err(Error::Closed(why.to_owned())));
        }
    }
}

/// A server running as a child process.
// Named with a trailing underscore: `Stdio` is std's.
struct Stdio_ {
    pipe: Arc<Pipe>,
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl Stdio_ {
    fn spawn(command: &str, args: &[String], env: &[(String, String)], cwd: Option<&str>, notify: &OnNotify) -> Result<Self, Error> {
        let program = expand(command);
        let mut process = Command::new(resolve_program(&program));
        process.args(args.iter().map(|a| expand(a)));
        if let Some(path) = login_path() {
            process.env("PATH", path);
        }
        process.envs(env.iter().map(|(k, v)| (k, expand(v))));
        let dir = cwd.map(expand).map(std::path::PathBuf::from).or_else(std::env::home_dir).unwrap_or_else(std::env::temp_dir);
        process.current_dir(dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut process, 0);
        let mut child = no_window(&mut process).spawn().map_err(|e| {
            Error::Transport(if e.kind() == std::io::ErrorKind::NotFound {
                format!("{program} was not found. Install it, or give its full path.")
            } else {
                format!("{program} could not start: {e}")
            })
        })?;
        let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
            return Err(Error::Transport("The server's pipes could not be opened.".into()));
        };
        let stdin = Arc::new(Mutex::new(Some(stdin)));
        let writer = Arc::clone(&stdin);
        let sender = Box::new(move |message: &Value| -> Result<(), String> {
            let mut line = message.to_string();
            line.push('\n');
            let mut stdin = lock(&writer);
            let stdin = stdin.as_mut().ok_or("The server's input is closed.")?;
            stdin.write_all(line.as_bytes()).and_then(|()| stdin.flush()).map_err(|e| format!("The server stopped reading: {e}"))
        });
        let pipe = Arc::new(Pipe::new(sender, Arc::clone(notify)));
        let child = Arc::new(Mutex::new(child));
        let errors = Arc::new(Mutex::new(Vec::new()));

        let collected = Arc::clone(&errors);
        spawn_thread("serechat-mcp-stderr", move || {
            let mut stderr = stderr;
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = stderr.read(&mut buf) {
                let mut kept = lock(&collected);
                kept.extend_from_slice(&buf[..n]);
                let excess = kept.len().saturating_sub(STDERR_KEEP);
                kept.drain(..excess);
            }
        });
        let reader = Arc::clone(&pipe);
        let (exited, stderr_tail) = (Arc::clone(&child), Arc::clone(&errors));
        spawn_thread("serechat-mcp-stdout", move || {
            let mut stdout = BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                line.clear();
                match (&mut stdout).take(MAX_MESSAGE).read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    // An over-long line: skip the rest of it.
                    Ok(_) if line.last() != Some(&b'\n') && line.len() as u64 >= MAX_MESSAGE => {
                        let _ = stdout.skip_until(b'\n');
                    }
                    // Servers must only print messages; anything else is ignored.
                    Ok(_) => {
                        if let Ok(message) = serde_json::from_slice::<Value>(&line) {
                            reader.dispatch(message);
                        }
                    }
                }
            }
            // Give the process a moment to exit, so the reason says how.
            std::thread::sleep(Duration::from_millis(100));
            let status = lock(&exited).try_wait().ok().flatten();
            let tail = String::from_utf8_lossy(&lock(&stderr_tail)).trim().lines().last().unwrap_or_default().to_owned();
            let mut why = match status.and_then(|s| s.code()) {
                Some(code) => format!("The server exited with code {code}."),
                None => "The server stopped.".to_owned(),
            };
            if !tail.is_empty() {
                why = format!("{why} {tail}");
            }
            reader.close(&why);
        });
        Ok(Self { pipe, child, stdin, stderr: errors })
    }
}

/// Starts a named thread; a failure to start one only loses that thread's work.
fn spawn_thread(name: &str, work: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name.to_owned()).spawn(work) {
        eprintln!("serechat: cannot start {name}: {e}");
    }
}

/// `program` as the OS can start it. Windows only runs `.exe` files found
/// on `PATH` by bare name, but `npx`, `uvx` and friends are often `.cmd`
/// scripts there.
fn resolve_program(program: &str) -> std::path::PathBuf {
    let path = std::path::PathBuf::from(program);
    if !cfg!(windows) || path.extension().is_some() || path.components().count() > 1 {
        return path;
    }
    let dirs = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    for ext in ["exe", "cmd", "bat", "com"] {
        if let Some(found) = dirs.iter().map(|d| d.join(format!("{program}.{ext}"))).find(|p| p.is_file()) {
            return found;
        }
    }
    path
}

/// The `PATH` of the user's login shell, on macOS and Linux. Apps started
/// from the Dock or a desktop launcher get a minimal `PATH` without
/// Homebrew, nvm or `~/.local/bin`, where `npx` and `uvx` usually live.
fn login_path() -> Option<String> {
    static PATH: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        if cfg!(windows) {
            return None;
        }
        let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
        // Interactive login shells may print banners; markers fence the value.
        let mut child = Command::new(shell)
            .args(["-ilc", "printf '\\n__SERECHAT_PATH__%s__END__\\n' \"$PATH\""])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        // A shell profile that waits for input must not hang every server.
        let deadline = Instant::now() + Duration::from_secs(5);
        while matches!(child.try_wait(), Ok(None)) {
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut text = String::new();
        child.stdout.take()?.read_to_string(&mut text).ok()?;
        let path = text.split("__SERECHAT_PATH__").nth(1)?.split("__END__").next()?.trim().to_owned();
        let current = std::env::var("PATH").unwrap_or_default();
        // Keep ours first, then whatever the login shell adds.
        let mut parts: Vec<&str> = current.split(':').filter(|p| !p.is_empty()).collect();
        for part in path.split(':') {
            if !part.is_empty() && !parts.contains(&part) {
                parts.push(part);
            }
        }
        Some(parts.join(":"))
    })
    .clone()
}

/// The HTTP client every MCP connection shares.
pub fn agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(15)))
            // Tool calls may take minutes before the server answers.
            .timeout_recv_response(Some(Duration::from_secs(600)))
            .user_agent(concat!("serechat-desktop/", env!("CARGO_PKG_VERSION")))
            .build()
            .into()
    })
}

/// A Streamable HTTP server. Cheap to clone: requests run on helper
/// threads that share its token and session.
#[derive(Clone)]
struct Http(Arc<HttpShared>);

struct HttpShared {
    url: String,
    /// Headers from the settings.
    headers: Vec<(String, String)>,
    token: Mutex<Option<String>>,
    /// Legacy session id (`Mcp-Session-Id`).
    session: Mutex<Option<String>>,
    /// Negotiated legacy revision, once known.
    version: Mutex<Option<String>>,
}

/// One HTTP response, read enough to act on.
enum Reply {
    /// `202 Accepted`, or a stream that ended without our reply.
    Nothing,
    /// The JSON-RPC message answering the request.
    Message(Value),
}

impl Http {
    fn new(url: String, headers: Vec<(String, String)>, token: Option<String>) -> Self {
        Self(Arc::new(HttpShared { url, headers, token: Mutex::new(token), session: Mutex::new(None), version: Mutex::new(None) }))
    }

    /// Sends request `id` and waits for its reply. A legacy session the
    /// server forgot (`404`) is started over once.
    fn call(&self, id: u64, message: &Value, modern: bool, extra: &[(String, String)], timeout: Duration, cancel: &AtomicBool) -> Result<Value, Error> {
        let had_session = lock(&self.0.session).is_some();
        let result = self.call_once(id, message, modern, extra, timeout, cancel);
        if !modern && had_session && matches!(result, Err(Error::Http { status: 404, .. })) {
            lock(&self.0.session).take();
            let version = lock(&self.0.version).clone().unwrap_or_else(|| LEGACY[0].to_owned());
            let init = json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize",
                "params": { "protocolVersion": version, "capabilities": {}, "clientInfo": client_info() } });
            self.call_once(0, &init, false, &[], SETUP_TIMEOUT, cancel)?;
            let _ = self.post(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }), false, &[], None);
            return self.call_once(id, message, modern, extra, timeout, cancel);
        }
        result
    }

    /// Posts on a helper thread, so the caller can give up waiting.
    fn call_once(&self, id: u64, message: &Value, modern: bool, extra: &[(String, String)], timeout: Duration, cancel: &AtomicBool) -> Result<Value, Error> {
        let (tx, rx) = mpsc::channel();
        let (http, message, extra) = (self.clone(), message.clone(), extra.to_vec());
        spawn_thread("serechat-mcp-http", move || {
            let _ = tx.send(http.post(&message, modern, &extra, Some(id)));
        });
        match wait(&rx, timeout, cancel)?? {
            Reply::Message(reply) => Ok(reply),
            Reply::Nothing => Err(Error::Transport("The server closed the stream before answering.".into())),
        }
    }

    /// POSTs `message` and reads the reply to request `id` from a JSON
    /// body or an SSE stream, answering server requests met on the way.
    fn post(&self, message: &Value, modern: bool, extra: &[(String, String)], id: Option<u64>) -> Result<Reply, Error> {
        let shared = &self.0;
        let method = message.get("method").and_then(Value::as_str).unwrap_or_default();
        let mut request = agent()
            .post(&shared.url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json");
        for (name, value) in &shared.headers {
            request = request.header(name, value);
        }
        if let Some(token) = lock(&shared.token).as_ref() {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        if modern {
            request = request.header("MCP-Protocol-Version", MODERN).header("Mcp-Method", method);
            let params = message.get("params");
            let name = params.and_then(|p| p.get("name").or_else(|| p.get("uri"))).and_then(Value::as_str);
            if let (true, Some(name)) = (matches!(method, "tools/call" | "resources/read" | "prompts/get"), name) {
                request = request.header("Mcp-Name", header_value(name));
            }
            for (name, value) in extra {
                request = request.header(format!("Mcp-Param-{name}"), value);
            }
        } else {
            if let Some(session) = lock(&shared.session).as_ref() {
                request = request.header("Mcp-Session-Id", session);
            }
            // The header arrived with 2025-06-18; older revisions don't know it.
            if let Some(version) = lock(&shared.version).as_deref().filter(|v| *v != "2024-11-05" && *v != "2025-03-26") {
                request = request.header("MCP-Protocol-Version", version);
            }
        }
        let response = request.send(message.to_string()).map_err(|e| Error::Transport(format!("The server could not be reached: {e}")))?;
        let status = response.status().as_u16();
        let header = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
        if let (Some(session), false) = (header("mcp-session-id"), modern) {
            *lock(&shared.session) = Some(session);
        }
        if status == 401 || (status == 403 && header("www-authenticate").is_some_and(|c| c.contains("insufficient_scope"))) {
            return Err(Error::Auth { status, challenge: header("www-authenticate").unwrap_or_default() });
        }
        let content_type = header("content-type").unwrap_or_default().to_ascii_lowercase();
        let mut body = response.into_body().into_with_config().limit(MAX_MESSAGE).reader();
        if !(200..300).contains(&status) {
            let mut text = String::new();
            let _ = (&mut body).take(64 << 10).read_to_string(&mut text);
            // A JSON-RPC error body is the server talking MCP; let the caller see it.
            if let Some(reply) = serde_json::from_str::<Value>(&text).ok().filter(|v| v.get("error").is_some_and(|e| e.get("code").is_some())) {
                return Ok(Reply::Message(reply));
            }
            let text: String = text.trim().chars().take(300).collect();
            return Err(Error::Http { status, body: text });
        }
        let Some(id) = id else { return Ok(Reply::Nothing) };
        if content_type.starts_with("text/event-stream") {
            let mut lines = BufReader::new(body);
            let mut decoder = serechat::SseDecoder::default();
            let mut line = String::new();
            loop {
                line.clear();
                match (&mut lines).take(MAX_MESSAGE).read_line(&mut line) {
                    Ok(0) => return Ok(Reply::Nothing),
                    Ok(_) => {}
                    Err(e) => return Err(Error::Transport(format!("The server's stream broke: {e}"))),
                }
                let Some(event) = decoder.line(line.trim_end_matches('\n')) else { continue };
                let Ok(message) = serde_json::from_str::<Value>(&event.data) else { continue };
                if message.get("id").and_then(Value::as_u64) == Some(id) && message.get("method").is_none() {
                    return Ok(Reply::Message(message));
                }
                if message.get("method").is_some() && message.get("id").is_some() {
                    // A legacy server asking us something mid-request.
                    let _ = self.post(&answer(&message), modern, &[], None);
                }
            }
        }
        let mut text = Vec::new();
        body.read_to_end(&mut text).map_err(|e| Error::Transport(format!("The server's reply broke off: {e}")))?;
        if text.iter().all(u8::is_ascii_whitespace) {
            return Ok(Reply::Nothing);
        }
        let value: Value = serde_json::from_slice(&text).map_err(|_| Error::Transport("The server's reply is not JSON.".into()))?;
        // Some legacy servers answer with a one-element batch.
        let reply = match value {
            Value::Array(items) => items.into_iter().find(|m| m.get("id").and_then(Value::as_u64) == Some(id)).unwrap_or(Value::Null),
            other => other,
        };
        Ok(if reply.is_null() { Reply::Nothing } else { Reply::Message(reply) })
    }

    /// Ends a legacy session, as the server asked to be told.
    fn end_session(&self) {
        let Some(session) = lock(&self.0.session).take() else { return };
        let (url, token) = (self.0.url.clone(), lock(&self.0.token).clone());
        spawn_thread("serechat-mcp-close", move || {
            let mut request = agent().delete(&url).header("Mcp-Session-Id", &session);
            if let Some(token) = token {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            let _ = request.call();
        });
    }
}

/// `value` as an HTTP header value: as is when it is plain visible ASCII
/// without surrounding spaces, otherwise in the `=?base64?…?=` sentinel form.
#[must_use]
pub fn header_value(value: &str) -> String {
    let plain = value.bytes().all(|b| b == b' ' || b == b'\t' || (0x21..=0x7E).contains(&b));
    let sentinel = value.starts_with("=?base64?") && value.ends_with("?=");
    if plain && !sentinel && value.trim() == value {
        value.to_owned()
    } else {
        format!("=?base64?{}?=", serechat::base64(value.as_bytes(), false))
    }
}

/// The deprecated HTTP+SSE transport (2024-11-05): replies arrive on a GET
/// event stream; requests are posted to the endpoint its first event names.
struct Sse {
    pipe: Arc<Pipe>,
    token: Arc<Mutex<Option<String>>>,
}

impl Sse {
    fn open(url: &str, headers: Vec<(String, String)>, token: Option<String>, notify: &OnNotify, cancel: &AtomicBool) -> Result<Self, Error> {
        let mut request = agent().get(url).header("Accept", "text/event-stream");
        for (name, value) in &headers {
            request = request.header(name, value);
        }
        if let Some(token) = &token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let response = request.call().map_err(|e| Error::Transport(format!("The server could not be reached: {e}")))?;
        let status = response.status().as_u16();
        let header = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
        if status == 401 || status == 403 {
            return Err(Error::Auth { status, challenge: header("www-authenticate").unwrap_or_default() });
        }
        if !(200..300).contains(&status) || !header("content-type").unwrap_or_default().starts_with("text/event-stream") {
            return Err(Error::Http { status, body: "It does not speak MCP over HTTP.".into() });
        }
        let token = Arc::new(Mutex::new(token));
        let endpoint: Arc<Mutex<Option<String>>> = Arc::default();
        let (post_url, post_token) = (Arc::clone(&endpoint), Arc::clone(&token));
        let sender = Box::new(move |message: &Value| -> Result<(), String> {
            let url = lock(&post_url).clone().ok_or("The server has not said where to send messages.")?;
            let mut request = agent().post(&url).header("Content-Type", "application/json");
            for (name, value) in &headers {
                request = request.header(name, value);
            }
            if let Some(token) = lock(&post_token).as_ref() {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            match request.send(message.to_string()) {
                Ok(response) if response.status().is_success() => Ok(()),
                Ok(response) => Err(format!("The server answered HTTP {}.", response.status().as_u16())),
                Err(e) => Err(format!("The server could not be reached: {e}")),
            }
        });
        let pipe = Arc::new(Pipe::new(sender, Arc::clone(notify)));
        let (ready_tx, ready_rx) = mpsc::channel();
        let (reader, base) = (Arc::clone(&pipe), url.to_owned());
        let body = response.into_body().into_with_config().limit(u64::MAX).reader();
        spawn_thread("serechat-mcp-sse", move || {
            let mut lines = BufReader::new(body);
            let mut decoder = serechat::SseDecoder::default();
            let mut line = String::new();
            loop {
                line.clear();
                match (&mut lines).take(MAX_MESSAGE).read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let Some(event) = decoder.line(line.trim_end_matches('\n')) else { continue };
                if event.name == "endpoint" {
                    *lock(&endpoint) = Some(join_url(&base, event.data.trim()));
                    let _ = ready_tx.send(());
                } else if let Ok(message) = serde_json::from_str::<Value>(&event.data) {
                    reader.dispatch(message);
                }
            }
            reader.close("The server closed its event stream.");
        });
        wait(&ready_rx, Duration::from_secs(30), cancel).map_err(|e| match e {
            Error::Timeout => Error::Transport("The server's event stream never said where to send messages.".into()),
            other => other,
        })?;
        Ok(Self { pipe, token })
    }
}

/// Resolves `reference` (absolute, or a path) against `base`.
fn join_url(base: &str, reference: &str) -> String {
    if reference.starts_with("http://") || reference.starts_with("https://") {
        return reference.to_owned();
    }
    let scheme_end = base.find("://").map_or(0, |i| i + 3);
    let origin_end = base[scheme_end..].find('/').map_or(base.len(), |i| scheme_end + i);
    if reference.starts_with('/') {
        format!("{}{reference}", &base[..origin_end])
    } else {
        let dir_end = base[..base.find(['?', '#']).unwrap_or(base.len())].rfind('/').filter(|&i| i >= origin_end).map_or(base.len(), |i| i + 1);
        let dir = &base[..dir_end];
        if dir.ends_with('/') { format!("{dir}{reference}") } else { format!("{dir}/{reference}") }
    }
}

/// The values of the arguments a tool's schema marks with `x-mcp-header`,
/// as `(name, encoded value)` header pairs, or why the schema's annotations
/// are invalid (then the tool must not be offered).
///
/// # Errors
/// An annotation in a place the spec forbids, a bad header name, or a
/// duplicate name.
pub fn param_headers(schema: &Value, arguments: &Value) -> Result<Vec<(String, String)>, String> {
    let mut found = Vec::new();
    collect_headers(schema, &mut Vec::new(), &mut found, 0)?;
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for (path, name, kind) in found {
        if !super::config::is_token(&name) {
            return Err(format!("x-mcp-header \"{name}\" is not a valid header name"));
        }
        if seen.iter().any(|s| s.eq_ignore_ascii_case(&name)) {
            return Err(format!("x-mcp-header \"{name}\" is used twice"));
        }
        seen.push(name.clone());
        if !matches!(kind.as_str(), "string" | "integer" | "boolean") {
            return Err(format!("x-mcp-header \"{name}\" is on a {kind} parameter"));
        }
        let value = path.iter().try_fold(arguments, |v, key| v.get(key));
        let text = match value {
            Some(Value::String(s)) => header_value(s),
            Some(Value::Bool(b)) => b.to_string(),
            Some(Value::Number(n)) => n.to_string(),
            // Absent, null, or not a primitive: no header.
            _ => continue,
        };
        out.push((name, text));
    }
    Ok(out)
}

/// Finds `x-mcp-header` annotations under `schema`, recording each one's
/// property path, header name and type. Annotations reachable only through
/// anything but `properties` are invalid.
fn collect_headers(schema: &Value, path: &mut Vec<String>, found: &mut Vec<(Vec<String>, String, String)>, depth: usize) -> Result<(), String> {
    if depth > 32 {
        return Ok(());
    }
    let Value::Object(map) = schema else { return Ok(()) };
    for (key, value) in map {
        match key.as_str() {
            "x-mcp-header" if path.is_empty() => return Err("x-mcp-header on the schema root".into()),
            "x-mcp-header" => {
                let name = value.as_str().ok_or("x-mcp-header must be a string")?.to_owned();
                let kind = map.get("type").and_then(Value::as_str).unwrap_or("missing").to_owned();
                found.push((path.clone(), name, kind));
            }
            "properties" => {
                if let Value::Object(properties) = value {
                    for (name, property) in properties {
                        path.push(name.clone());
                        collect_headers(property, path, found, depth + 1)?;
                        path.pop();
                    }
                }
            }
            _ if contains_header(value, 0) => return Err(format!("x-mcp-header inside \"{key}\"")),
            _ => {}
        }
    }
    Ok(())
}

/// Whether `value` holds an `x-mcp-header` key anywhere.
fn contains_header(value: &Value, depth: usize) -> bool {
    depth < 32
        && match value {
            Value::Object(map) => map.iter().any(|(k, v)| k == "x-mcp-header" || contains_header(v, depth + 1)),
            Value::Array(items) => items.iter().any(|v| contains_header(v, depth + 1)),
            _ => false,
        }
}

/// Lists every tool, following `nextCursor` (at most 100 pages).
///
/// # Errors
/// See [`Connection::request`].
pub fn list_tools(connection: &Connection, cancel: &AtomicBool) -> Result<Vec<Value>, Error> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..100 {
        let params = cursor.as_ref().map_or_else(|| json!({}), |c| json!({ "cursor": c }));
        let result = connection.request("tools/list", params, &[], SETUP_TIMEOUT, cancel)?;
        tools.extend(result.get("tools").and_then(Value::as_array).cloned().unwrap_or_default());
        cursor = result.get("nextCursor").and_then(Value::as_str).filter(|c| !c.is_empty()).map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    Ok(tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_values_and_urls() {
        assert_eq!(header_value("us-west1"), "us-west1");
        assert_eq!(header_value("Hello, 世界"), "=?base64?SGVsbG8sIOS4lueVjA==?=");
        assert_eq!(header_value(" padded "), "=?base64?IHBhZGRlZCA=?=");
        assert_eq!(header_value("line1\nline2"), "=?base64?bGluZTEKbGluZTI=?=");
        assert_eq!(header_value("=?base64?literal?="), "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?=");
        assert_eq!(join_url("https://a.dev/sse", "/messages?s=1"), "https://a.dev/messages?s=1");
        assert_eq!(join_url("https://a.dev/mcp/sse", "messages"), "https://a.dev/mcp/messages");
        assert_eq!(join_url("https://a.dev", "messages"), "https://a.dev/messages");
        assert_eq!(join_url("https://a.dev/sse", "https://b.dev/x"), "https://b.dev/x");
    }

    #[test]
    fn tool_arguments_become_headers() {
        let schema = json!({ "type": "object", "properties": {
            "region": { "type": "string", "x-mcp-header": "Region" },
            "opts": { "type": "object", "properties": { "dry": { "type": "boolean", "x-mcp-header": "Dry" } } },
            "query": { "type": "string" } } });
        let mut headers = param_headers(&schema, &json!({ "region": "us-west1", "opts": { "dry": true }, "query": "x" })).unwrap();
        headers.sort();
        assert_eq!(headers, [("Dry".to_owned(), "true".to_owned()), ("Region".to_owned(), "us-west1".to_owned())]);
        assert!(param_headers(&schema, &json!({ "query": "x" })).unwrap().is_empty(), "absent values send no header");
        let in_items = json!({ "type": "object", "properties": { "list": { "type": "array", "items": { "type": "string", "x-mcp-header": "X" } } } });
        assert!(param_headers(&in_items, &json!({})).is_err());
        let number = json!({ "type": "object", "properties": { "n": { "type": "number", "x-mcp-header": "N" } } });
        assert!(param_headers(&number, &json!({})).is_err());
        let twice = json!({ "type": "object", "properties": { "a": { "type": "string", "x-mcp-header": "X" }, "b": { "type": "string", "x-mcp-header": "x" } } });
        assert!(param_headers(&twice, &json!({})).is_err());
        let bad_name = json!({ "type": "object", "properties": { "a": { "type": "string", "x-mcp-header": "Bad Name" } } });
        assert!(param_headers(&bad_name, &json!({})).is_err());
    }

    use super::super::fake::{self, Reply};
    use std::sync::atomic::AtomicUsize;

    fn web(url: String) -> ServerConfig {
        ServerConfig::new("t".into(), Transport::Http { url, headers: vec![("X-Key".into(), "k".into())], sse: false })
    }

    fn reply(id: &Value, result: &Value) -> String {
        json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
    }

    #[test]
    fn modern_servers_are_spoken_to_statelessly() {
        let base = fake::serve(|r| -> Reply {
            let body = r.json();
            let method = body["method"].as_str().unwrap_or_default();
            // Every request carries the version, the method and identity, in headers and _meta.
            assert_eq!(r.header("mcp-protocol-version"), Some(MODERN));
            assert_eq!(r.header("mcp-method"), Some(method));
            assert_eq!(r.header("x-key"), Some("k"));
            assert_eq!(body["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"], MODERN);
            assert!(r.header("mcp-session-id").is_none());
            match method {
                "server/discover" => (200, vec![], reply(&body["id"], &json!({ "resultType": "complete", "supportedVersions": [MODERN], "capabilities": { "tools": {} }, "instructions": "Be nice." }))),
                "tools/list" => (200, vec![], reply(&body["id"], &json!({ "resultType": "complete", "tools": [{ "name": "echo", "inputSchema": { "type": "object" } }] }))),
                "tools/call" => {
                    assert_eq!(r.header("mcp-name"), Some("echo"));
                    assert_eq!(r.header("mcp-param-region"), Some("eu"));
                    // A progress notification, then the result, on an SSE stream.
                    let progress = json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": { "progress": 1 } });
                    let stream = format!(": keep-alive\n\ndata: {progress}\n\nevent: message\ndata: {}\n\n", reply(&body["id"], &json!({ "content": [{ "type": "text", "text": "hi" }] })));
                    (200, vec![("Content-Type", "text/event-stream".into())], stream)
                }
                _ => (404, vec![], json!({ "jsonrpc": "2.0", "id": body["id"], "error": { "code": -32601, "message": "nope" } }).to_string()),
            }
        });
        let notify: OnNotify = Arc::new(|_| {});
        let cancel = AtomicBool::new(false);
        let (connection, hello) = Connection::open(&web(format!("{base}/mcp")), None, &notify, &cancel).unwrap();
        assert_eq!((hello.version.as_str(), hello.instructions.as_deref(), hello.tools), (MODERN, Some("Be nice."), true));
        assert_eq!(list_tools(&connection, &cancel).unwrap().len(), 1);
        let headers = [("Region".to_owned(), "eu".to_owned())];
        let result = connection.request("tools/call", json!({ "name": "echo", "arguments": {} }), &headers, SETUP_TIMEOUT, &cancel).unwrap();
        assert_eq!(result["content"][0]["text"], "hi");
        let missing = connection.request("nothing/here", json!({}), &[], SETUP_TIMEOUT, &cancel).unwrap_err();
        assert!(matches!(missing, Error::Rpc { code: -32601, .. }));
    }

    #[test]
    fn legacy_servers_get_a_handshake_and_their_session() {
        let initialized = Arc::new(AtomicUsize::new(0));
        let lists = Arc::new(AtomicUsize::new(0));
        let (seen_init, seen_lists) = (Arc::clone(&initialized), Arc::clone(&lists));
        let base = fake::serve(move |r| -> Reply {
            let body = r.json();
            match body["method"].as_str().unwrap_or_default() {
                // A 2025 server knows nothing of discovery, and wants a session first.
                "server/discover" => (400, vec![], "Bad Request: No valid session ID provided".into()),
                "initialize" => {
                    // The newest legacy revision first; the negotiated one when starting over.
                    let asked = if seen_init.load(Ordering::SeqCst) == 0 { LEGACY[0] } else { "2025-06-18" };
                    assert_eq!(body["params"]["protocolVersion"], asked);
                    assert!(body["params"].get("_meta").is_none(), "no modern metadata in a legacy handshake");
                    let session = format!("s{}", seen_init.fetch_add(1, Ordering::SeqCst));
                    (200, vec![("Mcp-Session-Id", session)], reply(&body["id"], &json!({ "protocolVersion": "2025-06-18", "capabilities": { "tools": {} } })))
                }
                "notifications/initialized" => (202, vec![], String::new()),
                "tools/list" => {
                    assert_eq!(r.header("mcp-protocol-version"), Some("2025-06-18"));
                    assert!(r.header("mcp-method").is_none());
                    // The second listing finds its session expired.
                    if seen_lists.fetch_add(1, Ordering::SeqCst) == 1 {
                        assert_eq!(r.header("mcp-session-id"), Some("s0"));
                        return (404, vec![], "Session not found".into());
                    }
                    (200, vec![], reply(&body["id"], &json!({ "tools": [] })))
                }
                _ => (400, vec![], String::new()),
            }
        });
        let notify: OnNotify = Arc::new(|_| {});
        let cancel = AtomicBool::new(false);
        let (connection, hello) = Connection::open(&web(base), None, &notify, &cancel).unwrap();
        assert_eq!(hello.version, "2025-06-18");
        assert_eq!(list_tools(&connection, &cancel).unwrap().len(), 0);
        assert!(list_tools(&connection, &cancel).unwrap().is_empty(), "a lost session is started over");
        assert_eq!((initialized.load(Ordering::SeqCst), lists.load(Ordering::SeqCst)), (2, 3));
    }

    #[test]
    fn servers_that_want_a_token_say_where_to_get_one() {
        let base = fake::serve(|r| -> Reply {
            assert_eq!(r.header("authorization"), Some("Bearer old"));
            (401, vec![("WWW-Authenticate", r#"Bearer resource_metadata="http://127.0.0.1/.well-known/oauth-protected-resource", scope="read""#.into())], String::new())
        });
        let notify: OnNotify = Arc::new(|_| {});
        let result = Connection::open(&web(base), Some("old".into()), &notify, &AtomicBool::new(false));
        let Err(Error::Auth { status: 401, challenge }) = result else { panic!("expected a challenge") };
        assert_eq!(super::super::oauth::parse_challenge(&challenge).scope.as_deref(), Some("read"));
    }

    #[test]
    fn stdio_servers_that_vanish_fail_fast() {
        let config = ServerConfig::new("t".into(), Transport::Stdio { command: "serechat-no-such-program".into(), args: Vec::new(), env: Vec::new(), cwd: None });
        let notify: OnNotify = Arc::new(|_| {});
        let error = Connection::open(&config, None, &notify, &AtomicBool::new(false)).err().unwrap();
        assert!(error.to_string().contains("was not found"), "{error}");
        // A program that exits at once is reported with its exit, not a timeout.
        let (command, args) = if cfg!(windows) { ("cmd", vec!["/C".to_owned(), "echo boom 1>&2 & exit 3".to_owned()]) } else { ("sh", vec!["-c".to_owned(), "echo boom >&2; exit 3".to_owned()]) };
        let config = ServerConfig::new("t".into(), Transport::Stdio { command: command.into(), args, env: Vec::new(), cwd: None });
        let started = Instant::now();
        let error = Connection::open(&config, None, &notify, &AtomicBool::new(false)).err().unwrap();
        assert!(started.elapsed() < Duration::from_secs(10), "no waiting for a dead server");
        assert!(error.to_string().contains("code 3") && error.to_string().contains("boom"), "{error}");
    }

    #[test]
    fn meta_is_added_to_modern_requests() {
        let mut params = json!({ "name": "t" });
        add_meta(&mut params);
        assert_eq!(params["_meta"]["io.modelcontextprotocol/protocolVersion"], MODERN);
        assert_eq!(params["_meta"]["io.modelcontextprotocol/clientCapabilities"], json!({}));
        let mut empty = Value::Null;
        add_meta(&mut empty);
        assert!(empty["_meta"].is_object());
        assert_eq!(answer(&json!({ "id": 7, "method": "ping" })), json!({ "jsonrpc": "2.0", "id": 7, "result": {} }));
        assert_eq!(answer(&json!({ "id": "x", "method": "sampling/createMessage" }))["error"]["code"], -32601);
    }
}
