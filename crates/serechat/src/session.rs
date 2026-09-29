//! Chat sessions saved as one JSON file each in `~/.serechat/sessions/`.
//!
//! Files are written atomically with the same private permissions as the
//! config. Fields added later must be `#[serde(default)]` so older files keep
//! loading.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::{Config, write_private};
use crate::error::{Error, Result};
use crate::responses::{Role, Usage};

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
    /// Every turn, oldest first.
    pub messages: Vec<StoredMessage>,
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
}

impl StoredMessage {
    /// A plain message without reply metadata.
    #[must_use]
    pub fn new(role: Role, content: String) -> Self {
        Self { role, content, reasoning: String::new(), model: None, usage: Usage::default(), cost: 0.0, failed: false }
    }
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

/// Reads and writes the session files in one directory.
#[derive(Debug, Clone)]
pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    /// The store in `~/.serechat/sessions`.
    ///
    /// # Errors
    /// [`Error::NoHomeDir`] if the platform reports no home directory.
    pub fn open() -> Result<Self> {
        Ok(Self::at(Config::dir()?.join("sessions")))
    }

    /// A store in an explicit directory (created on first save).
    #[must_use]
    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
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

    /// Loads every session, most recently updated first. A file that fails
    /// to load is reported in place of its session and left untouched.
    ///
    /// # Errors
    /// The directory exists but cannot be listed.
    pub fn load_all(&self) -> Result<Vec<Result<Session>>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut sessions = Vec::new();
        for entry in entries {
            let path = entry?.path();
            let Some(id) = path.file_stem().and_then(|s| s.to_str()).filter(|id| valid_id(id)) else { continue };
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let id = id.to_owned();
            sessions.push(fs::read(&path).map_err(Error::from).and_then(|bytes| {
                let mut session: Session = serde_json::from_slice(&bytes)?;
                // The file name is authoritative; it is what `delete` removes.
                session.id = id;
                Ok(session)
            }));
        }
        sessions.sort_by_key(|s| std::cmp::Reverse(s.as_ref().map_or(u64::MAX, |s| s.updated)));
        Ok(sessions)
    }

    /// Writes `session` atomically.
    ///
    /// # Errors
    /// An invalid id or any I/O failure.
    pub fn save(&self, session: &Session) -> Result<()> {
        write_private(&self.path(&session.id)?, &serde_json::to_vec(session)?)
    }

    /// Deletes a session's file; deleting a missing session succeeds.
    ///
    /// # Errors
    /// An invalid id or any I/O failure other than "not found".
    pub fn delete(&self, id: &str) -> Result<()> {
        match fs::remove_file(self.path(id)?) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_delete() {
        let dir = std::env::temp_dir().join(format!("serechat-session-test-{}", std::process::id()));
        let store = SessionStore::at(dir.clone());
        assert!(store.load_all().unwrap().is_empty());

        let mut reply = StoredMessage::new(Role::Assistant, "hello".into());
        reply.usage = Usage { input_tokens: 10, output_tokens: 5 };
        reply.cost = 0.25;
        let old = Session { id: "a1".into(), title: "Old".into(), updated: 1, ..Session::default() };
        let new = Session {
            id: "b2".into(),
            title: "New".into(),
            updated: 2,
            messages: vec![StoredMessage::new(Role::User, "hi".into()), reply],
            ..Session::default()
        };
        store.save(&old).unwrap();
        store.save(&new).unwrap();
        fs::write(dir.join("broken.json"), "{").unwrap();

        let loaded = store.load_all().unwrap();
        assert!(loaded[0].is_err(), "unreadable files sort first so they get noticed");
        let loaded: Vec<_> = loaded.into_iter().filter_map(Result::ok).collect();
        assert_eq!(loaded, [new.clone(), old]);
        assert_eq!(loaded[0].tokens(), 15);
        assert!((loaded[0].cost() - 0.25).abs() < f64::EPSILON);

        store.delete("a1").unwrap();
        store.delete("a1").unwrap();
        assert!(store.save(&Session { id: "../evil".into(), ..Session::default() }).is_err());
        assert!(store.delete("..").is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compact_json_for_plain_messages() {
        let json = serde_json::to_string(&StoredMessage::new(Role::User, "hi".into())).unwrap();
        assert_eq!(json, r#"{"role":"user","content":"hi"}"#);
        assert!(valid_id(&new_session_id()));
    }
}
