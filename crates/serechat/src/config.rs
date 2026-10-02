//! User configuration stored in `~/.serechat/config.toml`.
//!
//! The file is a flat TOML document of `key = "string"` pairs. Only that
//! subset is parsed: basic (`"..."`) and literal (`'...'`) strings, comments
//! and blank lines. Unknown keys are ignored so older builds tolerate newer
//! files.
//!
//! ponytail: flat string-only TOML subset; switch to the `toml` crate once
//! the config needs tables, arrays or numbers.

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Name of the per-user data directory inside the home directory.
const DIR_NAME: &str = ".serechat";
/// Name of the configuration file inside [`DIR_NAME`].
const FILE_NAME: &str = "config.toml";

/// Persistent user settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// Bearer token obtained through the device-code flow.
    pub token: Option<String>,
    /// Identifier of the model used for new messages.
    pub model: Option<String>,
    /// Reasoning effort for new messages (`none`, `low`, `medium`, `high`);
    /// absent means the model's default.
    pub reasoning: Option<String>,
    /// Project folder new chats open in.
    pub project: Option<String>,
    /// Colour scheme name, interpreted by the app.
    pub theme: Option<String>,
    /// How replies show the model's reasoning, interpreted by the app.
    pub reasoning_view: Option<String>,
    /// Model `/image` generates with.
    pub image_model: Option<String>,
    /// Model `/video` generates with.
    pub video_model: Option<String>,
    /// Model `/audio` generates with.
    pub audio_model: Option<String>,
    /// `off` to only check for updates, not install them; anything else installs.
    pub auto_update: Option<String>,
    /// Browser the agent drives, interpreted by the app; absent means Auto.
    pub browser: Option<String>,
}

impl Config {
    /// Returns `~/.serechat`, the directory holding all local app data.
    ///
    /// # Errors
    /// [`Error::NoHomeDir`] if the platform reports no home directory.
    pub fn dir() -> Result<PathBuf> {
        std::env::home_dir()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|home| home.join(DIR_NAME))
            .ok_or(Error::NoHomeDir)
    }

    /// Returns the full path of the configuration file.
    ///
    /// # Errors
    /// See [`Config::dir`].
    pub fn path() -> Result<PathBuf> {
        Ok(Self::dir()?.join(FILE_NAME))
    }

    /// Loads the configuration, returning defaults if the file does not exist.
    ///
    /// # Errors
    /// I/O failures other than "not found", or a malformed file.
    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path()?)
    }

    /// Loads the configuration from an explicit path.
    ///
    /// # Errors
    /// See [`Config::load`].
    pub fn load_from(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Writes the configuration atomically to `~/.serechat/config.toml`.
    ///
    /// # Errors
    /// Any I/O failure while creating the directory or writing the file.
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path()?)
    }

    /// Writes the configuration atomically to an explicit path.
    ///
    /// The file holds a credential, so on Unix it is created with mode `0600`
    /// inside a `0700` directory. On Windows the user profile ACLs apply.
    ///
    /// # Errors
    /// See [`Config::save`].
    pub fn save_to(&self, path: &Path) -> Result<()> {
        write_private(path, self.serialize().as_bytes())
    }

    /// Parses the flat TOML subset described in the module docs.
    ///
    /// # Errors
    /// [`Error::Config`] pointing at the first malformed line.
    pub fn parse(text: &str) -> Result<Self> {
        let mut config = Self::default();
        for (index, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let err = |message| Error::Config { line: index + 1, message };
            let (key, rest) = line.split_once('=').ok_or_else(|| err("expected `key = \"value\"`"))?;
            let key = key.trim();
            let value = parse_string(rest.trim()).map_err(err)?;
            match key {
                "token" => config.token = Some(value),
                "model" => config.model = Some(value),
                "reasoning" => config.reasoning = Some(value),
                "theme" => config.theme = Some(value),
                "project" => config.project = Some(value),
                "reasoning_view" => config.reasoning_view = Some(value),
                "image_model" => config.image_model = Some(value),
                "video_model" => config.video_model = Some(value),
                "audio_model" => config.audio_model = Some(value),
                "auto_update" => config.auto_update = Some(value),
                "browser" => config.browser = Some(value),
                _ => {}
            }
        }
        Ok(config)
    }

    /// Serializes to TOML text.
    #[must_use]
    pub fn serialize(&self) -> String {
        let mut out = String::from("# SereChat desktop configuration.\n");
        let fields = [
            ("token", &self.token),
            ("model", &self.model),
            ("reasoning", &self.reasoning),
            ("theme", &self.theme),
            ("project", &self.project),
            ("reasoning_view", &self.reasoning_view),
            ("image_model", &self.image_model),
            ("video_model", &self.video_model),
            ("audio_model", &self.audio_model),
            ("auto_update", &self.auto_update),
            ("browser", &self.browser),
        ];
        for (key, value) in fields {
            if let Some(value) = value {
                out.push_str(key);
                out.push_str(" = ");
                push_quoted(&mut out, value);
                out.push('\n');
            }
        }
        out
    }
}

/// Parses a TOML basic or literal string, allowing a trailing comment.
fn parse_string(src: &str) -> std::result::Result<String, &'static str> {
    let mut chars = src.chars();
    let quote = chars.next().filter(|c| *c == '"' || *c == '\'').ok_or("value must be a quoted string")?;
    let mut out = String::new();
    loop {
        let c = chars.next().ok_or("unterminated string")?;
        match c {
            c if c == quote => break,
            '\\' if quote == '"' => out.push(match chars.next().ok_or("unterminated escape")? {
                '"' => '"',
                '\\' => '\\',
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                'b' => '\u{8}',
                'f' => '\u{c}',
                'u' => parse_unicode(&mut chars, 4)?,
                'U' => parse_unicode(&mut chars, 8)?,
                _ => return Err("invalid escape sequence"),
            }),
            '\n' | '\r' => return Err("newline in string"),
            c => out.push(c),
        }
    }
    let rest = chars.as_str().trim_start();
    if rest.is_empty() || rest.starts_with('#') {
        Ok(out)
    } else {
        Err("unexpected text after value")
    }
}

/// Reads `digits` hex digits and converts them to a scalar value.
fn parse_unicode(chars: &mut std::str::Chars<'_>, digits: usize) -> std::result::Result<char, &'static str> {
    let hex: String = chars.by_ref().take(digits).collect();
    if hex.len() != digits {
        return Err("truncated unicode escape");
    }
    u32::from_str_radix(&hex, 16)
        .ok()
        .and_then(char::from_u32)
        .ok_or("invalid unicode escape")
}

/// Appends `value` as a TOML basic string.
fn push_quoted(out: &mut String, value: &str) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Atomically replaces `path` with `bytes`, readable only by the user.
///
/// Writes a sibling `.tmp` file and renames it over the original, so a crash
/// mid-write never leaves a truncated file behind. On Unix the file is
/// created `0600` inside a `0700` directory; on Windows the profile ACLs apply.
///
/// # Errors
/// Any I/O failure while creating the directory or writing the file.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut file = private_file(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    Ok(())
}

#[cfg(unix)]
fn private_file(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    Ok(fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?)
}

#[cfg(not(unix))]
fn private_file(path: &Path) -> Result<fs::File> {
    Ok(fs::File::create(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_with_escapes() {
        let config = Config {
            token: Some("tok\"en\\\n\u{1}é".into()),
            model: Some("claude-sonnet-5.5".into()),
            reasoning: Some("high".into()),
            theme: Some("light".into()),
            project: Some(r"C:\work\café".into()),
            reasoning_view: Some("expanded".into()),
            image_model: Some("gpt-image-2.5-flare".into()),
            video_model: Some("veo-3.1-fast".into()),
            audio_model: None,
            auto_update: Some("off".into()),
            browser: Some("brave".into()),
        };
        assert_eq!(Config::parse(&config.serialize()).unwrap(), config);
    }

    #[test]
    fn parses_comments_literals_and_unknown_keys() {
        let text = "# hi\n\ntoken = 'raw\\n' # trailing\nfuture = \"x\"\nmodel=\"a\\u00e9\"\n";
        let config = Config::parse(text).unwrap();
        assert_eq!(config.token.as_deref(), Some("raw\\n"));
        assert_eq!(config.model.as_deref(), Some("aé"));
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["token", "token = x", "token = \"open", "token = \"a\" b", "token = \"\\q\""] {
            assert!(matches!(Config::parse(bad), Err(Error::Config { line: 1, .. })), "{bad}");
        }
    }

    #[test]
    fn save_and_load_file() {
        let dir = std::env::temp_dir().join(format!("serechat-config-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let config = Config { token: Some("abc".into()), ..Config::default() };
        config.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path).unwrap(), config);
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(Config::load_from(&path).unwrap(), Config::default());
    }
}
