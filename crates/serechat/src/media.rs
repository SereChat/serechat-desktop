//! Image, video and audio generation (`/v1/images`, `/v1/videos`, `/v1/audio`).
//!
//! A generation is a job on the server: [`Client::generate_media`] submits
//! one and returns its id at once, [`Client::media_status`] polls it, and
//! [`Client::download`] fetches the finished file. Because the job outlives
//! the request, a client that restarts keeps polling it by id; the reply
//! waiting for it saves a [`MediaJob`].

use std::collections::HashMap;
use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::client::{Client, read_json};
use crate::error::{Error, Result};

/// USD per Spark, the unit generations are billed in (the server's
/// `SPARK_CONFIG.sparkValueUsd`).
pub const SPARK_USD: f64 = 0.01;

/// What a generation makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    /// Pictures (`/v1/images`).
    Image,
    /// Clips (`/v1/videos`).
    Video,
    /// Speech and music (`/v1/audio`).
    Audio,
}

impl MediaKind {
    /// Every kind.
    pub const ALL: [Self; 3] = [Self::Image, Self::Video, Self::Audio];

    /// The API's path segment.
    fn path(self) -> &'static str {
        match self {
            Self::Image => "images",
            Self::Video => "videos",
            Self::Audio => "audio",
        }
    }

    /// Lower-case name, e.g. `image`; also its slash command.
    #[must_use]
    pub fn noun(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }

    /// Position in [`MediaKind::ALL`], for per-kind arrays.
    #[must_use]
    pub fn index(self) -> usize {
        self as usize
    }
}

/// One way of using a model, e.g. `generate` or `edit`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MediaMode {
    /// Sent as `mode`.
    pub id: String,
}

/// The inputs of one mode.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct MediaSchema {
    /// Parameters the mode cannot do without.
    #[serde(default)]
    pub required: Vec<String>,
    /// Every parameter it accepts, by name.
    #[serde(default)]
    pub properties: serde_json::Map<String, Value>,
}

/// A generation model offered by the API.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MediaModel {
    /// Identifier sent in requests, e.g. `gpt-image-2.5-flare`.
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Price in words, e.g. `$0.10 per second`.
    #[serde(default)]
    pub pricing: String,
    /// The mode used when a request names none.
    #[serde(default)]
    pub default_mode: String,
    /// Every mode, in the server's order.
    #[serde(default)]
    pub modes: Vec<MediaMode>,
    /// Each mode's inputs, by mode id.
    #[serde(default)]
    pub schemas: HashMap<String, MediaSchema>,
}

impl MediaModel {
    /// The mode that works from a text prompt alone: the default mode if it
    /// does, otherwise the first that does. `None` when every mode needs a
    /// file or other input (upscalers, background removal, …).
    #[must_use]
    pub fn prompt_mode(&self) -> Option<&str> {
        let prompt_only = |s: &MediaSchema| {
            s.required.iter().all(|r| r == "prompt" || r == "text") && (s.properties.contains_key("prompt") || s.properties.contains_key("text"))
        };
        std::iter::once(self.default_mode.as_str())
            .chain(self.modes.iter().map(|m| m.id.as_str()))
            .find(|mode| self.schemas.get(*mode).is_some_and(prompt_only))
    }

    /// The name to show, falling back to the id.
    #[must_use]
    pub fn label(&self) -> &str {
        if self.name.is_empty() { &self.id } else { &self.name }
    }
}

/// A submitted generation.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaTicket {
    /// Job id to poll.
    pub id: String,
    /// What it costs, in Sparks (see [`SPARK_USD`]).
    pub sparks: f64,
}

/// Where a job stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaStatus {
    /// Queued or being made.
    Pending,
    /// Done: the file to download, and its MIME type when the server said.
    Succeeded {
        /// Absolute URL of the file.
        url: String,
        /// MIME type, e.g. `image/png`.
        mime: Option<String>,
    },
    /// It failed (and the server refunded it); the reason.
    Failed(String),
}

/// A generation saved with the reply waiting for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaJob {
    /// What it makes.
    pub kind: MediaKind,
    /// Job id; empty until the server accepted the request.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
}

impl Client {
    /// Lists the models of one kind.
    ///
    /// # Errors
    /// Network failure or a non-success response.
    pub fn media_models(&self, kind: MediaKind) -> Result<Vec<MediaModel>> {
        #[derive(Deserialize)]
        struct Body {
            data: Vec<MediaModel>,
        }
        let body: Body = read_json(self.get(&self.url(&format!("/v1/{}", kind.path())), false)?)?;
        Ok(body.data)
    }

    /// Submits a generation from a text `prompt` and returns its ticket
    /// without waiting for it. `mode` `None` uses the model's default.
    ///
    /// # Errors
    /// Network failure or an API error, e.g. `insufficient_sparks` (402) or
    /// `content_policy_violation` (422).
    pub fn generate_media(&self, kind: MediaKind, model: &str, mode: Option<&str>, prompt: &str) -> Result<MediaTicket> {
        #[derive(Deserialize)]
        struct Body {
            id: String,
            #[serde(default)]
            sparks_used: f64,
        }
        let mut body = json!({ "model": model, "prompt": prompt, "async": true });
        if let Some(mode) = mode {
            body["mode"] = json!(mode);
        }
        let ticket: Body = self.post_json(&format!("/v1/{}/generations", kind.path()), &body, true)?;
        if !valid_job_id(&ticket.id) {
            return Err(Error::Response { code: None, message: "The server returned an invalid job id.".into() });
        }
        Ok(MediaTicket { id: ticket.id, sparks: ticket.sparks_used })
    }

    /// Checks on job `id`.
    ///
    /// # Errors
    /// An invalid id, a network failure or a non-success response.
    pub fn media_status(&self, kind: MediaKind, id: &str) -> Result<MediaStatus> {
        if !valid_job_id(id) {
            return Err(Error::Response { code: None, message: "Invalid job id.".into() });
        }
        let value: Value = read_json(self.get(&self.url(&format!("/v1/{}/generations/{id}", kind.path())), true)?)?;
        Ok(self.parse_status(&value))
    }

    /// Interprets a status payload; relative file URLs resolve against the API.
    fn parse_status(&self, value: &Value) -> MediaStatus {
        let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
        match value.get("status").and_then(Value::as_str) {
            Some("succeeded") => {
                let output = value.get("output").unwrap_or(&Value::Null);
                match text(output, "url").or_else(|| text(value, "resultUrl")) {
                    Some(url) => {
                        let url = if url.starts_with('/') { self.url(&url) } else { url };
                        MediaStatus::Succeeded { url, mime: text(output, "mime_type").or_else(|| text(value, "mimeType")) }
                    }
                    None => MediaStatus::Failed("The generation finished without a file.".into()),
                }
            }
            Some("failed") => {
                let message = value.get("error").and_then(|e| text(e, "message")).unwrap_or_else(|| "The generation failed.".into());
                MediaStatus::Failed(message)
            }
            _ => MediaStatus::Pending,
        }
    }

    /// Downloads `url` into `out`, refusing files over `limit` bytes. The
    /// token is sent only to the API's own origin. Returns the content type.
    ///
    /// # Errors
    /// Not an `http(s)` URL, a network failure, a non-success status, a write
    /// failure, or a file over the limit.
    pub fn download(&self, url: &str, limit: u64, out: &mut impl Write) -> Result<Option<String>> {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(Error::Io(std::io::Error::other("only http(s) files can be downloaded")));
        }
        let mut response = self.get(url, true)?;
        let content_type = response.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_owned);
        let mut body = response.body_mut().with_config().limit(limit.saturating_add(1)).reader().take(limit.saturating_add(1));
        let copied = std::io::copy(&mut body, out)?;
        if copied > limit {
            return Err(Error::Io(std::io::Error::other(format!("the file is larger than {} MB", limit >> 20))));
        }
        Ok(content_type)
    }
}

/// Job ids go into URL paths: only UUID-like ids are accepted.
fn valid_job_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_pick_a_prompt_only_mode() {
        #[derive(Deserialize)]
        struct Body {
            data: Vec<MediaModel>,
        }
        let json = r#"{"data":[
            {"id":"flare","name":"Flare","pricing":"$0.04","default_mode":"generate","modes":[{"id":"generate","label":"G"},{"id":"edit","label":"E"}],
             "schemas":{"generate":{"required":["prompt"],"properties":{"prompt":{}}},"edit":{"required":["image_urls","prompt"],"properties":{}}}},
            {"id":"song","default_mode":"tags","modes":[{"id":"tags"},{"id":"prompt"}],
             "schemas":{"tags":{"required":["tags"],"properties":{"tags":{}}},"prompt":{"required":["prompt"],"properties":{"prompt":{}}}}},
            {"id":"tts","default_mode":"generate","modes":[{"id":"generate"}],"schemas":{"generate":{"required":["text"],"properties":{"text":{}}}}},
            {"id":"upscaler","default_mode":"upscale","modes":[{"id":"upscale"}],"schemas":{"upscale":{"required":["image_url"],"properties":{"image_url":{}}}}}
        ]}"#;
        let models = serde_json::from_str::<Body>(json).unwrap().data;
        let picked: Vec<Option<&str>> = models.iter().map(MediaModel::prompt_mode).collect();
        assert_eq!(picked, [Some("generate"), Some("prompt"), Some("generate"), None]);
        assert_eq!((models[0].label(), models[1].label()), ("Flare", "song"));
    }

    #[test]
    fn statuses() {
        let client = Client::new(None);
        let status = |json: &str| client.parse_status(&serde_json::from_str(json).unwrap());
        assert_eq!(status(r#"{"status":"processing","output":null}"#), MediaStatus::Pending);
        assert_eq!(status(r#"{"status":"queued"}"#), MediaStatus::Pending);
        assert_eq!(
            status(r#"{"status":"succeeded","output":{"url":"/api/files/f1","mime_type":"image/png"}}"#),
            MediaStatus::Succeeded { url: client.url("/api/files/f1"), mime: Some("image/png".into()) }
        );
        assert_eq!(status(r#"{"status":"failed","error":{"message":"Blocked"}}"#), MediaStatus::Failed("Blocked".into()));
        assert!(matches!(status(r#"{"status":"succeeded","output":null}"#), MediaStatus::Failed(_)));
    }

    #[test]
    fn tokens_stay_home() {
        let client = Client::new(Some("t".into()));
        assert!(client.owns(&client.url("/api/files/x")));
        assert!(!client.owns(&format!("{}.evil.com/x", client.url(""))), "a look-alike host never gets the token");
        assert!(!client.owns("https://cdn.example.com/a.png"));
        assert!(valid_job_id("4c660f2d-2ee0-4f58-8f8e-8dfd52d4e0f6"));
        assert!(!valid_job_id("../x") && !valid_job_id("") && !valid_job_id("a?b"));
        let mut sink = Vec::new();
        assert!(client.download("file:///etc/passwd", 10, &mut sink).is_err());
    }

    #[test]
    fn jobs_serialize_compactly() {
        let job = MediaJob { kind: MediaKind::Video, id: String::new() };
        assert_eq!(serde_json::to_string(&job).unwrap(), r#"{"kind":"video"}"#);
        assert_eq!(MediaKind::ALL.map(MediaKind::index), [0, 1, 2]);
    }
}
