//! Client library for [SereChat](https://serechat.com).
//!
//! * [`Client`]: OAuth tokens (code exchange, refresh, revocation), model
//!   listing, streaming responses with attachments and function tools, and
//!   image, video and audio generation jobs.
//! * [`Config`]: the user's settings in `~/.serechat/config.toml`.
//! * [`SessionStore`]: saved conversations in `~/.serechat/sessions/`.
//! * [`Projects`]: project folders in `~/.serechat/projects.json`.
//!
//! All network calls are blocking; run them off the UI thread.

mod auth;
mod client;
mod config;
mod error;
mod media;
mod projects;
mod responses;
mod session;
mod sse;

pub use auth::{CLIENT_ID, GrantEvent, RESOURCE, SCOPES, Tokens};
pub use client::{BASE_URL, Client, Model};
pub use config::{Config, write_private};
pub use error::{Error, Result};
pub use media::{MAX_UPLOAD, MediaJob, MediaKind, MediaModel, MediaStatus, MediaTicket, SPARK_USD};
pub use projects::{Project, Projects};
pub use responses::{Completion, InputItem, Part, ResponseRequest, Role, StreamEvent, ToolCall, ToolSpec, Usage, base64, data_url};
pub use session::{
    Attachment, SearchHit, Session, SessionStore, SessionSummary, StoredMessage, ToolRecord, ToolStatus, new_session_id, unix_now,
};
pub use sse::{Decoder as SseDecoder, Event as SseEvent};
