//! MCP server settings in `~/.serechat/mcp.json`, and OAuth credentials in
//! `~/.serechat/mcp-auth.json`.
//!
//! `mcp.json` uses the `mcpServers` shape most MCP clients share (Claude,
//! Cursor, VS Code's `servers`, Windsurf's `serverUrl`, Gemini's `httpUrl`),
//! so a snippet from any server's README can be pasted in as is:
//!
//! ```json
//! { "mcpServers": {
//!     "files": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "~/notes"] },
//!     "linear": { "url": "https://mcp.linear.app/mcp" },
//!     "github": { "url": "https://api.githubcopilot.com/mcp/", "headers": { "Authorization": "Bearer ${GITHUB_TOKEN}" } }
//! } }
//! ```
//!
//! Strings may name environment variables as `${NAME}` or `${NAME:-default}`.
//! Keys this app does not use are kept, so saving never drops another
//! client's settings. Both files may hold secrets and are written `0600`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// How the app reaches a server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transport {
    /// A program the app starts, speaking over its stdin and stdout.
    Stdio {
        /// Program to run.
        command: String,
        /// Its arguments.
        args: Vec<String>,
        /// Extra environment variables.
        env: Vec<(String, String)>,
        /// Working folder; the home folder when `None`.
        cwd: Option<String>,
    },
    /// A server on the web.
    Http {
        /// Its MCP endpoint.
        url: String,
        /// Extra request headers (an API key, say).
        headers: Vec<(String, String)>,
        /// Use the deprecated HTTP+SSE transport without trying Streamable HTTP.
        sse: bool,
    },
}

/// An OAuth client registered with the server's authorization server by
/// hand, for servers that don't let clients register themselves.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OAuthClient {
    /// Pre-registered client id.
    pub client_id: Option<String>,
    /// Its secret, for confidential clients.
    pub client_secret: Option<String>,
    /// Scopes to ask for instead of the server's.
    pub scope: Option<String>,
    /// Fixed port for the sign-in redirect (`http://127.0.0.1:<port>/callback`).
    pub callback_port: Option<u16>,
}

/// One configured server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerConfig {
    /// Name the user gave it; its tools are called `mcp__<name>__<tool>`.
    pub name: String,
    /// Whether the app connects to it.
    pub enabled: bool,
    /// How to reach it.
    pub transport: Transport,
    /// OAuth settings, for servers on the web.
    pub oauth: OAuthClient,
    /// Keys this app does not use, kept as they were.
    extra: Map<String, Value>,
}

impl ServerConfig {
    /// A new, enabled server.
    #[must_use]
    pub fn new(name: String, transport: Transport) -> Self {
        Self { name, enabled: true, transport, oauth: OAuthClient::default(), extra: Map::new() }
    }

    /// What it runs or where it is, for display.
    #[must_use]
    pub fn target(&self) -> String {
        match &self.transport {
            Transport::Stdio { command, args, .. } => std::iter::once(command).chain(args).map(|a| quote(a)).collect::<Vec<_>>().join(" "),
            Transport::Http { url, .. } => url.clone(),
        }
    }

    /// The server as an `mcpServers` entry.
    fn to_json(&self) -> Value {
        let mut out = self.extra.clone();
        let pairs = |list: &[(String, String)]| Value::Object(list.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect());
        match &self.transport {
            Transport::Stdio { command, args, env, cwd } => {
                out.insert("command".into(), command.clone().into());
                if !args.is_empty() {
                    out.insert("args".into(), args.clone().into());
                }
                if !env.is_empty() {
                    out.insert("env".into(), pairs(env));
                }
                if let Some(cwd) = cwd {
                    out.insert("cwd".into(), cwd.clone().into());
                }
            }
            Transport::Http { url, headers, sse } => {
                out.insert("type".into(), if *sse { "sse" } else { "http" }.into());
                out.insert("url".into(), url.clone().into());
                if !headers.is_empty() {
                    out.insert("headers".into(), pairs(headers));
                }
            }
        }
        if !self.enabled {
            out.insert("disabled".into(), true.into());
        }
        let oauth = &self.oauth;
        if *oauth != OAuthClient::default() {
            let mut o = Map::new();
            let mut put = |key: &str, value: &Option<String>| {
                if let Some(value) = value {
                    o.insert(key.into(), value.clone().into());
                }
            };
            put("clientId", &oauth.client_id);
            put("clientSecret", &oauth.client_secret);
            put("scope", &oauth.scope);
            if let Some(port) = oauth.callback_port {
                o.insert("callbackPort".into(), port.into());
            }
            out.insert("oauth".into(), Value::Object(o));
        }
        Value::Object(out)
    }

    /// Reads an `mcpServers` entry named `name`.
    fn from_json(name: &str, value: &Value) -> Result<Self, String> {
        let Value::Object(object) = value else { return Err(format!("\"{name}\" is not an object.")) };
        let mut extra = object.clone();
        let mut take = |key: &str| extra.remove(key);
        let text = |value: Option<Value>, key: &str| match value {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(format!("\"{name}\": \"{key}\" must be a string.")),
        };
        let pairs = |value: Option<Value>, key: &str| -> Result<Vec<(String, String)>, String> {
            match value {
                None | Some(Value::Null) => Ok(Vec::new()),
                Some(Value::Object(map)) => map
                    .into_iter()
                    .map(|(k, v)| match v {
                        Value::String(s) => Ok((k, s)),
                        Value::Number(n) => Ok((k, n.to_string())),
                        Value::Bool(b) => Ok((k, b.to_string())),
                        _ => Err(format!("\"{name}\": every value in \"{key}\" must be a string.")),
                    })
                    .collect(),
                Some(_) => Err(format!("\"{name}\": \"{key}\" must be an object.")),
            }
        };
        let kind = text(take("type"), "type")?.or(text(take("transport"), "transport")?).or(text(take("transportType"), "transportType")?);
        let command = text(take("command"), "command")?;
        let args = match take("args") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items
                .into_iter()
                .map(|v| match v {
                    Value::String(s) => Ok(s),
                    Value::Number(n) => Ok(n.to_string()),
                    _ => Err(format!("\"{name}\": every argument must be a string.")),
                })
                .collect::<Result<_, _>>()?,
            Some(_) => Err(format!("\"{name}\": \"args\" must be a list."))?,
        };
        let env = pairs(take("env"), "env")?;
        let cwd = text(take("cwd"), "cwd")?;
        let url = text(take("url"), "url")?.or(text(take("serverUrl"), "serverUrl")?).or(text(take("httpUrl"), "httpUrl")?);
        let headers = pairs(take("headers"), "headers")?;
        let disabled = matches!(take("disabled"), Some(Value::Bool(true)));
        let enabled = !matches!(take("enabled"), Some(Value::Bool(false))) && !disabled;
        let oauth = match take("oauth") {
            Some(Value::Object(o)) => {
                let field = |key: &str| o.get(key).and_then(Value::as_str).map(str::to_owned);
                OAuthClient {
                    client_id: field("clientId").or_else(|| field("client_id")),
                    client_secret: field("clientSecret").or_else(|| field("client_secret")),
                    scope: field("scope").or_else(|| field("scopes")),
                    callback_port: o.get("callbackPort").and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok()),
                }
            }
            _ => OAuthClient::default(),
        };
        let transport = match (command, url) {
            (Some(command), None) if !command.trim().is_empty() => Transport::Stdio { command, args, env, cwd },
            (None, Some(url)) => {
                if !(url.starts_with("https://") || url.starts_with("http://")) {
                    return Err(format!("\"{name}\": the URL must start with https:// or http://."));
                }
                Transport::Http { url, headers, sse: kind.as_deref() == Some("sse") }
            }
            (Some(_), Some(_)) => return Err(format!("\"{name}\" has both a command and a URL; keep one.")),
            _ => return Err(format!("\"{name}\" needs a \"command\" to run or a \"url\" to connect to.")),
        };
        Ok(Self { name: name.to_owned(), enabled, transport, oauth, extra })
    }
}

/// `~/.serechat/mcp.json`.
///
/// # Errors
/// No home folder.
pub fn config_path() -> Result<PathBuf, String> {
    serechat::Config::dir().map(|dir| dir.join("mcp.json")).map_err(|e| e.to_string())
}

/// `~/.serechat/mcp-auth.json`.
///
/// # Errors
/// No home folder.
pub fn auth_path() -> Result<PathBuf, String> {
    serechat::Config::dir().map(|dir| dir.join("mcp-auth.json")).map_err(|e| e.to_string())
}

/// The servers in `text` (a whole `mcp.json`), sorted by name. An empty
/// file has none.
///
/// # Errors
/// Malformed JSON or a server entry that makes no sense, named.
pub fn parse(text: &str) -> Result<Vec<ServerConfig>, String> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_str(text).map_err(|e| format!("mcp.json is not valid JSON: {e}"))?;
    let servers = value.get("mcpServers").or_else(|| value.get("servers")).unwrap_or(&Value::Null);
    match servers {
        Value::Null => Ok(Vec::new()),
        Value::Object(map) => map.iter().map(|(name, entry)| ServerConfig::from_json(name, entry)).collect(),
        _ => Err("\"mcpServers\" must be an object.".into()),
    }
}

/// `servers` as the text of `mcp.json`.
#[must_use]
pub fn serialize(servers: &[ServerConfig]) -> String {
    let map: Map<String, Value> = servers.iter().map(|s| (s.name.clone(), s.to_json())).collect();
    let mut text = serde_json::to_string_pretty(&json!({ "mcpServers": map })).unwrap_or_default();
    text.push('\n');
    text
}

/// Servers pasted from somewhere else: a whole config (`mcpServers`, VS
/// Code's `servers`, or its settings' `mcp.servers`), a bare map of servers,
/// or one server on its own (named after its package or host).
///
/// # Errors
/// Nothing that reads as a server.
pub fn import(text: &str) -> Result<Vec<ServerConfig>, String> {
    let text = text.trim();
    // A snippet copied out of a larger file often lacks its outer braces.
    let value: Value = serde_json::from_str(text)
        .or_else(|_| serde_json::from_str(&format!("{{{}}}", text.trim_end_matches(','))))
        .map_err(|_| "The clipboard doesn't hold MCP server settings (JSON with \"mcpServers\", or a server with a \"command\" or \"url\").".to_owned())?;
    let is_server = |v: &Value| ["command", "url", "serverUrl", "httpUrl"].iter().any(|k| v.get(k).is_some_and(Value::is_string));
    let map = value
        .get("mcpServers")
        .or_else(|| value.get("servers"))
        .or_else(|| value.get("mcp").and_then(|m| m.get("servers")))
        .unwrap_or(&value);
    if is_server(map) {
        return ServerConfig::from_json(&guess_name(map), map).map(|s| vec![s]);
    }
    let Value::Object(map) = map else { return Err("Expected an object of MCP servers.".into()) };
    let servers: Vec<ServerConfig> = map.iter().filter(|(_, v)| is_server(v)).map(|(n, v)| ServerConfig::from_json(n, v)).collect::<Result<_, _>>()?;
    if servers.is_empty() { Err("The clipboard JSON names no MCP servers.".into()) } else { Ok(servers) }
}

/// A name for a server pasted without one: its package or its host.
fn guess_name(server: &Value) -> String {
    let field = |k: &str| server.get(k).and_then(Value::as_str);
    if let Some(url) = field("url").or(field("serverUrl")).or(field("httpUrl")) {
        let host = url.split("://").nth(1).unwrap_or(url).split(['/', ':', '?']).next().unwrap_or_default();
        let parts: Vec<&str> = host.split('.').filter(|p| !matches!(*p, "www" | "mcp" | "api" | "com" | "app" | "io" | "dev" | "net" | "org")).collect();
        return parts.first().map_or_else(|| "server".to_owned(), |p| (*p).to_owned());
    }
    // `npx -y @scope/server-name` → `server-name`; `uvx mcp-server-x` → `mcp-server-x`.
    let args = server.get("args").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>()).unwrap_or_default();
    let package = args.iter().find(|a| !a.starts_with('-')).copied().or(field("command")).unwrap_or("server");
    let base = package.rsplit(['/', '\\']).next().unwrap_or(package);
    let base = base.split('@').find(|p| !p.is_empty()).unwrap_or(base);
    base.trim_start_matches("server-").trim_end_matches(".exe").to_owned()
}

/// Builds a server from the settings form: `target` is a URL or a command
/// line; `extra` holds one `KEY=value` environment variable (commands) or
/// `Name: value` header (URLs) per line.
///
/// # Errors
/// A missing name or target, or a line that is not a pair.
pub fn from_form(name: &str, target: &str, extra: &str) -> Result<ServerConfig, String> {
    let name = name.trim();
    let target = target.trim();
    if name.is_empty() {
        return Err("Give the server a name.".into());
    }
    if target.is_empty() {
        return Err("Enter the command that starts the server, or its URL.".into());
    }
    let lines = extra.lines().map(str::trim).filter(|l| !l.is_empty());
    let transport = if target.starts_with("https://") || target.starts_with("http://") {
        let headers = lines
            .map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned())).filter(|(k, _)| is_token(k)))
            .collect::<Option<Vec<_>>>()
            .ok_or("Write each header as Name: value, one per line.")?;
        Transport::Http { url: target.to_owned(), headers, sse: false }
    } else {
        let mut words = split_command(target).into_iter();
        let command = words.next().ok_or("Enter the command that starts the server.")?;
        let env = lines
            .map(|l| l.split_once('=').map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned())).filter(|(k, _)| !k.is_empty()))
            .collect::<Option<Vec<_>>>()
            .ok_or("Write each environment variable as NAME=value, one per line.")?;
        Transport::Stdio { command, args: words.collect(), env, cwd: None }
    };
    Ok(ServerConfig::new(name.to_owned(), transport))
}

/// Whether `name` is a valid HTTP header name (RFC 9110 token).
pub(super) fn is_token(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Splits a command line into words: spaces separate, and `"…"` or `'…'`
/// group. Backslashes are kept as typed (they are Windows path separators).
#[must_use]
pub fn split_command(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started || !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                started = false;
            }
            (_, c) => word.push(c),
        }
    }
    if started || !word.is_empty() {
        words.push(word);
    }
    words
}

/// `word` quoted if it needs it to read back as one word.
fn quote(word: &str) -> String {
    if !word.is_empty() && !word.contains(char::is_whitespace) && !word.contains(['"', '\'']) {
        word.to_owned()
    } else if word.contains('"') {
        format!("'{word}'")
    } else {
        format!("\"{word}\"")
    }
}

/// Replaces `${NAME}` and `${NAME:-default}` with environment variables
/// (unset ones become empty, or their default) and a leading `~/` with the
/// home folder.
#[must_use]
pub fn expand(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    if let Some(tail) = rest.strip_prefix("~/").or_else(|| rest.strip_prefix("~\\"))
        && let Some(home) = std::env::home_dir()
    {
        out.push_str(&home.to_string_lossy());
        out.push(std::path::MAIN_SEPARATOR);
        rest = tail;
    }
    while let Some(start) = rest.find("${") {
        let Some(len) = rest[start..].find('}') else { break };
        out.push_str(&rest[..start]);
        let inner = &rest[start + 2..start + len];
        let (name, default) = inner.split_once(":-").map_or((inner, None), |(n, d)| (n, Some(d)));
        match std::env::var(name) {
            Ok(value) if !value.is_empty() => out.push_str(&value),
            _ => out.push_str(default.unwrap_or_default()),
        }
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    out
}

/// A client registered with an authorization server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    /// Client id.
    pub client_id: String,
    /// Secret, for confidential clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// How the token endpoint wants the client authenticated
    /// (`none`, `client_secret_basic` or `client_secret_post`).
    #[serde(default)]
    pub auth_method: String,
    /// The redirect URI it was registered with.
    #[serde(default)]
    pub redirect_uri: String,
}

/// Tokens for one server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    /// The server URL they were issued for; they are dropped if it changes.
    pub url: String,
    /// The authorization server that issued them.
    pub issuer: String,
    /// Where to refresh them.
    pub token_endpoint: String,
    /// The `resource` they were requested for (RFC 8707).
    #[serde(default)]
    pub resource: String,
    /// The client that holds them.
    pub client: Registration,
    /// Bearer token.
    pub access_token: String,
    /// For getting a new access token without the browser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// When the access token expires, in Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// Scopes asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Everything in `mcp-auth.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthStore {
    /// Clients this app registered, by the issuer that registered them.
    #[serde(default)]
    pub clients: BTreeMap<String, Registration>,
    /// Tokens by server name.
    #[serde(default)]
    pub tokens: BTreeMap<String, Tokens>,
}

impl AuthStore {
    /// Parses `mcp-auth.json`; an unreadable file counts as empty (the user
    /// signs in again).
    #[must_use]
    pub fn parse(text: &str) -> Self {
        serde_json::from_str(text).unwrap_or_default()
    }

    /// The file's text.
    #[must_use]
    pub fn serialize(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_keeps_unknown_keys() {
        let text = r#"{ "mcpServers": {
            "files": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"], "env": { "DEBUG": "1" }, "alwaysAllow": ["read"] },
            "linear": { "type": "streamable-http", "url": "https://mcp.linear.app/mcp", "disabled": true, "oauth": { "clientId": "abc", "callbackPort": 8765 } },
            "old": { "type": "sse", "url": "http://localhost:3000/sse", "headers": { "X-Key": "k" } }
        } }"#;
        let servers = parse(text).unwrap();
        assert_eq!(servers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["files", "linear", "old"]);
        assert!(matches!(&servers[0].transport, Transport::Stdio { command, args, env, .. } if command == "npx" && args.len() == 3 && env[0].0 == "DEBUG"));
        assert!(!servers[1].enabled && servers[1].oauth.client_id.as_deref() == Some("abc") && servers[1].oauth.callback_port == Some(8765));
        assert!(matches!(&servers[2].transport, Transport::Http { sse: true, headers, .. } if headers[0].1 == "k"));
        let again = parse(&serialize(&servers)).unwrap();
        assert_eq!(again, servers);
        assert!(serialize(&servers).contains("alwaysAllow"), "other clients' keys survive");
        assert_eq!(servers[0].target(), "npx -y @modelcontextprotocol/server-filesystem /tmp");
    }

    #[test]
    fn rejects_nonsense() {
        assert!(parse("{").unwrap_err().contains("not valid JSON"));
        assert!(parse(r#"{"mcpServers": {"x": {}}}"#).unwrap_err().contains("needs a"));
        assert!(parse(r#"{"mcpServers": {"x": {"url": "ftp://a"}}}"#).unwrap_err().contains("https://"));
        assert!(parse(r#"{"mcpServers": {"x": {"command": "a", "args": [{}]}}}"#).is_err());
        assert_eq!(parse("").unwrap(), Vec::new());
        assert_eq!(parse(r#"{"other": 1}"#).unwrap(), Vec::new());
    }

    #[test]
    fn imports_whatever_was_copied() {
        let vscode = r#"{"servers": {"gh": {"type": "http", "url": "https://api.githubcopilot.com/mcp/"}}}"#;
        assert_eq!(import(vscode).unwrap()[0].name, "gh");
        let fragment = r#""memory": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-memory"] },"#;
        assert_eq!(import(fragment).unwrap()[0].name, "memory");
        let single = r#"{ "command": "npx", "args": ["-y", "@modelcontextprotocol/server-everything"] }"#;
        assert_eq!(import(single).unwrap()[0].name, "everything");
        assert_eq!(import(r#"{"url": "https://mcp.linear.app/sse"}"#).unwrap()[0].name, "linear");
        assert_eq!(import(r#"{"serverUrl": "https://mcp.notion.com/mcp"}"#).unwrap()[0].name, "notion");
        assert!(import("hello").is_err());
        assert!(import(r#"{"a": 1}"#).is_err());
    }

    #[test]
    fn forms_and_command_lines() {
        assert_eq!(split_command(r#"npx -y "@scope/pkg" 'C:\My Files' "" x"#), ["npx", "-y", "@scope/pkg", r"C:\My Files", "", "x"]);
        assert_eq!(split_command("  "), Vec::<String>::new());
        let stdio = from_form(" files ", r#"uvx mcp-server-git --repository "C:\code\my repo""#, "TOKEN=abc\n\n A = b=c ").unwrap();
        assert!(matches!(&stdio.transport, Transport::Stdio { command, args, env, .. }
            if command == "uvx" && args[2] == r"C:\code\my repo" && env == &[("TOKEN".into(), "abc".into()), ("A".into(), "b=c".into())]));
        assert_eq!(stdio.name, "files");
        assert_eq!(parse(&serialize(std::slice::from_ref(&stdio))).unwrap()[0].target(), r#"uvx mcp-server-git --repository "C:\code\my repo""#);
        let web = from_form("w", "https://x.dev/mcp", "Authorization: Bearer t").unwrap();
        assert!(matches!(&web.transport, Transport::Http { headers, .. } if headers[0] == ("Authorization".into(), "Bearer t".into())));
        assert!(from_form("w", "https://x.dev/mcp", "not a header").is_err());
        assert!(from_form("w", "https://x.dev/mcp", "Bad Name: x").is_err());
        assert!(from_form("", "npx x", "").is_err() && from_form("n", " ", "").is_err());
    }

    #[test]
    fn expands_variables() {
        // SAFETY: tests touching this variable run in this test only.
        unsafe { std::env::set_var("SERECHAT_TEST_TOKEN", "s3cret") };
        assert_eq!(expand("Bearer ${SERECHAT_TEST_TOKEN}"), "Bearer s3cret");
        assert_eq!(expand("${SERECHAT_UNSET_VAR:-fallback}/x"), "fallback/x");
        assert_eq!(expand("${SERECHAT_UNSET_VAR}"), "");
        assert_eq!(expand("a ${unclosed"), "a ${unclosed");
        assert!(!expand("~/notes").starts_with('~'));
    }

    #[test]
    fn auth_store_round_trips() {
        let mut store = AuthStore::default();
        store.tokens.insert("s".into(), Tokens { url: "https://x".into(), access_token: "t".into(), ..Tokens::default() });
        assert_eq!(AuthStore::parse(&store.serialize()), store);
        assert_eq!(AuthStore::parse("garbage"), AuthStore::default());
    }
}
