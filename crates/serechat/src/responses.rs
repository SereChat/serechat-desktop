//! Streaming access to `POST /v1/responses`.

use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::client::{Client, parse_error_body};
use crate::error::{Error, Result};
use crate::sse;

/// Author of a conversation turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The human.
    User,
    /// The model.
    Assistant,
}

/// One turn of a conversation, serialized as a Responses API input message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Message {
    /// Who wrote it.
    pub role: Role,
    /// Plain-text content.
    pub content: String,
}

/// Token accounting reported when a response completes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    /// Prompt tokens.
    pub input_tokens: u64,
    /// Generated tokens.
    pub output_tokens: u64,
}

/// An incremental update from a streaming response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// A chunk of the visible answer.
    Text(String),
    /// A chunk of the model's reasoning, for servers that stream it.
    Reasoning(String),
    /// The response finished successfully.
    Completed(Completion),
}

/// Final details of a finished response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completion {
    /// Token accounting.
    pub usage: Usage,
    /// The model's full reasoning. SereChat delivers it here rather than as
    /// [`StreamEvent::Reasoning`] deltas; empty for non-thinking models.
    pub reasoning: String,
}

/// Parameters for a single response.
#[derive(Debug, Clone, Copy)]
pub struct ResponseRequest<'a> {
    /// Model identifier.
    pub model: &'a str,
    /// Optional system/developer instructions.
    pub instructions: Option<&'a str>,
    /// Reasoning effort (`none`, `low`, `medium`, `high`); `None` leaves it
    /// to the model.
    pub reasoning: Option<&'a str>,
    /// Conversation so far, oldest first; the last entry is the new prompt.
    pub input: &'a [Message],
}

impl Client {
    /// Streams a response, invoking `on_event` for every update.
    ///
    /// Returns `Ok(true)` when the response completed and `Ok(false)` when
    /// `cancel` was raised or the stream ended early. `cancel` is checked
    /// between SSE lines, so cancellation takes effect at the next token.
    ///
    /// # Errors
    /// Network failures, non-success statuses, and error events inside the stream.
    pub fn stream_response(
        &self,
        request: &ResponseRequest<'_>,
        cancel: &AtomicBool,
        mut on_event: impl FnMut(StreamEvent),
    ) -> Result<bool> {
        let mut body = json!({
            "model": request.model,
            "input": request.input,
            "stream": true,
        });
        if let Some(instructions) = request.instructions {
            body["instructions"] = instructions.into();
        }
        if let Some(effort) = request.reasoning {
            body["reasoning"] = json!({ "effort": effort });
        }
        let response = self.post("/v1/responses", &body, true)?;
        let reader = std::io::BufReader::new(response.into_body().into_reader());
        let mut decoder = sse::Decoder::default();

        // The trailing empty line flushes an event the server did not terminate.
        for line in reader.lines().chain([Ok(String::new())]) {
            if cancel.load(Ordering::Relaxed) {
                return Ok(false);
            }
            let Some(event) = decoder.line(&line?) else { continue };
            if let Some(event) = parse_event(&event.name, &event.data)? {
                let done = matches!(event, StreamEvent::Completed(_));
                on_event(event);
                if done {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

/// Interprets one SSE event. Its kind comes from the SSE `event` field,
/// falling back to the payload's `type`. Unknown kinds are ignored.
fn parse_event(name: &str, data: &str) -> Result<Option<StreamEvent>> {
    if data == "[DONE]" {
        return Ok(Some(StreamEvent::Completed(Completion::default())));
    }
    let value: Value = serde_json::from_str(data)?;
    let kind = if name.is_empty() { value.get("type").and_then(Value::as_str).unwrap_or_default() } else { name };
    let delta = || value.get("delta").and_then(Value::as_str).unwrap_or_default().to_owned();
    Ok(match kind {
        "response.output_text.delta" | "response.text.delta" => Some(StreamEvent::Text(delta())),
        "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" | "response.reasoning.delta" => {
            Some(StreamEvent::Reasoning(delta()))
        }
        "response.completed" => {
            let usage = value.pointer("/response/usage");
            let count = |key| usage.and_then(|u| u.get(key)).and_then(Value::as_u64).unwrap_or(0);
            let usage = Usage { input_tokens: count("input_tokens"), output_tokens: count("output_tokens") };
            Some(StreamEvent::Completed(Completion { usage, reasoning: reasoning_text(&value) }))
        }
        "error" | "response.failed" | "response.incomplete" => {
            let source = value.pointer("/response/error").unwrap_or(&value);
            let (code, message) = parse_error_body(&source.to_string());
            return Err(Error::Api {
                status: 200,
                code,
                message: message.unwrap_or_else(|| format!("the response stream reported `{kind}`")),
            });
        }
        _ => None,
    })
}

/// Joins the text of every `reasoning` output item of a completed response,
/// preferring full reasoning over summaries.
fn reasoning_text(completed: &Value) -> String {
    let items = completed.pointer("/response/output").and_then(Value::as_array).into_iter().flatten();
    let texts = |item: &Value, key: &str| -> Vec<String> {
        item.get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|part| part.get("text").and_then(Value::as_str).map(str::trim))
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let mut parts = Vec::new();
    for item in items.filter(|item| item.get("type").and_then(Value::as_str) == Some("reasoning")) {
        let content = texts(item, "content");
        parts.extend(if content.is_empty() { texts(item, "summary") } else { content });
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stream_events() {
        let text = |s: &str| parse_event("", s).unwrap();
        assert_eq!(text(r#"{"type":"response.created","response":{}}"#), None);
        assert_eq!(text(r#"{"type":"response.output_text.delta","delta":"Hi"}"#), Some(StreamEvent::Text("Hi".into())));
        // SereChat names events only in the SSE `event` field.
        assert_eq!(
            parse_event("response.output_text.delta", r#"{"output_index":0,"delta":"!"}"#).unwrap(),
            Some(StreamEvent::Text("!".into()))
        );
        assert_eq!(
            parse_event("response.completed", r#"{"response":{"usage":{"input_tokens":3,"output_tokens":4}}}"#).unwrap(),
            Some(StreamEvent::Completed(Completion { usage: Usage { input_tokens: 3, output_tokens: 4 }, reasoning: String::new() }))
        );
        // Shape captured from the live API: reasoning only arrives on completion.
        let completed = r#"{"response":{"output":[
            {"type":"message","content":[{"type":"output_text","text":"391"}]},
            {"type":"reasoning","content":[{"type":"reasoning_text","text":"17*23=391\n"}]},
            {"type":"reasoning","summary":[{"type":"summary_text","text":"Multiplied."}]}]}}"#;
        let Some(StreamEvent::Completed(completion)) = parse_event("response.completed", completed).unwrap() else {
            panic!("expected a completion");
        };
        assert_eq!(completion.reasoning, "17*23=391\n\nMultiplied.");
        let err = parse_event("", r#"{"type":"error","error":{"code":"x","message":"boom"}}"#).unwrap_err();
        assert_eq!(err.to_string(), "boom (HTTP 200)");
        assert!(parse_event("", "{").is_err());
    }

    #[test]
    fn serializes_input() {
        let input = [Message { role: Role::User, content: "hi".into() }];
        assert_eq!(serde_json::to_string(&input).unwrap(), r#"[{"role":"user","content":"hi"}]"#);
    }
}
