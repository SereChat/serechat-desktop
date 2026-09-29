//! Client library for [SereChat](https://serechat.com).
//!
//! * [`Client`]: device-code sign-in, model listing and streaming responses.
//! * [`Config`]: the user's settings in `~/.serechat/config.toml`.
//! * [`SessionStore`]: saved conversations in `~/.serechat/sessions/`.
//!
//! All network calls are blocking; run them off the UI thread.

mod client;
mod config;
mod error;
mod responses;
mod session;
mod sse;

pub use client::{AccessToken, BASE_URL, Client, Model};
pub use config::Config;
pub use error::{Error, Result};
pub use responses::{Completion, Message, ResponseRequest, Role, StreamEvent, Usage};
pub use session::{Session, SessionStore, StoredMessage, new_session_id, unix_now};
