//! Project folders the user works in, kept in `~/.serechat/projects.json`.
//!
//! A project is just a directory: sessions opened in it get that directory
//! as their working directory, and the agent's file tools are confined to it.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{Config, write_private};
use crate::error::Result;
use crate::session::unix_now;

/// One project folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// Absolute path of the folder.
    pub path: String,
    /// Display name (the folder's name).
    pub name: String,
    /// Last time it was opened, seconds since the Unix epoch.
    #[serde(default)]
    pub last_used: u64,
}

impl Project {
    /// A project for `dir`, named after its last component.
    #[must_use]
    pub fn new(dir: &Path) -> Self {
        let name = dir.file_name().map_or_else(|| dir.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
        Self { path: dir.to_string_lossy().into_owned(), name, last_used: unix_now() }
    }
}

/// The project list and where it is stored.
#[derive(Debug)]
pub struct Projects {
    path: PathBuf,
    /// Projects, most recently used first.
    pub list: Vec<Project>,
}

impl Projects {
    /// Loads `~/.serechat/projects.json` (empty if missing).
    ///
    /// # Errors
    /// No home directory, an unreadable file, or malformed JSON.
    pub fn load() -> Result<Self> {
        Self::load_from(Config::dir()?.join("projects.json"))
    }

    /// Loads from an explicit path (empty if missing).
    ///
    /// # Errors
    /// An unreadable file or malformed JSON.
    pub fn load_from(path: PathBuf) -> Result<Self> {
        let mut list: Vec<Project> = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        list.sort_by_key(|p| std::cmp::Reverse(p.last_used));
        Ok(Self { path, list })
    }

    /// Adds `dir` (or refreshes it if already listed) and moves it to the front.
    pub fn touch(&mut self, dir: &Path) -> &Project {
        let path = dir.to_string_lossy();
        let mut project = match self.list.iter().position(|p| p.path == path) {
            Some(i) => self.list.remove(i),
            None => Project::new(dir),
        };
        project.last_used = unix_now();
        self.list.insert(0, project);
        &self.list[0]
    }

    /// Forgets a project (its folder and sessions are untouched).
    pub fn remove(&mut self, path: &str) {
        self.list.retain(|p| p.path != path);
    }

    /// Writes the list atomically.
    ///
    /// # Errors
    /// Any I/O failure.
    pub fn save(&self) -> Result<()> {
        write_private(&self.path, &serde_json::to_vec_pretty(&self.list)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_save_load() {
        let dir = std::env::temp_dir().join(format!("serechat-projects-{}", std::process::id()));
        let file = dir.join("projects.json");
        let mut projects = Projects::load_from(file.clone()).unwrap();
        assert!(projects.list.is_empty());
        projects.touch(Path::new("/work/alpha"));
        projects.touch(Path::new("/work/beta"));
        assert_eq!(projects.touch(Path::new("/work/alpha")).name, "alpha");
        assert_eq!(projects.list.len(), 2);
        projects.save().unwrap();

        let mut loaded = Projects::load_from(file).unwrap();
        assert_eq!(loaded.list.len(), 2);
        loaded.remove(&projects.list[1].path);
        assert_eq!(loaded.list.len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }
}
