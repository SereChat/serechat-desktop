//! MCP: tools from Model Context Protocol servers the user connects, offered
//! to the model in every chat, project or not.
//!
//! The servers are configured in `~/.serechat/mcp.json` (see
//! [`config`]); the app hands the list to [`configure`], which connects to
//! each enabled server on a thread of its own and lists its tools. Their
//! tools reach the model as `mcp__<server>__<tool>`. A tool the server
//! marks read-only runs at once; any other waits for the user's approval,
//! like the agent's own tools.
//!
//! Servers on the web may need the user to sign in ([`sign_in`], OAuth in
//! the browser). Tokens live in `~/.serechat/mcp-auth.json`, are refreshed
//! when they expire, and are only ever sent to the server they were issued
//! for. Every change of state is reported through the listener set with
//! [`set_listener`], which the app turns into a redraw and, for tokens, a
//! write of the auth file.
//!
//! Calls block; the app runs them on worker threads.
//!
//! ponytail: tool lists refresh when a stdio or legacy server says they
//! changed, or on reconnect; a modern server's `subscriptions/listen`
//! stream is not opened. Resources and prompts are not offered.

pub mod config;
#[cfg(test)]
mod fake;
mod oauth;
mod transport;

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

pub use config::{AuthStore, ServerConfig, Transport};
use oauth::Challenge;
use transport::{Connection, Error, OnNotify};

use crate::tools::Output;

/// Prefix of every MCP tool's name.
const PREFIX: &str = "mcp__";
/// Longest tool name the model APIs accept.
const MAX_NAME: usize = 64;
/// Longest a tool call may run.
const CALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Longest description passed on for one tool.
const MAX_DESCRIPTION: usize = 4000;
/// Longest a server's instructions may be in the system prompt.
const MAX_INSTRUCTIONS: usize = 2000;

/// What a server is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Disabled in the settings.
    Off,
    /// Starting or connecting.
    Connecting,
    /// Connected; its tools are offered.
    Ready,
    /// It wants the user to sign in; why, if something went wrong.
    NeedsSignIn(Option<String>),
    /// Waiting for the user to finish signing in, in the browser.
    SigningIn,
    /// It failed; why.
    Failed(String),
}

/// A server as the settings page shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerView {
    /// Its name.
    pub name: String,
    /// The command it runs or its URL.
    pub target: String,
    /// Whether the app connects to it.
    pub enabled: bool,
    /// Whether it is on the web (so it may need signing in).
    pub web: bool,
    /// What it is doing.
    pub status: Status,
    /// The protocol revision it speaks, once connected.
    pub version: String,
    /// Its tools: name and the first line of the description.
    pub tools: Vec<(String, String)>,
    /// Whether the app holds a token for it.
    pub signed_in: bool,
    /// Whether it offers signing in while already connected (some servers
    /// list their tools to anyone but want a sign-in to run them).
    pub takes_sign_in: bool,
    /// Tools left out, and why.
    pub warnings: Vec<String>,
}

/// A tool as the model is offered it.
pub struct ToolDef {
    /// `mcp__server__tool`.
    pub name: String,
    /// What it does, with the server named.
    pub description: String,
    /// JSON Schema of its arguments.
    pub parameters: Value,
}

/// Something the app should know about.
pub enum Event {
    /// A server's state or tools changed: redraw, and requests offer the new tools.
    Changed,
    /// The OAuth store changed; holds the new `mcp-auth.json`.
    AuthChanged(String),
}

/// One of a server's tools.
#[derive(Clone)]
struct RemoteTool {
    /// Name offered to the model.
    exposed: String,
    /// Name at the server.
    name: String,
    description: String,
    schema: Value,
    read_only: bool,
}

struct Server {
    config: ServerConfig,
    status: Status,
    version: String,
    connection: Option<Arc<Connection>>,
    tools: Vec<RemoteTool>,
    warnings: Vec<String>,
    instructions: Option<String>,
    /// Bumped on every (re)connect, so a stale connection attempt is dropped.
    generation: u64,
    /// The server's latest `WWW-Authenticate` challenge.
    challenge: Challenge,
    /// It let us in without a token but publishes sign-in metadata.
    takes_sign_in: bool,
    /// Raised to stop the current connection attempt or sign-in.
    stop: Arc<AtomicBool>,
}

type Listener = Arc<dyn Fn(Event) + Send + Sync>;

#[derive(Default)]
struct State {
    servers: Vec<Server>,
    auth: AuthStore,
    listener: Option<Listener>,
    next_generation: u64,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
/// Held while refreshing a token, so two calls never spend one refresh token.
static REFRESH: Mutex<()> = Mutex::new(());

/// Runs `f` on the state.
fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut guard = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    f(guard.get_or_insert_with(State::default))
}

fn lock_refresh() -> MutexGuard<'static, ()> {
    REFRESH.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Tells the listener about `event`, outside the state lock.
fn emit(event: Event) {
    if let Some(listener) = with(|s| s.listener.clone()) {
        listener(event);
    }
}

/// Reports the auth store's new contents.
fn auth_changed() {
    let text = with(|s| s.auth.serialize());
    emit(Event::AuthChanged(text));
}

/// Sets who hears about changes.
pub fn set_listener(listener: impl Fn(Event) + Send + Sync + 'static) {
    with(|s| s.listener = Some(Arc::new(listener)));
}

/// Loads the OAuth store (the text of `mcp-auth.json`).
pub fn load_auth(text: &str) {
    with(|s| s.auth = AuthStore::parse(text));
}

/// Makes `configs` the servers: new and changed ones connect, removed ones
/// disconnect (and forget their tokens), unchanged ones carry on.
pub fn configure(configs: &[ServerConfig]) {
    let mut start = Vec::new();
    let mut closed = Vec::new();
    let forgot = with(|state| {
        let mut old = std::mem::take(&mut state.servers);
        for config in configs {
            match old.iter().position(|s| s.config.name == config.name) {
                Some(i) if old[i].config == *config => state.servers.push(old.remove(i)),
                found => {
                    if let Some(i) = found {
                        let server = old.remove(i);
                        server.stop.store(true, Ordering::Relaxed);
                        closed.extend(server.connection);
                    }
                    state.next_generation += 1;
                    let status = if config.enabled { Status::Connecting } else { Status::Off };
                    if config.enabled {
                        start.push((config.name.clone(), state.next_generation));
                    }
                    state.servers.push(Server {
                        config: config.clone(),
                        status,
                        version: String::new(),
                        connection: None,
                        tools: Vec::new(),
                        warnings: Vec::new(),
                        instructions: None,
                        generation: state.next_generation,
                        challenge: Challenge::default(),
                        takes_sign_in: false,
                        stop: Arc::default(),
                    });
                }
            }
        }
        let mut forgot = false;
        for server in old {
            server.stop.store(true, Ordering::Relaxed);
            closed.extend(server.connection);
            forgot |= state.auth.tokens.remove(&server.config.name).is_some();
        }
        forgot
    });
    for connection in closed {
        close(&connection);
    }
    if forgot {
        auth_changed();
    }
    for (name, generation) in start {
        spawn_connect(name, generation);
    }
    emit(Event::Changed);
}

/// Closes a connection: calls still running on it fail, and a stdio server
/// is ended once the last of them lets go.
fn close(connection: &Connection) {
    connection.release();
}

/// Connects to server `name` again (after an error, or by request).
pub fn reconnect(name: &str) {
    let restart = with(|state| {
        state.next_generation += 1;
        let generation = state.next_generation;
        let server = state.servers.iter_mut().find(|s| s.config.name == name && s.config.enabled)?;
        server.stop.store(true, Ordering::Relaxed);
        server.stop = Arc::default();
        server.generation = generation;
        server.status = Status::Connecting;
        server.tools.clear();
        Some((server.connection.take(), generation))
    });
    if let Some((old, generation)) = restart {
        if let Some(old) = old {
            close(&old);
        }
        spawn_connect(name.to_owned(), generation);
        emit(Event::Changed);
    }
}

/// Connects on a thread of its own.
fn spawn_connect(name: String, generation: u64) {
    let spawned = std::thread::Builder::new().name("serechat-mcp-connect".into()).spawn(move || {
        connect(&name, generation);
        emit(Event::Changed);
    });
    if let Err(e) = spawned {
        eprintln!("serechat: cannot start an MCP connection: {e}");
    }
}

/// Connects to server `name` and lists its tools, unless a newer attempt
/// (`generation`) took over. Blocks.
fn connect(name: &str, generation: u64) {
    let Some((config, stop)) = with(|s| s.servers.iter().find(|s| s.config.name == name && s.generation == generation).map(|s| (s.config.clone(), Arc::clone(&s.stop)))) else {
        return;
    };
    let token = fresh_token(name, &config);
    let notify: OnNotify = {
        let name = name.to_owned();
        Arc::new(move |method: &str| {
            if method == "notifications/tools/list_changed" {
                let name = name.clone();
                let _ = std::thread::Builder::new().name("serechat-mcp-tools".into()).spawn(move || refresh_tools(&name, generation));
            }
        })
    };
    let mut attempt = Connection::open(&config, token.clone(), &notify, &stop);
    // An expired or revoked token: refresh it once and try again.
    if let (Err(Error::Auth { .. }), Some(_)) = (&attempt, &token)
        && let Some(token) = refresh_token(name, &config)
    {
        attempt = Connection::open(&config, Some(token), &notify, &stop);
    }
    // A server that lets us in without a token may still take sign-ins
    // for its tools (Google's do): its published metadata says so.
    let takes_sign_in = match (&attempt, &token, &config.transport) {
        (Ok(_), None, Transport::Http { url, .. }) => oauth::protected(&config::expand(url)),
        _ => false,
    };
    let result = attempt.and_then(|(connection, hello)| {
        let tools = if hello.tools { transport::list_tools(&connection, &stop) } else { Ok(Vec::new()) };
        match tools {
            Ok(tools) => Ok((connection, hello, tools)),
            Err(e) => {
                drop(connection);
                Err(e)
            }
        }
    });
    let mut leftover = None;
    with(|state| {
        let Some(server) = state.servers.iter_mut().find(|s| s.config.name == name && s.generation == generation) else {
            leftover = result.ok().map(|(connection, ..)| connection);
            return;
        };
        match result {
            Ok((connection, hello, tools)) => {
                let (tools, warnings) = offered_tools(name, &tools, connection.is_http());
                server.tools = tools;
                server.warnings = warnings;
                server.instructions = hello.instructions;
                server.version = hello.version;
                server.connection = Some(Arc::new(connection));
                server.status = Status::Ready;
                server.takes_sign_in = takes_sign_in;
            }
            Err(Error::Auth { challenge, .. }) => {
                server.challenge = oauth::parse_challenge(&challenge);
                server.status = Status::NeedsSignIn(None);
            }
            Err(Error::Cancelled) => {}
            Err(e) => server.status = Status::Failed(e.to_string()),
        }
    });
    if let Some(connection) = leftover {
        drop(connection);
    }
}

/// Lists server `name`'s tools again, after it said they changed.
fn refresh_tools(name: &str, generation: u64) {
    let Some((connection, stop)) = with(|s| {
        s.servers.iter().find(|s| s.config.name == name && s.generation == generation).and_then(|s| Some((Arc::clone(s.connection.as_ref()?), Arc::clone(&s.stop))))
    }) else {
        return;
    };
    let Ok(tools) = transport::list_tools(&connection, &stop) else { return };
    let (tools, warnings) = offered_tools(name, &tools, connection.is_http());
    with(|s| {
        if let Some(server) = s.servers.iter_mut().find(|s| s.config.name == name && s.generation == generation) {
            server.tools = tools;
            server.warnings = warnings;
        }
    });
    emit(Event::Changed);
}

/// The tools of server `name` the model may be offered, and warnings for
/// those left out.
fn offered_tools(server: &str, tools: &[Value], http: bool) -> (Vec<RemoteTool>, Vec<String>) {
    let mut offered: Vec<RemoteTool> = Vec::new();
    let mut warnings = Vec::new();
    for tool in tools {
        let Some(name) = tool.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()) else { continue };
        let mut schema = tool.get("inputSchema").cloned().filter(Value::is_object).unwrap_or_else(|| json!({ "type": "object" }));
        // Over HTTP, arguments marked for headers must be marked correctly.
        if http && let Err(why) = transport::param_headers(&schema, &json!({})) {
            warnings.push(format!("{name} was left out: {why}."));
            continue;
        }
        if let Value::Object(map) = &mut schema {
            map.remove("$schema");
            map.entry("type").or_insert_with(|| "object".into());
            map.entry("properties").or_insert_with(|| json!({}));
        }
        let title = tool.get("title").or_else(|| tool.get("annotations").and_then(|a| a.get("title"))).and_then(Value::as_str);
        let mut description = tool.get("description").and_then(Value::as_str).or(title).unwrap_or_default().trim().to_owned();
        if description.len() > MAX_DESCRIPTION {
            let mut cut = MAX_DESCRIPTION;
            while !description.is_char_boundary(cut) {
                cut -= 1;
            }
            description.truncate(cut);
            description.push('…');
        }
        let read_only = tool.get("annotations").and_then(|a| a.get("readOnlyHint")).and_then(Value::as_bool) == Some(true);
        let mut exposed = exposed_name(server, name);
        if offered.iter().any(|t| t.exposed == exposed) {
            exposed = shorten(&exposed, &format!("{server}\u{0}{name}"), MAX_NAME - 7);
        }
        offered.push(RemoteTool { exposed, name: name.to_owned(), description, schema, read_only });
    }
    (offered, warnings)
}

/// `mcp__<server>__<tool>`, with characters model APIs refuse replaced and
/// long names shortened (a hash keeps them apart).
fn exposed_name(server: &str, tool: &str) -> String {
    let clean = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect::<String>();
    let full = format!("{PREFIX}{}__{}", clean(server), clean(tool));
    if full.len() <= MAX_NAME { full } else { shorten(&full, &format!("{server}\u{0}{tool}"), MAX_NAME - 7) }
}

/// The first `keep` bytes of `name` (ASCII) and a hash of `key`.
fn shorten(name: &str, key: &str, keep: usize) -> String {
    let hash = crate::sha256::hex(&crate::sha256::digest(key.as_bytes()));
    format!("{}_{}", &name[..keep.min(name.len())], &hash[..6])
}

/// Whether `name` is an MCP tool's.
#[must_use]
pub fn is_mcp(name: &str) -> bool {
    name.starts_with(PREFIX)
}

/// Looks at an offered tool (and its server) by the name the model knows.
/// Cheap: the UI asks every frame.
fn lookup<R>(name: &str, f: impl FnOnce(&Server, &RemoteTool) -> R) -> Option<R> {
    with(|s| s.servers.iter().find_map(|server| server.tools.iter().find(|t| t.exposed == name).map(|tool| (server, tool))).map(|(server, tool)| f(server, tool)))
}

/// An offered tool by the name the model knows, with its server's name.
fn find(name: &str) -> Option<(String, RemoteTool)> {
    lookup(name, |server, tool| (server.config.name.clone(), tool.clone()))
}

/// Whether a call to `name` waits for the user: every MCP tool does unless
/// its server marks it read-only (and is connected to say so).
#[must_use]
pub fn needs_approval(name: &str) -> bool {
    lookup(name, |_, tool| !tool.read_only).unwrap_or(true)
}

/// The server and tool behind `name`, for showing a call. Falls back to
/// reading the name when the server is not connected.
#[must_use]
pub fn label(name: &str) -> (String, String) {
    lookup(name, |server, tool| (server.config.name.clone(), tool.name.clone())).unwrap_or_else(|| {
        let rest = name.strip_prefix(PREFIX).unwrap_or(name);
        rest.split_once("__").map_or((String::new(), rest.to_owned()), |(s, t)| (s.to_owned(), t.to_owned()))
    })
}

/// The tools of every connected server, in a stable order.
#[must_use]
pub fn tools() -> Vec<ToolDef> {
    with(|state| {
        let mut servers: Vec<&Server> = state.servers.iter().filter(|s| s.status == Status::Ready).collect();
        servers.sort_by(|a, b| a.config.name.cmp(&b.config.name));
        servers
            .into_iter()
            .flat_map(|server| {
                server.tools.iter().map(move |tool| ToolDef {
                    name: tool.exposed.clone(),
                    description: if tool.description.is_empty() {
                        format!("{} (from the {} MCP server)", tool.name, server.config.name)
                    } else {
                        format!("{} (from the {} MCP server)", tool.description, server.config.name)
                    },
                    parameters: tool.schema.clone(),
                })
            })
            .collect()
    })
}

/// What connected servers ask the model to know, for the system prompt;
/// empty when none say anything.
#[must_use]
pub fn instructions() -> String {
    with(|state| {
        let mut out = String::new();
        let mut servers: Vec<&Server> = state.servers.iter().filter(|s| s.status == Status::Ready && s.instructions.is_some()).collect();
        servers.sort_by(|a, b| a.config.name.cmp(&b.config.name));
        for server in servers {
            let text = server.instructions.as_deref().unwrap_or_default().trim();
            let text: String = text.chars().take(MAX_INSTRUCTIONS).collect();
            let _ = write!(out, "\n\n## {} (tools named mcp__{}__…)\n\n{text}", server.config.name, server.config.name);
        }
        if out.is_empty() { out } else { format!("\n\n# MCP servers\n\nThe user connected these Model Context Protocol servers; their notes on using them follow.{out}") }
    })
}

/// The servers, for the settings page.
#[must_use]
pub fn views() -> Vec<ServerView> {
    with(|state| {
        state
            .servers
            .iter()
            .map(|server| {
                let web = matches!(server.config.transport, Transport::Http { .. });
                let signed_in = token_for(&state.auth, &server.config).is_some();
                ServerView {
                    name: server.config.name.clone(),
                    target: server.config.target(),
                    enabled: server.config.enabled,
                    web,
                    status: server.status.clone(),
                    version: server.version.clone(),
                    tools: server.tools.iter().map(|t| (t.name.clone(), t.description.lines().next().unwrap_or_default().to_owned())).collect(),
                    signed_in,
                    takes_sign_in: server.takes_sign_in,
                    warnings: server.warnings.clone(),
                }
            })
            .collect()
    })
}

/// The stored access token for `config`'s server, if it was issued for its URL.
fn token_for(auth: &AuthStore, config: &ServerConfig) -> Option<config::Tokens> {
    let Transport::Http { url, .. } = &config.transport else { return None };
    auth.tokens.get(&config.name).filter(|t| t.url == *url).cloned()
}

/// A usable access token for server `name`: the stored one, refreshed first
/// if it is about to expire.
fn fresh_token(name: &str, config: &ServerConfig) -> Option<String> {
    let tokens = with(|s| token_for(&s.auth, config))?;
    let expiring = tokens.expires_at.is_some_and(|at| at <= serechat::unix_now() + 60);
    if expiring && tokens.refresh_token.is_some() {
        return refresh_token(name, config).or(Some(tokens.access_token));
    }
    Some(tokens.access_token)
}

/// Refreshes server `name`'s token; `None` if it cannot be refreshed. A
/// refused refresh forgets the tokens, so the user is asked to sign in.
fn refresh_token(name: &str, config: &ServerConfig) -> Option<String> {
    let _turn = lock_refresh();
    let tokens = with(|s| token_for(&s.auth, config))?;
    // Another call may have refreshed while this one waited its turn.
    if tokens.expires_at.is_some_and(|at| at > serechat::unix_now() + 60) && tokens.refresh_token.is_some() {
        return Some(tokens.access_token);
    }
    match oauth::refresh(&tokens) {
        Ok(new) => {
            let token = new.access_token.clone();
            with(|s| s.auth.tokens.insert(name.to_owned(), new));
            auth_changed();
            Some(token)
        }
        Err(oauth::RefreshError::Rejected(_)) => {
            with(|s| s.auth.tokens.remove(name));
            auth_changed();
            None
        }
        Err(oauth::RefreshError::Unreachable(_)) => None,
    }
}

/// Signs in to server `name` in the browser, then connects. Blocks until
/// the user finishes or [`cancel_sign_in`] is called.
///
/// # Errors
/// Why signing in failed, for the user.
pub fn sign_in(name: &str) -> Result<(), String> {
    let prepared = with(|state| {
        let server = state.servers.iter_mut().find(|s| s.config.name == name)?;
        server.stop.store(true, Ordering::Relaxed);
        server.stop = Arc::default();
        server.status = Status::SigningIn;
        let previous = token_for(&state.auth, &server.config);
        Some((server.config.clone(), server.challenge.clone(), previous, state.auth.clients.clone(), Arc::clone(&server.stop)))
    });
    let Some((config, challenge, previous, registered, cancel)) = prepared else { return Err(format!("There is no server named {name}.")) };
    let Transport::Http { url, .. } = &config.transport else { return Err("Only servers on the web sign in.".into()) };
    emit(Event::Changed);
    let url = config::expand(url);
    let request = oauth::Request { url: &url, client: &config.oauth, challenge: &challenge, previous: previous.as_ref(), registered: &registered };
    let result = oauth::sign_in(&request, &cancel);
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(e) => {
            let cancelled = cancel.load(Ordering::Relaxed);
            with(|s| {
                if let Some(server) = s.servers.iter_mut().find(|s| s.config.name == name && s.status == Status::SigningIn) {
                    server.status = Status::NeedsSignIn((!cancelled).then(|| e.clone()));
                }
            });
            emit(Event::Changed);
            return Err(e);
        }
    };
    with(|state| {
        let mut tokens = outcome.tokens;
        // Stored under the URL as configured, so a changed URL drops them.
        if let Transport::Http { url, .. } = &config.transport {
            tokens.url.clone_from(url);
        }
        state.auth.tokens.insert(name.to_owned(), tokens);
        if let Some((issuer, client)) = outcome.registered {
            state.auth.clients.insert(issuer, client);
        }
    });
    auth_changed();
    reconnect(name);
    Ok(())
}

/// Stops a sign-in that is waiting for the browser.
pub fn cancel_sign_in(name: &str) {
    with(|s| {
        if let Some(server) = s.servers.iter().find(|s| s.config.name == name) {
            server.stop.store(true, Ordering::Relaxed);
        }
    });
}

/// Forgets server `name`'s tokens and reconnects without them.
pub fn sign_out(name: &str) {
    let removed = with(|s| s.auth.tokens.remove(name).is_some());
    if removed {
        auth_changed();
    }
    reconnect(name);
}

/// Calls MCP tool `name` with `arguments` (JSON). Reconnects first to a
/// server that dropped.
///
/// # Errors
/// A message for the model: an unknown tool, a server that is not
/// connected or wants signing in, or the tool's own error.
pub fn call(name: &str, arguments: &str, cancel: &AtomicBool) -> Result<Output, String> {
    let (server, tool) = find(name).ok_or_else(|| {
        let (server, _) = label(name);
        format!("The {server} MCP server is not connected, so {name} is unavailable. The user can check it in Settings → MCP.")
    })?;
    let arguments: Value = if arguments.trim().is_empty() { json!({}) } else { serde_json::from_str(arguments).map_err(|e| format!("The arguments are not valid JSON: {e}"))? };
    if !arguments.is_object() {
        return Err("Arguments must be a JSON object.".into());
    }
    let connection = connection_of(&server)?;
    let headers = if connection.is_http() { transport::param_headers(&tool.schema, &arguments)? } else { Vec::new() };
    let params = json!({ "name": tool.name, "arguments": arguments });
    let mut result = connection.request("tools/call", params.clone(), &headers, CALL_TIMEOUT, cancel);
    if let Err(Error::Auth { status: 401, .. }) = &result
        && let Some(config) = with(|s| s.servers.iter().find(|s| s.config.name == server).map(|s| s.config.clone()))
        && let Some(token) = refresh_token(&server, &config)
    {
        connection.set_token(Some(token));
        result = connection.request("tools/call", params, &headers, CALL_TIMEOUT, cancel);
    }
    match result {
        Ok(result) => format_result(&result),
        Err(Error::Auth { challenge, .. }) => {
            with(|s| {
                if let Some(entry) = s.servers.iter_mut().find(|s| s.config.name == server) {
                    entry.challenge = oauth::parse_challenge(&challenge);
                    entry.status = Status::NeedsSignIn(None);
                }
            });
            emit(Event::Changed);
            Err(format!("The {server} MCP server needs the user to sign in (Settings → MCP). Once they have, try again."))
        }
        Err(Error::Closed(why)) => {
            with(|s| {
                if let Some(entry) = s.servers.iter_mut().find(|s| s.config.name == server && s.connection.as_ref().is_some_and(|c| Arc::ptr_eq(c, &connection))) {
                    entry.status = Status::Failed(why.clone());
                }
            });
            emit(Event::Changed);
            Err(format!("The {server} MCP server stopped: {why}"))
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Server `name`'s live connection, reconnecting (on this thread) if it
/// dropped.
fn connection_of(name: &str) -> Result<Arc<Connection>, String> {
    let current = with(|s| s.servers.iter().find(|s| s.config.name == name).map(|s| (s.connection.clone(), s.generation, s.status.clone())));
    let Some((connection, generation, status)) = current else { return Err(format!("There is no MCP server named {name}.")) };
    if let Some(connection) = connection.filter(|c| c.alive()) {
        return Ok(connection);
    }
    if !matches!(status, Status::Ready | Status::Failed(_)) {
        return Err(format!("The {name} MCP server is not connected."));
    }
    // A server that crashed is started again, once per call.
    connect(name, generation);
    emit(Event::Changed);
    with(|s| s.servers.iter().find(|s| s.config.name == name).and_then(|s| s.connection.clone()))
        .filter(|c| c.alive())
        .ok_or_else(|| format!("The {name} MCP server could not be restarted."))
}

/// A `tools/call` result as tool output: its text, the first image, and
/// its error flag.
fn format_result(result: &Value) -> Result<Output, String> {
    let mut text = Vec::new();
    let mut image = None;
    for item in result.get("content").and_then(Value::as_array).into_iter().flatten() {
        let field = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or_default();
        match field("type") {
            "text" => text.push(field("text").to_owned()),
            "image" => {
                let mime = field("mimeType");
                let decoded = crate::browser::base64_decode(field("data").trim());
                match decoded {
                    Some(bytes) if image.is_none() && matches!(mime, "image/png" | "image/jpeg" | "image/gif" | "image/webp") => {
                        text.push(format!("[An image ({mime}) is attached below.]"));
                        image = Some((mime.to_owned(), bytes));
                    }
                    _ => text.push(format!("[An image ({mime}) was left out.]")),
                }
            }
            "audio" => text.push(format!("[Audio ({}) was left out.]", field("mimeType"))),
            "resource_link" => {
                let mut line = format!("Resource: {} <{}>", field("name"), field("uri"));
                if !field("description").is_empty() {
                    let _ = write!(line, " — {}", field("description"));
                }
                text.push(line);
            }
            "resource" => {
                let resource = item.get("resource").unwrap_or(&Value::Null);
                let get = |key: &str| resource.get(key).and_then(Value::as_str).unwrap_or_default();
                if resource.get("text").is_some() {
                    text.push(format!("<resource uri=\"{}\">\n{}\n</resource>", get("uri"), get("text")));
                } else {
                    text.push(format!("[Binary resource {} ({}) was left out.]", get("uri"), get("mimeType")));
                }
            }
            _ => text.push(item.to_string()),
        }
    }
    if text.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        text.push(serde_json::to_string_pretty(structured).unwrap_or_default());
    }
    let mut text = text.join("\n\n");
    if text.trim().is_empty() {
        text = "(no output)".into();
    }
    let text = crate::tools::truncate_middle(text, crate::tools::MAX_OUTPUT);
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(text);
    }
    Ok(Output { text, image })
}

/// Disconnects every server; called when the app exits. Stdio servers get
/// two seconds to exit after their stdin closes, then are ended.
pub fn shutdown() {
    let connections: Vec<Arc<Connection>> = with(|s| {
        s.servers
            .iter_mut()
            .filter_map(|server| {
                server.stop.store(true, Ordering::Relaxed);
                server.connection.take()
            })
            .collect()
    });
    for connection in &connections {
        connection.release();
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline && !connections.iter().all(|c| c.exited()) {
        std::thread::sleep(Duration::from_millis(50));
    }
    for connection in &connections {
        connection.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Waits until server `name` leaves `Connecting`.
    fn settle(name: &str) -> Status {
        for _ in 0..1200 {
            let status = views().into_iter().find(|v| v.name == name).map(|v| v.status);
            if let Some(status) = status.filter(|s| *s != Status::Connecting) {
                return status;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Status::Connecting
    }

    /// Against the reference server over every transport. Needs Node:
    /// `cargo test -p serechat-desktop live_reference_server -- --ignored --nocapture`.
    #[test]
    #[ignore = "starts real MCP servers with npx"]
    fn live_reference_server() {
        let npx = |args: &[&str]| Transport::Stdio {
            command: "npx".into(),
            args: std::iter::once("-y").chain(std::iter::once("@modelcontextprotocol/server-everything")).chain(args.iter().copied()).map(str::to_owned).collect(),
            env: Vec::new(),
            cwd: None,
        };
        // stdio first, then the same server's HTTP modes, each started as a stdio "server" we ignore.
        configure(&[ServerConfig::new("everything".into(), npx(&[]))]);
        assert_eq!(settle("everything"), Status::Ready);
        let view = views().into_iter().find(|v| v.name == "everything").unwrap();
        println!("stdio: MCP {} with {} tools: {:?}", view.version, view.tools.len(), view.tools.iter().map(|t| &t.0).collect::<Vec<_>>());
        let cancel = AtomicBool::new(false);
        let echo = call("mcp__everything__echo", r#"{"message":"héllo"}"#, &cancel).unwrap();
        assert!(echo.text.contains("héllo"), "{}", echo.text);
        let sum = call("mcp__everything__get-sum", r#"{"a":2,"b":3}"#, &cancel).or_else(|_| call("mcp__everything__add", r#"{"a":2,"b":3}"#, &cancel)).unwrap();
        assert!(sum.text.contains('5'), "{}", sum.text);
        if find("mcp__everything__get-tiny-image").is_some() {
            let image = call("mcp__everything__get-tiny-image", "{}", &cancel).unwrap();
            assert!(image.image.is_some(), "{}", image.text);
        }
        println!("{}", instructions());

        for (mode, url) in [("streamableHttp", "http://localhost:3001/mcp"), ("sse", "http://localhost:3001/sse")] {
            let server = ServerConfig::new("runner".into(), npx(&[mode]));
            let web = ServerConfig::new("web".into(), Transport::Http { url: url.into(), headers: Vec::new(), sse: false });
            configure(std::slice::from_ref(&server));
            std::thread::sleep(Duration::from_secs(8));
            configure(&[server, web]);
            let status = settle("web");
            let view = views().into_iter().find(|v| v.name == "web").unwrap();
            println!("{mode}: {status:?}, MCP {}, {} tools", view.version, view.tools.len());
            assert_eq!(status, Status::Ready, "{mode}");
            let echo = call("mcp__web__echo", r#"{"message":"over http"}"#, &cancel).unwrap();
            assert!(echo.text.contains("over http"), "{}", echo.text);
            configure(&[]);
            std::thread::sleep(Duration::from_secs(3));
        }
        shutdown();
    }

    #[test]
    fn tool_names() {
        assert_eq!(exposed_name("my server", "read.file"), "mcp__my_server__read_file");
        let long = exposed_name("a-very-long-server-name-indeed", "and_an_equally_long_tool_name_that_goes_on");
        assert!(long.len() <= MAX_NAME && long.starts_with("mcp__a-very-long"));
        assert_ne!(long, exposed_name("a-very-long-server-name-indeed", "and_an_equally_long_tool_name_that_goes_on_2"));
        assert!(is_mcp("mcp__x__y") && !is_mcp("read_file"));
        assert_eq!(label("mcp__gh__search_issues"), ("gh".to_owned(), "search_issues".to_owned()));
        assert!(needs_approval("mcp__nobody__nothing"), "unknown tools ask");
    }

    #[test]
    fn tools_are_cleaned_up_for_the_model() {
        let listed = [
            json!({ "name": "search", "description": "Find things.", "inputSchema": { "$schema": "x", "type": "object", "properties": { "q": { "type": "string" } } }, "annotations": { "readOnlyHint": true } }),
            json!({ "name": "delete", "inputSchema": { "type": "object" }, "title": "Delete things" }),
            json!({ "name": "bad", "inputSchema": { "type": "object", "properties": { "n": { "type": "number", "x-mcp-header": "N" } } } }),
            json!({ "description": "no name" }),
        ];
        let (tools, warnings) = offered_tools("srv", &listed, true);
        assert_eq!(tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["search", "delete"]);
        assert!(tools[0].read_only && !tools[1].read_only);
        assert!(tools[0].schema.get("$schema").is_none());
        assert_eq!(tools[1].schema["properties"], json!({}));
        assert_eq!(tools[1].description, "Delete things");
        assert!(warnings[0].starts_with("bad was left out"));
        let (stdio, warnings) = offered_tools("srv", &listed, false);
        assert_eq!((stdio.len(), warnings.len()), (3, 0), "stdio ignores header annotations");
    }

    #[test]
    fn results_become_output() {
        let ok = json!({ "content": [
            { "type": "text", "text": "Hello" },
            { "type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png" },
            { "type": "resource_link", "uri": "file:///a.txt", "name": "a.txt" },
            { "type": "resource", "resource": { "uri": "x://y", "text": "inner" } },
            { "type": "audio", "data": "", "mimeType": "audio/wav" } ] });
        let output = format_result(&ok).unwrap();
        assert!(output.text.starts_with("Hello\n\n[An image (image/png)") && output.text.contains("Resource: a.txt <file:///a.txt>"));
        assert!(output.text.contains("<resource uri=\"x://y\">\ninner") && output.text.contains("[Audio (audio/wav) was left out.]"));
        assert_eq!(output.image.as_ref().map(|(mime, bytes)| (mime.as_str(), bytes.len())), Some(("image/png", 8)));
        assert_eq!(format_result(&json!({ "content": [], "structuredContent": { "n": 1 } })).unwrap().text, "{\n  \"n\": 1\n}");
        assert_eq!(format_result(&json!({})).unwrap().text, "(no output)");
        assert_eq!(format_result(&json!({ "content": [{ "type": "text", "text": "nope" }], "isError": true })).unwrap_err(), "nope");
    }
}
