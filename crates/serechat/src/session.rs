//! Chat sessions saved as one JSON file each in `~/.serechat/sessions/`.
//!
//! Next to them, `.index.json` holds a [`SessionSummary`] per session so the
//! sidebar and usage totals load without reading any message bodies. The
//! index is a cache: [`SessionStore::list`] reconciles it against the files
//! on disk (a directory listing plus modification times, no reads) and
//! re-reads only sessions that are new or changed since the index was
//! written, so a crash between the two writes of a save heals itself.
//!
//! Files are written atomically with the same private permissions as the
//! config. Fields added later must be `#[serde(default)]` so older files keep
//! loading.

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::{Config, write_private};
use crate::error::{Error, Result};
use crate::responses::{Role, Usage};

/// Name of the index file. The leading dot keeps it out of session scans,
/// since `.` is not a valid id character.
const INDEX_FILE: &str = ".index.json";

/// One saved conversation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    /// Identifier, also the file name without `.json`.
    pub id: String,
    /// Sidebar title, taken from the first prompt.
    pub title: String,
    /// Creation time, seconds since the Unix epoch.
    pub created: u64,
    /// Last activity, seconds since the Unix epoch.
    pub updated: u64,
    /// Project directory the session works in, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Every turn, oldest first.
    pub messages: Vec<StoredMessage>,
    /// Tools the user allowed to run without asking in this session.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,
}

impl Session {
    /// Total spent on this session in USD.
    #[must_use]
    pub fn cost(&self) -> f64 {
        self.messages.iter().map(|m| m.cost).sum()
    }

    /// Total tokens billed for this session.
    #[must_use]
    pub fn tokens(&self) -> u64 {
        self.messages.iter().map(|m| m.usage.input_tokens + m.usage.output_tokens).sum()
    }

    /// The index entry for this session.
    #[must_use]
    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            id: self.id.clone(),
            title: self.title.clone(),
            created: self.created,
            updated: self.updated,
            project: self.project.clone(),
            cost: self.cost(),
            tokens: self.tokens(),
        }
    }
}

/// What the sidebar and usage totals need to know about a session, without
/// its messages.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionSummary {
    /// Session id.
    pub id: String,
    /// Sidebar title.
    pub title: String,
    /// Creation time, seconds since the Unix epoch.
    pub created: u64,
    /// Last activity, seconds since the Unix epoch.
    pub updated: u64,
    /// Project directory, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Total spent in USD.
    pub cost: f64,
    /// Total tokens billed.
    pub tokens: u64,
}

/// One turn of a saved session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredMessage {
    /// Author.
    pub role: Role,
    /// Text of the prompt or reply (or the error, when `failed`).
    pub content: String,
    /// The model's reasoning, for replies from thinking models.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning: String,
    /// How long the model reasoned before answering, in milliseconds
    /// (0 when unknown or it did not reason).
    #[serde(default, skip_serializing_if = "is_default")]
    pub reasoning_ms: u64,
    /// Model that wrote a reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Tokens billed for a reply.
    #[serde(default, skip_serializing_if = "is_default")]
    pub usage: Usage,
    /// What a reply cost in USD, at the prices when it was written.
    #[serde(default, skip_serializing_if = "is_default")]
    pub cost: f64,
    /// The turn is an error message, not model output.
    #[serde(default, skip_serializing_if = "is_default")]
    pub failed: bool,
    /// Files sent with a prompt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// Tools a reply asked to run, with their results.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolRecord>,
    /// A summary that replaces every earlier message when talking to the
    /// model, written when the conversation outgrew the context window.
    /// The earlier messages stay in the session for the user.
    #[serde(default, skip_serializing_if = "is_default")]
    pub compaction: bool,
    /// The image, video or audio generation this reply is, or waits for.
    /// Its file arrives as an attachment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<crate::media::MediaJob>,
}

impl StoredMessage {
    /// A plain message without reply metadata.
    #[must_use]
    pub fn new(role: Role, content: String) -> Self {
        Self {
            role,
            content,
            reasoning: String::new(),
            reasoning_ms: 0,
            model: None,
            usage: Usage::default(),
            cost: 0.0,
            failed: false,
            attachments: Vec::new(),
            tool_calls: Vec::new(),
            compaction: false,
            media: None,
        }
    }

    /// A generation whose file has not arrived and that has not failed.
    #[must_use]
    pub fn media_pending(&self) -> bool {
        self.media.is_some() && self.attachments.is_empty() && !self.failed
    }
}

/// A file attached to a prompt. The app keeps its own copy so the session
/// stays complete if the original moves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Original file name.
    pub name: String,
    /// MIME type, e.g. `image/png`, `text/plain`, `application/pdf`.
    pub mime: String,
    /// Size in bytes.
    pub size: u64,
    /// Absolute path of the app's copy.
    pub path: String,
    /// Width and height in pixels, for images whose size is known (generated
    /// ones), so they show at their own aspect ratio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<(u32, u32)>,
}

impl Attachment {
    /// Whether it is an image (sent as `input_image`).
    #[must_use]
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }
}

/// Progress of a tool call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// Waiting to run or for the user's approval.
    #[default]
    Pending,
    /// Running now.
    Running,
    /// Finished; `output` holds the result.
    Done,
    /// Failed; `output` holds the error.
    Failed,
    /// The user declined it.
    Denied,
}

impl ToolStatus {
    /// Whether the call has an output to send back to the model.
    #[must_use]
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Denied)
    }
}

/// A tool call and what became of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRecord {
    /// The call as the model made it.
    #[serde(flatten)]
    pub call: crate::responses::ToolCall,
    /// Progress.
    #[serde(default)]
    pub status: ToolStatus,
    /// Result or error text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output: String,
    /// An image the tool returned (a browser screenshot), kept with the
    /// session's attachments and shown to the model after `output`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<Attachment>,
}

/// A message matching a content search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// Session id.
    pub session: String,
    /// Session title.
    pub title: String,
    /// The matching line, shortened around the match.
    pub snippet: String,
    /// Session's last activity, for ranking.
    pub updated: u64,
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

/// Seconds since the Unix epoch.
#[must_use]
pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A fresh, file-name-safe session id (the creation time in hex nanoseconds).
#[must_use]
pub fn new_session_id() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("{nanos:x}")
}

/// Ids become file names, so only a conservative character set is allowed.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// On-disk shape of [`INDEX_FILE`].
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Index {
    version: u32,
    sessions: Vec<SessionSummary>,
}

/// Reads and writes the session files in one directory, keeping the index
/// up to date.
#[derive(Debug)]
pub struct SessionStore {
    dir: PathBuf,
    /// Summaries, newest first. Valid once `indexed` is set.
    index: Vec<SessionSummary>,
    indexed: bool,
}

impl SessionStore {
    /// The store in `~/.serechat/sessions`.
    ///
    /// # Errors
    /// [`Error::NoHomeDir`] if the platform reports no home directory.
    pub fn open() -> Result<Self> {
        Ok(Self::at(Config::dir()?.join("sessions")))
    }

    /// A store in an explicit directory (created on first save). Cheap: no
    /// I/O happens until a method needs it.
    #[must_use]
    pub fn at(dir: PathBuf) -> Self {
        Self { dir, index: Vec::new(), indexed: false }
    }

    /// The directory holding the session files.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, id: &str) -> Result<PathBuf> {
        if valid_id(id) {
            Ok(self.dir.join(format!("{id}.json")))
        } else {
            Err(Error::Io(std::io::Error::new(ErrorKind::InvalidInput, "invalid session id")))
        }
    }

    /// Summaries of every session, most recently updated first, plus the
    /// errors of session files that could not be read (those files are left
    /// untouched and not listed).
    ///
    /// Reads only the index, re-reading session files that are missing from
    /// it or changed after it was written.
    ///
    /// # Errors
    /// The directory exists but cannot be listed.
    pub fn list(&mut self) -> Result<(Vec<SessionSummary>, Vec<Error>)> {
        let index_path = self.dir.join(INDEX_FILE);
        let (mut index, index_time) = match fs::read(&index_path) {
            // A corrupt index is rebuilt from the session files.
            Ok(bytes) => match serde_json::from_slice::<Index>(&bytes) {
                Ok(index) => (index.sessions, fs::metadata(&index_path).and_then(|m| m.modified()).ok()),
                Err(_) => (Vec::new(), None),
            },
            Err(e) if e.kind() == ErrorKind::NotFound => (Vec::new(), None),
            Err(e) => return Err(e.into()),
        };
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => Some(entries),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };

        let (mut on_disk, mut errors, mut changed) = (HashSet::new(), Vec::new(), false);
        for entry in entries.into_iter().flatten() {
            let entry = entry?;
            let path = entry.path();
            let Some(id) = path.file_stem().and_then(|s| s.to_str()).filter(|id| valid_id(id)) else { continue };
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            on_disk.insert(id.to_owned());
            let known = index.iter().position(|s| s.id == id);
            let modified = entry.metadata().and_then(|m| m.modified()).ok();
            let stale = match (known, index_time, modified) {
                (Some(_), Some(indexed), Some(modified)) => modified > indexed,
                _ => true,
            };
            if !stale {
                continue;
            }
            match self.load(id) {
                Ok(session) => {
                    let summary = session.summary();
                    match known {
                        Some(i) => index[i] = summary,
                        None => index.push(summary),
                    }
                    changed = true;
                }
                Err(e) => {
                    // Keep a stale entry out of the list rather than show it.
                    if let Some(i) = known {
                        index.remove(i);
                    }
                    errors.push(e);
                }
            }
        }
        let before = index.len();
        index.retain(|s| on_disk.contains(&s.id));
        changed |= index.len() != before;

        index.sort_by_key(|s| std::cmp::Reverse(s.updated));
        self.index = index;
        self.indexed = true;
        if changed && let Err(e) = self.write_index() {
            // Listing still worked; the next save retries the write.
            errors.push(e);
        }
        Ok((self.index.clone(), errors))
    }

    /// Reads one session with all its messages.
    ///
    /// # Errors
    /// An invalid id, a missing or unreadable file, or malformed JSON.
    pub fn load(&self, id: &str) -> Result<Session> {
        let mut session: Session = serde_json::from_slice(&fs::read(self.path(id)?)?)?;
        // The file name is authoritative; it is what `delete` removes.
        id.clone_into(&mut session.id);
        Ok(session)
    }

    /// Writes `session` atomically and updates the index.
    ///
    /// # Errors
    /// An invalid id or any I/O failure.
    pub fn save(&mut self, session: &Session) -> Result<()> {
        write_private(&self.path(&session.id)?, &serde_json::to_vec(session)?)?;
        self.ensure_indexed()?;
        let summary = session.summary();
        match self.index.iter_mut().find(|s| s.id == session.id) {
            Some(entry) => *entry = summary,
            None => self.index.push(summary),
        }
        self.index.sort_by_key(|s| std::cmp::Reverse(s.updated));
        self.write_index()
    }

    /// Where attachment copies live: `attachments/` next to the sessions.
    #[must_use]
    pub fn attachments_dir(&self) -> PathBuf {
        self.dir.parent().map_or_else(|| self.dir.join("attachments"), |p| p.join("attachments"))
    }

    /// Case-insensitive search of every session's messages for `query`,
    /// newest sessions first, at most one hit per session. Reads every file,
    /// so run it off the UI thread.
    ///
    /// # Errors
    /// The directory exists but cannot be listed.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let needle = query.trim().to_lowercase();
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut hits = Vec::new();
        if needle.is_empty() {
            return Ok(hits);
        }
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(id) = path.file_stem().and_then(|s| s.to_str()).filter(|id| valid_id(id)) else { continue };
            // Unreadable files are skipped; `list` reports them.
            let Ok(session) = self.load(id) else { continue };
            let found = session.messages.iter().find_map(|m| {
                m.content.lines().find(|line| line.to_lowercase().contains(&needle)).map(|line| snippet(line, &needle))
            });
            if let Some(snippet) = found {
                hits.push(SearchHit { session: session.id, title: session.title, snippet, updated: session.updated });
            }
        }
        hits.sort_by_key(|h| std::cmp::Reverse(h.updated));
        hits.truncate(limit);
        Ok(hits)
    }

    /// Deletes a session's file, its attachment copies and its index entry;
    /// deleting a missing session succeeds.
    ///
    /// # Errors
    /// An invalid id or any I/O failure other than "not found".
    pub fn delete(&mut self, id: &str) -> Result<()> {
        if let Ok(session) = self.load(id) {
            let attachments = self.attachments_dir();
            for message in &session.messages {
                let images = message.tool_calls.iter().filter_map(|r| r.image.as_ref());
                for attachment in message.attachments.iter().chain(images) {
                    // Only ever delete the app's own copies.
                    let path = Path::new(&attachment.path);
                    if path.starts_with(&attachments) {
                        let _ = fs::remove_file(path);
                    }
                }
            }
        }
        match fs::remove_file(self.path(id)?) {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        self.ensure_indexed()?;
        self.index.retain(|s| s.id != id);
        self.write_index()
    }

    /// Loads the index before the first change, so writing it back never
    /// drops sessions this store has not listed yet.
    fn ensure_indexed(&mut self) -> Result<()> {
        if !self.indexed {
            self.list()?;
        }
        Ok(())
    }

    fn write_index(&self) -> Result<()> {
        let index = Index { version: 1, sessions: self.index.clone() };
        write_private(&self.dir.join(INDEX_FILE), &serde_json::to_vec(&index)?)
    }
}

/// About 90 characters of `line` around the first match of `needle`
/// (already lower-cased), with ellipses where cut.
fn snippet(line: &str, needle: &str) -> String {
    const CONTEXT: usize = 40;
    let chars: Vec<char> = line.trim().chars().collect();
    let lower: Vec<char> = chars.iter().flat_map(|c| c.to_lowercase()).collect();
    let needle: Vec<char> = needle.chars().collect();
    // Case folding can change lengths; fall back to the start if it did.
    let at = if lower.len() == chars.len() { lower.windows(needle.len()).position(|w| w == needle.as_slice()).unwrap_or(0) } else { 0 };
    let start = at.saturating_sub(CONTEXT);
    let end = (at + needle.len() + CONTEXT).min(chars.len());
    let mut out: String = chars[start..end].iter().collect();
    if start > 0 {
        out.insert(0, '…');
    }
    if end < chars.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> (PathBuf, SessionStore) {
        // `sessions/` inside a private folder, so `attachments/` is private too.
        let dir = std::env::temp_dir().join(format!("serechat-{name}-{}", std::process::id())).join("sessions");
        let _ = fs::remove_dir_all(dir.parent().unwrap());
        (dir.clone(), SessionStore::at(dir))
    }

    fn session(id: &str, updated: u64, cost: f64) -> Session {
        let mut reply = StoredMessage::new(Role::Assistant, "hello".into());
        reply.usage = Usage::new(10, 5);
        reply.cost = cost;
        Session {
            id: id.into(),
            title: format!("Title {id}"),
            updated,
            messages: vec![StoredMessage::new(Role::User, "hi".into()), reply],
            ..Session::default()
        }
    }

    #[test]
    fn save_list_load_delete() {
        let (dir, mut store) = temp_store("sessions");
        assert_eq!(store.list().unwrap().0.len(), 0);

        let (old, new) = (session("a1", 1, 0.5), session("b2", 2, 0.25));
        store.save(&old).unwrap();
        store.save(&new).unwrap();
        fs::write(dir.join("broken.json"), "{").unwrap();

        // A fresh store (a new app launch) lists from the index alone.
        let mut store = SessionStore::at(dir.clone());
        let (listed, errors) = store.list().unwrap();
        assert_eq!(listed, [new.summary(), old.summary()]);
        assert_eq!(errors.len(), 1, "the broken file is reported, not listed");
        assert_eq!(listed[0].tokens, 15);
        assert_eq!(store.load("b2").unwrap(), new);

        store.delete("a1").unwrap();
        store.delete("a1").unwrap();
        assert_eq!(SessionStore::at(dir.clone()).list().unwrap().0, [new.summary()]);
        assert!(store.save(&Session { id: "../evil".into(), ..Session::default() }).is_err());
        assert!(store.delete("..").is_err());
        fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn index_heals_from_the_files() {
        let (dir, mut store) = temp_store("index");
        let (kept, edited) = (session("k1", 1, 0.1), session("e2", 2, 0.2));
        store.save(&kept).unwrap();
        store.save(&edited).unwrap();

        // Simulate a crash between writes and outside edits: one file changed
        // without the index, one appeared, and one vanished.
        let mut changed = edited.clone();
        changed.title = "Renamed".into();
        fs::write(dir.join("e2.json"), serde_json::to_vec(&changed).unwrap()).unwrap();
        let old = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
        fs::File::options().write(true).open(dir.join(INDEX_FILE)).unwrap().set_modified(old).unwrap();
        let added = session("n3", 3, 0.3);
        fs::write(dir.join("n3.json"), serde_json::to_vec(&added).unwrap()).unwrap();
        fs::remove_file(dir.join("k1.json")).unwrap();

        let (listed, errors) = SessionStore::at(dir.clone()).list().unwrap();
        assert!(errors.is_empty());
        assert_eq!(listed, [added.summary(), changed.summary()]);

        // A corrupt index is rebuilt rather than trusted.
        fs::write(dir.join(INDEX_FILE), "garbage").unwrap();
        assert_eq!(SessionStore::at(dir.clone()).list().unwrap().0, listed);
        fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn search_and_attachment_cleanup() {
        let (dir, mut store) = temp_store("search");
        let attachments = store.attachments_dir();
        fs::create_dir_all(&attachments).unwrap();
        let copy = attachments.join("a.txt");
        fs::write(&copy, "x").unwrap();

        let mut prompt = StoredMessage::new(Role::User, "first line\nThe Quick brown fox jumps".into());
        prompt.attachments.push(Attachment { name: "a.txt".into(), mime: "text/plain".into(), size: 1, path: copy.to_string_lossy().into(), dimensions: None });
        let mut s = session("s1", 5, 0.0);
        s.messages.insert(0, prompt);
        store.save(&s).unwrap();
        store.save(&session("s2", 6, 0.0)).unwrap();

        let hits = store.search("quick BROWN", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session, "s1");
        assert_eq!(hits[0].snippet, "The Quick brown fox jumps");
        assert_eq!(store.search("  ", 10).unwrap().len(), 0);
        assert_eq!(snippet(&"x".repeat(100), "xx").chars().last(), Some('…'));

        store.delete("s1").unwrap();
        assert!(!copy.exists(), "attachment copies go with their session");
        fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn tool_records_round_trip() {
        let mut reply = StoredMessage::new(Role::Assistant, String::new());
        reply.tool_calls.push(ToolRecord {
            call: crate::responses::ToolCall { call_id: "c1".into(), name: "read_file".into(), arguments: "{}".into() },
            status: ToolStatus::Done,
            output: "ok".into(),
            image: None,
        });
        let json = serde_json::to_string(&reply).unwrap();
        assert!(json.contains(r#""call_id":"c1""#) && json.contains(r#""status":"done""#));
        assert_eq!(serde_json::from_str::<StoredMessage>(&json).unwrap(), reply);
    }

    #[test]
    fn compact_json_for_plain_messages() {
        let json = serde_json::to_string(&StoredMessage::new(Role::User, "hi".into())).unwrap();
        assert_eq!(json, r#"{"role":"user","content":"hi"}"#);
        assert!(valid_id(&new_session_id()));
    }
}
