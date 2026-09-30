//! Streaming access to `POST /v1/responses`: messages with attachments,
//! function tools and their results.

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

/// Token accounting reported when a response completes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    /// Prompt tokens, including those read from or written to the cache.
    pub input_tokens: u64,
    /// Generated tokens.
    pub output_tokens: u64,
    /// Prompt tokens read from the provider's prompt cache.
    #[serde(skip_serializing_if = "is_zero")]
    pub cached_tokens: u64,
    /// Prompt tokens written to the prompt cache.
    #[serde(skip_serializing_if = "is_zero")]
    pub cache_write_tokens: u64,
}

impl Usage {
    /// Plain input and output counts, without cache details.
    #[must_use]
    pub fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self { input_tokens, output_tokens, ..Self::default() }
    }
}

#[expect(clippy::trivially_copy_pass_by_ref, reason = "serde's skip_serializing_if passes a reference")]
fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// A function call requested by the model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Identifier echoed back with the result.
    pub call_id: String,
    /// Tool name.
    pub name: String,
    /// Arguments as a JSON object in text form.
    pub arguments: String,
}

/// A piece of a user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    /// Plain text.
    Text(String),
    /// An image as a `data:` URL.
    Image(String),
    /// A document (e.g. a PDF) as a `data:` URL.
    File {
        /// File name shown to the model.
        name: String,
        /// The contents as a `data:` URL.
        data_url: String,
    },
}

/// One item of a request's `input`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputItem {
    /// A chat turn.
    Message {
        /// Its author.
        role: Role,
        /// Its contents; assistant turns use text parts only.
        parts: Vec<Part>,
    },
    /// A tool call the model made earlier in the conversation.
    ToolCall(ToolCall),
    /// The result of running a tool call.
    ToolOutput {
        /// The call this answers.
        call_id: String,
        /// What the tool returned (or why it did not run).
        output: String,
    },
}

impl InputItem {
    /// A plain-text message.
    #[must_use]
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self::Message { role, parts: vec![Part::Text(text.into())] }
    }

    /// The Responses API JSON for this item.
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            Self::Message { role: Role::Assistant, parts } => {
                let text: Vec<&str> = parts.iter().filter_map(|p| if let Part::Text(t) = p { Some(t.as_str()) } else { None }).collect();
                json!({ "role": "assistant", "content": text.join("\n\n") })
            }
            Self::Message { role: Role::User, parts } => match parts.as_slice() {
                // A lone text part stays a plain string: every model accepts that.
                [Part::Text(text)] => json!({ "role": "user", "content": text }),
                parts => {
                    let content: Vec<Value> = parts
                        .iter()
                        .map(|part| match part {
                            Part::Text(text) => json!({ "type": "input_text", "text": text }),
                            // SereChat wants the object form; a bare string fails upstream.
                            Part::Image(url) => json!({ "type": "input_image", "image_url": { "url": url } }),
                            Part::File { name, data_url } => json!({ "type": "input_file", "filename": name, "file_data": data_url }),
                        })
                        .collect();
                    json!({ "role": "user", "content": content })
                }
            },
            Self::ToolCall(call) => {
                json!({ "type": "function_call", "call_id": call.call_id, "name": call.name, "arguments": call.arguments })
            }
            Self::ToolOutput { call_id, output } => json!({ "type": "function_call_output", "call_id": call_id, "output": output }),
        }
    }
}

/// A function the model may call.
#[derive(Debug, Clone, Copy)]
pub struct ToolSpec<'a> {
    /// Name the model uses.
    pub name: &'a str,
    /// What it does, for the model.
    pub description: &'a str,
    /// JSON Schema of its arguments.
    pub parameters: &'a Value,
}

/// An incremental update from a streaming response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// A chunk of the visible answer.
    Text(String),
    /// A chunk of the model's reasoning, for servers that stream it.
    Reasoning(String),
    /// A keep-alive from the server; it sends one every 15 seconds while the
    /// model works, so a silent connection means a dead one.
    Ping,
    /// The model started writing a tool call. `index` identifies it in the
    /// [`StreamEvent::ToolCallDelta`]s that follow.
    ToolCallStarted {
        /// Position of the call in the response's output.
        index: u64,
        /// Tool name.
        name: String,
    },
    /// More of a tool call's JSON arguments, for showing it as it is written.
    /// The complete call arrives in [`StreamEvent::Completed`].
    ToolCallDelta {
        /// The call, as in [`StreamEvent::ToolCallStarted`].
        index: u64,
        /// The next piece of the arguments.
        delta: String,
    },
    /// What a failed response was billed for; sent just before the error.
    Charged(Usage),
    /// The response finished, possibly cut short (see [`Completion::incomplete`]).
    Completed(Completion),
}

/// Final details of a finished response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completion {
    /// Token accounting.
    pub usage: Usage,
    /// The model's full reasoning, for servers that only send it at the end;
    /// empty for non-thinking models.
    pub reasoning: String,
    /// Function calls the model wants run before it continues.
    pub tool_calls: Vec<ToolCall>,
    /// Why the response stopped early (`max_output_tokens`,
    /// `content_filter`); `None` when it finished. Tool calls of an
    /// incomplete response may have truncated arguments and must not run.
    pub incomplete: Option<String>,
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
    /// Conversation so far, oldest first.
    pub input: &'a [InputItem],
    /// Functions the model may call; empty for plain chat.
    pub tools: &'a [ToolSpec<'a>],
    /// `auto` (the default when `None`), `none` or `required`.
    pub tool_choice: Option<&'a str>,
}

impl ResponseRequest<'_> {
    /// The request body.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut body = json!({
            "model": self.model,
            "input": self.input.iter().map(InputItem::to_json).collect::<Vec<_>>(),
            "stream": true,
        });
        if let Some(choice) = self.tool_choice {
            body["tool_choice"] = choice.into();
        }
        if let Some(instructions) = self.instructions {
            body["instructions"] = instructions.into();
        }
        if let Some(effort) = self.reasoning {
            body["reasoning"] = json!({ "effort": effort });
        }
        if !self.tools.is_empty() {
            let tools: Vec<Value> = self
                .tools
                .iter()
                .map(|t| json!({ "type": "function", "name": t.name, "description": t.description, "parameters": t.parameters }))
                .collect();
            body["tools"] = tools.into();
        }
        body
    }
}

/// Encodes `bytes` as a `data:` URL of type `mime`.
#[must_use]
pub fn data_url(mime: &str, bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(mime.len() + 13 + bytes.len().div_ceil(3) * 4);
    out.push_str("data:");
    out.push_str(mime);
    out.push_str(";base64,");
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

impl Client {
    /// Streams a response, invoking `on_event` for every update.
    ///
    /// Returns `Ok(true)` when the response completed (or was cut short, see
    /// [`Completion::incomplete`]) and `Ok(false)` when `cancel` was raised or
    /// the stream ended early. `cancel` is checked between SSE lines, which
    /// arrive at least every 15 seconds thanks to the server's keep-alives.
    ///
    /// # Errors
    /// Network failures, non-success statuses, and failures reported inside
    /// the stream ([`Error::Response`]).
    pub fn stream_response(&self, request: &ResponseRequest<'_>, cancel: &AtomicBool, mut on_event: impl FnMut(StreamEvent)) -> Result<bool> {
        let response = self.post("/v1/responses", &request.to_json(), true)?;
        let reader = std::io::BufReader::new(response.into_body().into_reader());
        let mut decoder = sse::Decoder::default();

        // The trailing empty line flushes an event the server did not terminate.
        for line in reader.lines().chain([Ok(String::new())]) {
            if cancel.load(Ordering::Relaxed) {
                return Ok(false);
            }
            let line = line?;
            if line.starts_with(':') {
                on_event(StreamEvent::Ping);
                continue;
            }
            let Some(event) = decoder.line(&line) else { continue };
            if event.name == "response.failed"
                && let Ok(value) = serde_json::from_str::<Value>(&event.data)
                && let Some(usage) = usage_at(&value).filter(|u| *u != Usage::default())
            {
                on_event(StreamEvent::Charged(usage));
            }
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
        "response.output_item.added" if value.pointer("/item/type").and_then(Value::as_str) == Some("function_call") => {
            let name = value.pointer("/item/name").and_then(Value::as_str).unwrap_or_default().to_owned();
            Some(StreamEvent::ToolCallStarted { index: output_index(&value), name })
        }
        "response.function_call_arguments.delta" => Some(StreamEvent::ToolCallDelta { index: output_index(&value), delta: delta() }),
        "response.completed" | "response.incomplete" => {
            let usage = usage_at(&value).unwrap_or_default();
            let incomplete = (kind == "response.incomplete").then(|| {
                let reason = value.pointer("/response/incomplete_details/reason").and_then(Value::as_str);
                reason.unwrap_or("unknown").to_owned()
            });
            Some(StreamEvent::Completed(Completion { usage, reasoning: reasoning_text(&value), tool_calls: tool_calls(&value), incomplete }))
        }
        "error" | "response.failed" => {
            let source = value.pointer("/response/error").unwrap_or(&value);
            let (code, message) = parse_error_body(&source.to_string());
            return Err(Error::Response { code, message: message.unwrap_or_else(|| format!("The response stream reported `{kind}`.")) });
        }
        _ => None,
    })
}

/// An event's `output_index`.
fn output_index(value: &Value) -> u64 {
    value.get("output_index").and_then(Value::as_u64).unwrap_or(0)
}

/// The `response.usage` of a terminal event, if it has one.
fn usage_at(value: &Value) -> Option<Usage> {
    let usage = value.pointer("/response/usage").filter(|u| u.is_object())?;
    let count = |path: &str| usage.pointer(path).and_then(Value::as_u64).unwrap_or(0);
    Some(Usage {
        input_tokens: count("/input_tokens"),
        output_tokens: count("/output_tokens"),
        cached_tokens: count("/input_tokens_details/cached_tokens"),
        cache_write_tokens: count("/input_tokens_details/cache_write_tokens"),
    })
}

fn output_items<'a>(completed: &'a Value, kind: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
    let items = completed.pointer("/response/output").and_then(Value::as_array).into_iter().flatten();
    items.filter(move |item| item.get("type").and_then(Value::as_str) == Some(kind))
}

/// Joins the text of every `reasoning` output item of a completed response,
/// preferring full reasoning over summaries.
fn reasoning_text(completed: &Value) -> String {
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
    for item in output_items(completed, "reasoning") {
        let content = texts(item, "content");
        parts.extend(if content.is_empty() { texts(item, "summary") } else { content });
    }
    parts.join("\n\n")
}

/// The `function_call` output items of a completed response.
fn tool_calls(completed: &Value) -> Vec<ToolCall> {
    output_items(completed, "function_call")
        .filter_map(|item| {
            let field = |key: &str| item.get(key).and_then(Value::as_str).map(str::to_owned);
            Some(ToolCall { call_id: field("call_id").or_else(|| field("id"))?, name: field("name")?, arguments: field("arguments").unwrap_or_default() })
        })
        .collect()
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
            Some(StreamEvent::Completed(Completion { usage: Usage::new(3, 4), ..Completion::default() }))
        );
        let err = parse_event("", r#"{"type":"error","error":{"code":"x","message":"boom"}}"#).unwrap_err();
        assert_eq!(err.to_string(), "boom");
        assert!(parse_event("", "{").is_err());
    }

    #[test]
    fn terminal_events() {
        // Shapes from the server's ResponseBuilder.
        let incomplete = r#"{"type":"response.incomplete","response":{"status":"incomplete",
            "incomplete_details":{"reason":"max_output_tokens"},
            "usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":6,"cache_write_tokens":2},"output_tokens":5}}}"#;
        let Some(StreamEvent::Completed(done)) = parse_event("response.incomplete", incomplete).unwrap() else { panic!("expected a completion") };
        assert_eq!(done.incomplete.as_deref(), Some("max_output_tokens"));
        assert_eq!(done.usage, Usage { input_tokens: 10, output_tokens: 5, cached_tokens: 6, cache_write_tokens: 2 });

        let failed = r#"{"response":{"status":"failed","error":{"code":"context_length_exceeded","message":"Too long."},"usage":{"input_tokens":7,"output_tokens":1}}}"#;
        let err = parse_event("response.failed", failed).unwrap_err();
        assert!(err.is_context_overflow() && !err.is_retryable());
        assert_eq!(err.to_string(), "Too long.");
        assert_eq!(usage_at(&serde_json::from_str(failed).unwrap()), Some(Usage::new(7, 1)));
        assert_eq!(usage_at(&json!({ "response": { "usage": null } })), None);
    }

    #[test]
    fn tool_calls_stream_as_they_are_written() {
        let added = r#"{"output_index":2,"item":{"type":"function_call","name":"write_file","arguments":""}}"#;
        assert_eq!(
            parse_event("response.output_item.added", added).unwrap(),
            Some(StreamEvent::ToolCallStarted { index: 2, name: "write_file".into() })
        );
        let text_item = r#"{"output_index":0,"item":{"type":"message"}}"#;
        assert_eq!(parse_event("response.output_item.added", text_item).unwrap(), None);
        let delta = r#"{"output_index":2,"delta":"{\"path\":"}"#;
        assert_eq!(
            parse_event("response.function_call_arguments.delta", delta).unwrap(),
            Some(StreamEvent::ToolCallDelta { index: 2, delta: r#"{"path":"#.into() })
        );
    }

    #[test]
    fn completion_carries_reasoning_and_tool_calls() {
        // Shapes captured from the live API.
        let completed = r#"{"response":{"output":[
            {"type":"message","content":[{"type":"output_text","text":"391"}]},
            {"type":"reasoning","content":[{"type":"reasoning_text","text":"17*23=391\n"}]},
            {"type":"reasoning","summary":[{"type":"summary_text","text":"Multiplied."}]},
            {"id":"call-1","type":"function_call","name":"list_directory","call_id":"call-1","arguments":"{\"path\":\".\"}","status":"completed"}]}}"#;
        let Some(StreamEvent::Completed(completion)) = parse_event("response.completed", completed).unwrap() else {
            panic!("expected a completion");
        };
        assert_eq!(completion.reasoning, "17*23=391\n\nMultiplied.");
        assert_eq!(
            completion.tool_calls,
            [ToolCall { call_id: "call-1".into(), name: "list_directory".into(), arguments: r#"{"path":"."}"#.into() }]
        );
    }

    #[test]
    fn serializes_input_items() {
        let items = [
            InputItem::text(Role::User, "hi"),
            InputItem::Message { role: Role::User, parts: vec![Part::Text("look".into()), Part::Image("data:image/png;base64,AA==".into())] },
            InputItem::text(Role::Assistant, "ok"),
            InputItem::ToolCall(ToolCall { call_id: "c".into(), name: "n".into(), arguments: "{}".into() }),
            InputItem::ToolOutput { call_id: "c".into(), output: "done".into() },
        ];
        let json: Vec<String> = items.iter().map(|i| i.to_json().to_string()).collect();
        assert_eq!(json[0], r#"{"content":"hi","role":"user"}"#);
        assert_eq!(
            json[1],
            r#"{"content":[{"text":"look","type":"input_text"},{"image_url":{"url":"data:image/png;base64,AA=="},"type":"input_image"}],"role":"user"}"#
        );
        assert_eq!(json[3], r#"{"arguments":"{}","call_id":"c","name":"n","type":"function_call"}"#);
        assert_eq!(json[4], r#"{"call_id":"c","output":"done","type":"function_call_output"}"#);

        let schema = json!({"type": "object"});
        let tools = [ToolSpec { name: "t", description: "d", parameters: &schema }];
        let request = ResponseRequest { model: "m", instructions: Some("be brief"), reasoning: None, input: &items[..1], tools: &tools, tool_choice: None };
        let body = request.to_json();
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["instructions"], "be brief");
        assert!(body.get("tool_choice").is_none());
        assert_eq!(ResponseRequest { tool_choice: Some("none"), ..request }.to_json()["tool_choice"], "none");
    }

    #[test]
    fn base64_data_urls() {
        assert_eq!(data_url("text/plain", b""), "data:text/plain;base64,");
        assert_eq!(data_url("a/b", b"f"), "data:a/b;base64,Zg==");
        assert_eq!(data_url("a/b", b"fo"), "data:a/b;base64,Zm8=");
        assert_eq!(data_url("a/b", b"foo"), "data:a/b;base64,Zm9v");
        assert_eq!(data_url("a/b", &[0xFF, 0xFE, 0xFD, 0x00]), "data:a/b;base64,//79AA==");
    }
}
