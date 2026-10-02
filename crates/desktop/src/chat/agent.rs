//! The agent loop: streaming replies, running tool calls, retrying failed
//! requests and compacting conversations that outgrow the context window.
//!
//! A prompt starts a run. A reply may request tool calls: reading tools start
//! at once, the rest wait for approval in the chat. When every call of a reply
//! has a result, the next request goes out with them, until the model answers
//! without tools or the run has taken [`STEP_BUDGET`] rounds and pauses.
//!
//! Failures: dropped or silent connections, rate limits and server errors are
//! retried after [`RETRY_DELAYS`]. A conversation whose last request used more
//! than [`compact_limit`] tokens, or that the server rejects as too long, is
//! first replaced (for the model only) by a summary the model writes. A reply
//! cut off by the output limit never runs its tool calls, whose arguments may
//! be truncated; the model is told and tries again.
//!
//! Every change is saved, so a run interrupted by a crash, a restart or the
//! user shows a Continue button (see [`Conversation::resumable`]).

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serechat::{
    Attachment, Completion, Error, InputItem, Part, Role, StoredMessage, StreamEvent, ToolCall, ToolRecord, ToolStatus, Usage, data_url, unix_now,
};

use super::composer::Command;
use super::{Chat, Conversation, Decision, Entry, Load, Reasoning, StreamingCall};
use crate::app::Action;
use crate::skills::{Catalog, Skill};
use crate::{attachments, tools};

/// Tool rounds a run may take before it pauses for the user.
pub const STEP_BUDGET: u32 = 100;
/// Wait before each retry of a failed request; one retry per entry.
const RETRY_DELAYS: [Duration; 5] =
    [Duration::from_secs(2), Duration::from_secs(5), Duration::from_secs(15), Duration::from_secs(30), Duration::from_secs(60)];
/// Retries a request gets before its error is shown.
pub(super) const MAX_RETRIES: usize = RETRY_DELAYS.len();
/// A stream this quiet is dead: the server pings every 15 seconds.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Context window assumed when the server reports none.
const DEFAULT_WINDOW: u64 = 128_000;
/// Most context a request may use before the conversation is compacted,
/// whatever the window: long prompts cost more and models lose focus.
// ponytail: fixed cap; make it a setting if people want to spend more per step.
const MAX_CONTEXT: u64 = 250_000;

/// Asks for the summary that replaces a conversation's history.
const COMPACT_PROMPT: &str = "The conversation is about to exceed the context window, so everything above will be replaced by a \
    summary that you write now; the next turn sees only the summary. Write it as a handoff to yourself: the user's requests and \
    constraints (quote them where the wording matters); what has been done, including files created or changed and commands run \
    with their results; key findings and decisions; open problems; the plan; and the exact next step. Be specific: paths, names, \
    error messages. Reply with the summary only, without calling tools.";
/// Introduces a summary when it is sent in place of the history.
const SUMMARY_INTRO: &str = "Earlier messages were replaced by this summary to fit the context window. Continue from where it leaves off.";
/// What the model is asked when the user sends `/init`.
const INIT_PROMPT: &str = "Write an AGENTS.md file at the root of this project for coding agents that will work in it. Look through the \
    project first instead of guessing: what it is, how to build, test and run it, how the code is laid out, the conventions to follow, \
    and anything surprising. Keep it short and specific to this project. If an AGENTS.md already exists, improve it rather than \
    starting over.";
/// Screenshots a request carries, newest first. Each costs the model about
/// a thousand tokens, and old ones rarely matter.
const MAX_SCREENSHOTS: usize = 3;
/// Output of a call from a reply that hit the output limit.
const TRUNCATED_CALL: &str = "Not run: your reply reached the output limit before this call was complete, so its arguments may be \
    cut off. Split the work into smaller steps, e.g. write a large file in parts with edit_file.";

/// A request being streamed.
pub(super) struct ActiveStream {
    pub id: u64,
    pub cancel: Arc<AtomicBool>,
    /// Model answering, for pricing the reply.
    pub model: String,
    /// When the request went out, for timing the model's thinking.
    pub started: Instant,
    /// Entry the reply streams into.
    pub entry: u64,
    /// Retries already spent on this request.
    pub attempt: usize,
    /// Last sign of life from the server.
    pub last_event: Instant,
    /// Why the response stopped early, once it has.
    pub incomplete: Option<String>,
    /// What the server billed for it if it failed.
    pub charged: Usage,
}

/// A failed request waiting to be sent again.
pub(super) struct Retry {
    /// When to send it.
    pub at: Instant,
    /// Retries spent, including this one.
    pub attempt: usize,
    /// It asks for a summary rather than a reply.
    pub compaction: bool,
    /// What went wrong, shown while waiting.
    pub error: String,
}

/// Everything a worker thread needs to stream one reply.
pub struct SendJob {
    /// Conversation the reply belongs to.
    pub conversation: u64,
    /// Identifies this stream so late events from a stopped one are dropped.
    pub stream: u64,
    /// Model identifier.
    pub model: String,
    /// Reasoning effort, or `None` for the model default.
    pub reasoning: Option<&'static str>,
    /// System instructions.
    pub instructions: String,
    /// Conversation so far; attachments are read on the worker.
    pub history: Vec<StoredMessage>,
    /// Project folder, whose AGENTS.md and skills the worker adds.
    pub project: Option<String>,
    /// The user's skills, if scanned; the worker scans them otherwise.
    pub user_skills: Option<Arc<Catalog>>,
    /// The project's skills and AGENTS.md, if scanned; likewise.
    pub project_skills: Option<Arc<Catalog>>,
    /// Offer the agent's tools.
    pub tools: bool,
    /// `tool_choice`, or `None` to let the model decide.
    pub tool_choice: Option<&'static str>,
    /// Raised to abort the stream.
    pub cancel: Arc<AtomicBool>,
}

/// A tool call to run on a worker thread.
pub struct ToolJob {
    /// Conversation that made the call.
    pub conversation: u64,
    /// The call.
    pub call: ToolCall,
    /// Project folder the tool is confined to; `None` in a chat without one,
    /// where only `use_skill` runs.
    pub root: Option<PathBuf>,
    /// The skills `use_skill` may load.
    pub skills: Arc<[Skill]>,
    /// Raised to stop it.
    pub cancel: Arc<AtomicBool>,
}

/// Converts saved messages into API input, starting at the latest summary.
/// Reads attachments from disk, so call it on a worker thread.
///
/// # Errors
/// An attachment could not be read.
pub fn input_items(history: &[StoredMessage]) -> Result<Vec<InputItem>, String> {
    let start = history.iter().rposition(is_summary).unwrap_or(0);
    // Only the latest screenshots are sent; older ones stay as their text.
    let recent: Vec<&str> = history[start..]
        .iter()
        .rev()
        .filter(|m| !m.failed)
        .flat_map(|m| m.tool_calls.iter().rev())
        .filter(|r| r.status.is_finished() && r.image.is_some())
        .take(MAX_SCREENSHOTS)
        .map(|r| r.call.call_id.as_str())
        .collect();
    let mut items = Vec::with_capacity(history.len() - start);
    for message in history[start..].iter().filter(|m| !m.failed) {
        match message.role {
            _ if message.compaction => items.push(InputItem::text(Role::User, format!("{SUMMARY_INTRO}\n\n{}", message.content))),
            Role::User => {
                // `/init` shows as typed; the model gets what it stands for.
                let text = if message.content == "/init" { INIT_PROMPT } else { &message.content };
                items.push(InputItem::Message { role: Role::User, parts: attachments::parts(text, &message.attachments)? });
            }
            Role::Assistant => {
                if !message.content.is_empty() {
                    items.push(InputItem::Message { role: Role::Assistant, parts: vec![Part::Text(message.content.clone())] });
                }
                // A generated file: the model learns it exists, not its bytes.
                if let (Some(job), Some(file)) = (&message.media, message.attachments.first()) {
                    items.push(InputItem::text(Role::Assistant, format!("(Generated the {} {} from the prompt above.)", job.kind.noun(), file.name)));
                }
                // Only answered calls go back; the API needs an output for each.
                for record in message.tool_calls.iter().filter(|r| r.status.is_finished()) {
                    items.push(InputItem::ToolCall(record.call.clone()));
                    items.push(InputItem::ToolOutput { call_id: record.call.call_id.clone(), output: record.output.clone() });
                }
                // Tool outputs are text only, so screenshots follow as a user turn.
                let shots: Vec<&ToolRecord> = message.tool_calls.iter().filter(|r| recent.contains(&r.call.call_id.as_str())).collect();
                if !shots.is_empty() {
                    items.push(InputItem::Message { role: Role::User, parts: screenshot_parts(&shots) });
                }
            }
        }
    }
    Ok(items)
}

/// The images of tool calls `records`, each introduced by its call. A
/// screenshot whose file is gone becomes a note rather than failing the
/// request.
fn screenshot_parts(records: &[&ToolRecord]) -> Vec<Part> {
    let mut parts = Vec::with_capacity(records.len() * 2);
    for record in records {
        let Some(image) = &record.image else { continue };
        parts.push(Part::Text(format!("The screenshot from your {} call {}:", record.call.name, record.call.call_id)));
        parts.push(match fs::read(&image.path) {
            Ok(bytes) => Part::Image(data_url(&image.mime, &bytes)),
            Err(_) => Part::Text("(The screenshot's file is missing.)".to_owned()),
        });
    }
    parts
}

/// A finished summary, which the model's view of the conversation starts from.
fn is_summary(message: &StoredMessage) -> bool {
    message.compaction && !message.failed && !message.content.is_empty()
}

/// System instructions for a conversation. They never change within one, so
/// the provider can cache the prompt prefix.
pub(super) fn instructions(project: Option<&str>) -> String {
    let os = std::env::consts::OS;
    match project {
        Some(root) => format!(
            "You are SereChat, an AI assistant and coding agent in a desktop app on {os}. You are working in the project folder `{root}`; \
             tool paths are relative to it. Look at the files with your tools before answering questions about the project, and use \
             them to make changes when asked. Writing files, editing, running commands, fetching URLs and acting in the browser need \
             the user's approval, so say briefly what you are about to do.\n\n\
             For work that takes several steps, keep a plan with update_plan and carry on until the task is done instead of stopping \
             to ask, unless you need a decision only the user can make. Check your changes (build, tests) when the project allows it. \
             Keep each tool call reasonably small: edit files in place rather than rewriting large ones. Answer in Markdown and keep \
             answers focused."
        ),
        None => format!("You are SereChat, a helpful AI assistant in a desktop app on {os}. Answer in Markdown and keep answers focused."),
    }
}

/// Tokens the conversation's next request will use at least: what the
/// latest reply after the latest summary was billed for.
fn context_used(entries: &[Entry]) -> u64 {
    let start = entries.iter().rposition(|e| is_summary(&e.message)).map_or(0, |i| i + 1);
    entries[start..]
        .iter()
        .rev()
        .find(|e| e.message.role == Role::Assistant && !e.message.failed && e.message.usage.input_tokens > 0)
        .map_or(0, |e| e.message.usage.input_tokens + e.message.usage.output_tokens)
}

/// Whether the model owes the conversation a reply: its last message is a
/// prompt, or a round of answered tool calls. Summaries are looked past: one
/// asked for with `/compact` owes nothing, one made mid-run owes what the
/// message before it did. Generations are answered by their file.
fn owes_reply(entries: &[Entry]) -> bool {
    entries.iter().rev().map(|e| &e.message).find(|m| !m.failed && !m.compaction).is_some_and(|m| match m.role {
        Role::User => !matches!(Command::parse(&m.content), Some((Command::Media(_), _))),
        Role::Assistant => !m.tool_calls.is_empty() && m.tool_calls.iter().all(|r| r.status.is_finished()),
    })
}

/// Context use that triggers compaction for a model with `window` tokens.
fn compact_limit(window: u64) -> u64 {
    let window = if window == 0 { DEFAULT_WINDOW } else { window };
    (window / 4 * 3).min(MAX_CONTEXT)
}

impl Conversation {
    /// The model owes a reply and nothing is on its way: the last prompt,
    /// summary or round of tool results was never answered, because the run
    /// failed, paused, was stopped or the app closed. Continue sends it.
    pub(super) fn resumable(&self) -> bool {
        self.load == Load::Loaded && !self.busy() && owes_reply(&self.entries)
    }

    /// Tool rounds and cost of the latest run: everything after the last prompt.
    pub(super) fn run_stats(&self) -> (usize, f64) {
        let start = self.entries.iter().rposition(|e| e.message.role == Role::User && !e.message.failed).map_or(0, |i| i + 1);
        let run = &self.entries[start..];
        (run.iter().filter(|e| !e.message.tool_calls.is_empty()).count(), run.iter().map(|e| e.message.cost).sum())
    }

    /// Answers every open call of the last reply with `output`.
    fn fail_open_calls(&mut self, status: ToolStatus, output: &str) {
        if let Some(calls) = self.tool_calls() {
            for record in calls.iter_mut().filter(|r| !r.status.is_finished()) {
                record.status = status;
                output.clone_into(&mut record.output);
            }
        }
    }
}

impl Chat {
    /// The selected model's context window (`0` when unknown).
    fn context_window(&self) -> u64 {
        self.selected_model().map_or(0, |m| m.context_window)
    }

    /// Sends conversation `id`'s next request: a reply, or first a summary
    /// when the conversation outgrew its context budget.
    pub(super) fn request_reply(&mut self, id: u64, actions: &mut Vec<Action>) {
        let limit = compact_limit(self.context_window());
        let Some(conversation) = self.find(id) else { return };
        let compaction = context_used(&conversation.entries) > limit;
        self.start_request(id, compaction, 0, actions);
    }

    /// Adds the entry a request streams into and starts it. `attempt`
    /// counts the retries already spent on it.
    fn start_request(&mut self, id: u64, compaction: bool, attempt: usize, actions: &mut Vec<Action>) {
        let (entry_id, stream_id) = (self.next_id(), self.next_id());
        let model = self.model.clone();
        let reasoning = Some(self.reasoning_in_use()).filter(|r| *r != Reasoning::Auto).map(Reasoning::key);
        let project = self.find(id).and_then(|c| c.project.clone());
        let (user_skills, project_skills) = self.skill_catalogs(project.as_deref());
        let Some(conversation) = self.find(id) else { return };
        conversation.retry = None;
        conversation.updated = unix_now();
        conversation.tool_cancel = Arc::default();
        let mut history: Vec<StoredMessage> = conversation.entries.iter().map(|e| e.message.clone()).collect();
        let mut message = StoredMessage::new(Role::Assistant, String::new());
        let mut at = history.len();
        if compaction {
            message.compaction = true;
            // A prompt sent just now stays word for word after the summary.
            if history.last().is_some_and(|m| m.role == Role::User && !m.failed) {
                at -= 1;
                history.truncate(at);
            }
            history.push(StoredMessage::new(Role::User, COMPACT_PROMPT.to_owned()));
        }
        conversation.entries.insert(at, Entry::new(entry_id, message));
        let cancel = Arc::new(AtomicBool::new(false));
        let now = Instant::now();
        conversation.stream = Some(ActiveStream {
            id: stream_id,
            cancel: Arc::clone(&cancel),
            model: model.clone(),
            started: now,
            entry: entry_id,
            attempt,
            last_event: now,
            incomplete: None,
            charged: Usage::default(),
        });
        actions.push(Action::SaveSession(conversation.to_session()));
        let job = SendJob {
            conversation: id,
            stream: stream_id,
            model,
            reasoning,
            instructions: instructions(conversation.project.as_deref()),
            history,
            project,
            user_skills,
            project_skills,
            tools: conversation.project.is_some(),
            // Tools stay declared: providers reject tool history without them.
            tool_choice: compaction.then_some("none"),
            cancel,
        };
        if id == self.current {
            self.stick_to_bottom = true;
        }
        actions.push(Action::Send(job));
    }

    fn stream_target(conversations: &mut [Conversation], conversation: u64, stream: u64) -> Option<&mut Conversation> {
        conversations
            .iter_mut()
            .find(|c| c.id == conversation && c.stream.as_ref().is_some_and(|s| s.id == stream))
    }

    /// Applies one streamed update.
    pub fn stream_event(&mut self, conversation: u64, stream: u64, event: StreamEvent) {
        let Some(conversation) = Self::stream_target(&mut self.conversations, conversation, stream) else { return };
        let Conversation { stream: Some(stream), entries, carried_cost, .. } = conversation else { return };
        stream.last_event = Instant::now();
        let Some(entry) = entries.iter_mut().find(|e| e.id == stream.entry) else { return };
        let message = &mut entry.message;
        let elapsed = u64::try_from(stream.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match event {
            StreamEvent::Ping => {}
            StreamEvent::Charged(usage) => stream.charged = usage,
            StreamEvent::ToolCallStarted { index, name } => {
                // Thinking ends where the first call starts.
                if message.content.is_empty() && message.reasoning_ms == 0 {
                    message.reasoning_ms = elapsed;
                }
                entry.streaming_calls.push(StreamingCall { index, name, arguments: String::new(), shown: None });
            }
            StreamEvent::ToolCallDelta { index, delta } => {
                if let Some(call) = entry.streaming_calls.iter_mut().find(|c| c.index == index) {
                    call.arguments.push_str(&delta);
                }
            }
            StreamEvent::Text(delta) => {
                // Models often open with blank lines; don't render them.
                let delta = if message.content.is_empty() { delta.trim_start() } else { &delta };
                // Thinking ends where the answer starts.
                if message.content.is_empty() && !delta.is_empty() {
                    message.reasoning_ms = elapsed;
                }
                message.content.push_str(delta);
            }
            StreamEvent::Reasoning(delta) => message.reasoning.push_str(&delta),
            StreamEvent::Completed(Completion { usage, reasoning, tool_calls, incomplete }) => {
                if message.reasoning.is_empty() {
                    message.reasoning = reasoning;
                }
                // A reply of only tool calls thought until it completed.
                message.reasoning_ms = match (message.reasoning.is_empty(), message.content.is_empty()) {
                    (true, _) => 0,
                    (false, true) => elapsed,
                    (false, false) => message.reasoning_ms,
                };
                // Failed attempts before this reply are billed with it.
                message.cost = self.models.iter().find(|m| m.id == stream.model).map_or(0.0, |m| m.cost(usage)) + std::mem::take(carried_cost);
                message.model = Some(stream.model.clone());
                message.usage = usage;
                message.tool_calls = tool_calls.into_iter().map(|call| ToolRecord { call, status: ToolStatus::Pending, output: String::new(), image: None }).collect();
                entry.streaming_calls.clear();
                stream.incomplete = incomplete;
            }
        }
    }

    /// Finishes a stream: saves the reply and moves the run on, retries, or
    /// shows why it failed. Returns `true` if the server rejected our token.
    pub fn stream_end(&mut self, conversation: u64, stream: u64, result: Result<bool, Error>, actions: &mut Vec<Action>) -> bool {
        let unauthorized = result.as_ref().is_err_and(Error::is_unauthorized);
        let id = conversation;
        let note_id = self.next_id();
        // A stream the user stopped is already detached and ends up here.
        let Some(target) = Self::stream_target(&mut self.conversations, id, stream) else { return false };
        let Some(active) = target.stream.take() else { return false };
        target.updated = unix_now();
        let Some(index) = target.entries.iter().position(|e| e.id == active.entry) else { return false };
        let message = &target.entries[index].message;
        // `server_error` and a missing code are retried; `incomplete` is not.
        let failure = |code: &str, message: &str| Err(Error::Response { code: Some(code.to_owned()), message: message.to_owned() });
        let result = match (result, active.incomplete.as_deref()) {
            (Ok(true), incomplete) if message.content.is_empty() && message.tool_calls.is_empty() => match incomplete {
                None => failure("server_error", "The model returned an empty response."),
                Some("max_output_tokens") => failure("incomplete", "The reply reached the model's output limit before it said anything."),
                Some(reason) => failure("incomplete", &format!("The provider stopped the reply early ({reason}).")),
            },
            (Ok(false), _) => Err(Error::Response { code: None, message: "The connection closed before the reply finished.".into() }),
            (other, _) => other,
        };
        match result {
            Err(error) => self.request_failed(id, &active, index, &error, actions),
            Ok(_) if target.entries[index].message.compaction => self.compacted(id, index, active.incomplete.is_some(), actions),
            Ok(_) => {
                if let Some(reason) = &active.incomplete {
                    Self::cut_short(target, reason, note_id);
                }
                actions.push(Action::SaveSession(target.to_session()));
                // File changes that wait for approval show their diff against the file.
                if let (Some(root), Some(last)) = (&target.project, target.entries.last()) {
                    let waiting = last.message.tool_calls.iter().filter(|r| r.status == ToolStatus::Pending && !target.allowed.contains(&r.call.name));
                    actions.extend(waiting.filter_map(|r| super::preview(std::path::Path::new(root), &r.call, id)));
                }
                if active.incomplete.as_deref().is_none_or(|r| r == "max_output_tokens") {
                    self.advance(id, actions);
                }
            }
        }
        self.attention_if_waiting(id, actions);
        unauthorized
    }

    /// A reply stopped early for `reason`: its calls never run, and a reply
    /// without calls gets a note (entry `note_id`) saying it was cut off.
    fn cut_short(conversation: &mut Conversation, reason: &str, note_id: u64) {
        let cut = reason == "max_output_tokens";
        let has_calls = conversation.tool_calls().is_some();
        if cut {
            conversation.fail_open_calls(ToolStatus::Failed, TRUNCATED_CALL);
        } else {
            conversation.fail_open_calls(ToolStatus::Failed, &format!("Not run: the reply was stopped early ({reason})."));
        }
        if !cut || !has_calls {
            let note = if cut {
                "The reply reached the model's output limit and was cut off.".to_owned()
            } else {
                format!("The provider stopped the reply early ({reason}).")
            };
            let mut message = StoredMessage::new(Role::Assistant, note);
            message.failed = true;
            conversation.entries.push(Entry::new(note_id, message));
        }
    }

    /// A request failed with `error`: compact and resend, retry later, or
    /// give up and show the error.
    fn request_failed(&mut self, id: u64, active: &ActiveStream, index: usize, error: &Error, actions: &mut Vec<Action>) {
        let note_id = self.next_id();
        let billed = self.models.iter().find(|m| m.id == active.model).map_or(0.0, |m| m.cost(active.charged));
        let Some(conversation) = self.find(id) else { return };
        // Billed with the next reply, or with the error if none follows.
        conversation.carried_cost += billed;
        let compaction = conversation.entries[index].message.compaction;
        let can_compact = !compaction && context_used(&conversation.entries) > 0;
        // Compacting or retrying redoes the request; what streamed is dropped.
        if error.is_context_overflow() && can_compact {
            conversation.entries.remove(index);
            self.start_request(id, true, 0, actions);
            return;
        }
        if error.is_retryable() && active.attempt < MAX_RETRIES {
            conversation.entries.remove(index);
            conversation.retry =
                Some(Retry { at: Instant::now() + RETRY_DELAYS[active.attempt], attempt: active.attempt + 1, compaction, error: error.to_string() });
            return;
        }
        let text = if error.is_context_overflow() {
            "This conversation no longer fits the model's context window, even summarised. Start a new chat, or pick a model with a larger window.".to_owned()
        } else if compaction {
            format!("Summarising the conversation failed: {error}")
        } else {
            error.to_string()
        };
        let cost = std::mem::take(&mut conversation.carried_cost);
        let entry = &mut conversation.entries[index];
        entry.streaming_calls.clear();
        if entry.message.content.is_empty() && entry.message.tool_calls.is_empty() {
            entry.message.content = text;
            entry.message.failed = true;
            entry.message.cost += cost;
            entry.doc = None;
        } else {
            let mut message = StoredMessage::new(Role::Assistant, text);
            message.failed = true;
            message.cost = cost;
            conversation.entries.insert(index + 1, Entry::new(note_id, message));
        }
        actions.push(Action::SaveSession(conversation.to_session()));
    }

    /// A summary finished streaming: keep the plan with it and send the
    /// reply it was made for. A summary that was cut off is not trusted.
    fn compacted(&mut self, id: u64, index: usize, cut_off: bool, actions: &mut Vec<Action>) {
        let Some(conversation) = self.find(id) else { return };
        let (before, rest) = conversation.entries.split_at_mut(index);
        let entry = &mut rest[0];
        if cut_off {
            "Summarising the conversation failed: the summary reached the output limit.".clone_into(&mut entry.message.content);
            entry.message.failed = true;
            entry.doc = None;
            actions.push(Action::SaveSession(conversation.to_session()));
            return;
        }
        // The plan survives compaction word for word.
        let plan = before
            .iter()
            .rev()
            .flat_map(|e| e.message.tool_calls.iter().rev())
            .find(|r| r.call.name == "update_plan" && r.status == ToolStatus::Done)
            .and_then(|r| tools::plan_text(&r.call));
        if let Some(plan) = plan {
            let list: Vec<String> = plan.lines().map(|line| format!("- {line}")).collect();
            entry.message.content = format!("{}\n\n**Current plan**\n\n{}", entry.message.content.trim_end(), list.join("\n"));
        }
        // So do the instructions of the skills in use, as the standard asks.
        let mut skills: Vec<(String, &str)> = Vec::new();
        for record in before.iter().flat_map(|e| &e.message.tool_calls).filter(|r| r.call.name == "use_skill" && r.status == ToolStatus::Done) {
            let args: serde_json::Value = serde_json::from_str(&record.call.arguments).unwrap_or_default();
            if let (Some(name), None) = (args.get("name").and_then(serde_json::Value::as_str), args.get("file")) {
                skills.retain(|(n, _)| n != name);
                skills.push((name.to_owned(), &record.output));
            }
        }
        if !skills.is_empty() {
            let loaded: Vec<&str> = skills.iter().map(|(_, output)| *output).collect();
            entry.message.content = format!("{}\n\n**Skills in use**\n\n{}", entry.message.content.trim_end(), loaded.join("\n\n"));
        }
        actions.push(Action::SaveSession(conversation.to_session()));
        // Mid-run, the reply it was made for follows; after `/compact`, nothing.
        if conversation.resumable() {
            self.start_request(id, false, 0, actions);
        }
    }

    /// Summarises the open conversation now (`/compact`): later requests
    /// start from the summary.
    pub(super) fn compact(&mut self, actions: &mut Vec<Action>) {
        let conversation = self.current();
        if conversation.load != Load::Loaded || conversation.busy() {
            self.notify("Wait for the reply to finish, then summarise.");
            return;
        }
        if context_used(&conversation.entries) == 0 {
            self.notify("There is nothing to summarise yet.");
            return;
        }
        let id = conversation.id;
        self.start_request(id, true, 0, actions);
    }

    /// Starts tool calls that may run on their own, and continues the
    /// conversation once every call of the last reply has a result.
    fn advance(&mut self, id: u64, actions: &mut Vec<Action>) {
        let Some(project) = self.find(id).map(|c| c.project.clone()) else { return };
        let skills = self.skills_for(project.as_deref());
        let Some(conversation) = self.find(id) else { return };
        let root = project.map(PathBuf::from);
        let allowed = conversation.allowed.clone();
        let cancel = Arc::clone(&conversation.tool_cancel);
        let Some(calls) = conversation.tool_calls() else { return };
        for record in calls.iter_mut().filter(|r| r.status == ToolStatus::Pending) {
            // Skills work in every chat; the other tools need a project.
            if root.is_none() && record.call.name != "use_skill" {
                record.status = ToolStatus::Failed;
                "Tools are only available in a project.".clone_into(&mut record.output);
                continue;
            }
            if !tools::needs_approval(&record.call.name) || allowed.contains(&record.call.name) {
                record.status = ToolStatus::Running;
                let (call, root, skills, cancel) = (record.call.clone(), root.clone(), Arc::clone(&skills), Arc::clone(&cancel));
                actions.push(Action::RunTool(ToolJob { conversation: id, call, root, skills, cancel }));
            }
        }
        if !calls.iter().all(|r| r.status.is_finished()) {
            return;
        }
        conversation.steps += 1;
        if conversation.steps > STEP_BUDGET {
            let next = self.next_id();
            let Some(conversation) = self.find(id) else { return };
            let mut message = StoredMessage::new(Role::Assistant, format!("Paused after {STEP_BUDGET} tool rounds."));
            message.failed = true;
            conversation.entries.push(Entry::new(next, message));
            actions.push(Action::SaveSession(conversation.to_session()));
            return;
        }
        self.request_reply(id, actions);
    }

    /// Stores a finished tool call's result (and the image it returned, if
    /// any) and moves the agent on.
    pub fn tool_done(&mut self, conversation: u64, call_id: &str, result: Result<String, String>, image: Option<Attachment>, actions: &mut Vec<Action>) {
        let Some(target) = self.find(conversation) else { return };
        let Some(record) = target
            .entries
            .iter_mut()
            .rev()
            .flat_map(|e| e.message.tool_calls.iter_mut())
            .find(|r| r.call.call_id == call_id && r.status == ToolStatus::Running)
        else {
            return;
        };
        (record.status, record.output) = match result {
            Ok(output) => (ToolStatus::Done, output),
            Err(error) => (ToolStatus::Failed, error),
        };
        record.image = image;
        let call = record.call.clone();
        let project = target.project.clone();
        actions.push(Action::SaveSession(target.to_session()));
        // The call may have changed the project's skills or AGENTS.md.
        self.after_tool(project.as_deref(), &call, actions);
        self.advance(conversation, actions);
        self.attention_if_waiting(conversation, actions);
    }

    /// Answers an approval prompt for tool call `index` of entry `entry`.
    pub(super) fn decide(&mut self, entry: usize, index: usize, decision: Decision, actions: &mut Vec<Action>) {
        let id = self.current;
        let conversation = self.current();
        let Some(record) = conversation.entries.get_mut(entry).and_then(|e| e.message.tool_calls.get_mut(index)) else { return };
        if record.status != ToolStatus::Pending {
            return;
        }
        match decision {
            Decision::Deny => {
                record.status = ToolStatus::Denied;
                "The user declined this tool call.".clone_into(&mut record.output);
            }
            Decision::Always => {
                let name = record.call.name.clone();
                conversation.allowed.insert(name);
            }
            Decision::Allow => {
                // Approve just this call: run it right away.
                let root = conversation.project.clone().map(PathBuf::from);
                if let Some(root) = root {
                    record.status = ToolStatus::Running;
                    // Calls that need approval never load skills.
                    actions.push(Action::RunTool(ToolJob {
                        conversation: id,
                        call: record.call.clone(),
                        root: Some(root),
                        skills: Arc::new([]),
                        cancel: Arc::clone(&conversation.tool_cancel),
                    }));
                }
            }
        }
        actions.push(Action::SaveSession(self.current().to_session()));
        self.advance(id, actions);
    }

    /// Continues a run that failed, paused or was interrupted.
    pub(super) fn resume(&mut self, id: u64, actions: &mut Vec<Action>) {
        let Some(conversation) = self.find(id).filter(|c| c.resumable()) else { return };
        conversation.steps = 0;
        self.request_reply(id, actions);
    }

    /// Stops the current conversation's stream, retry and tools, keeping
    /// partial output.
    pub(super) fn stop(&mut self, actions: &mut Vec<Action>) {
        let conversation = self.current();
        let mut stopped = conversation.retry.take().is_some();
        if let Some(stream) = conversation.stream.take() {
            stream.cancel.store(true, Ordering::Relaxed);
            conversation.entries.retain(|e| e.id != stream.entry || !e.message.content.is_empty() || !e.message.tool_calls.is_empty());
            stopped = true;
        }
        conversation.tool_cancel.store(true, Ordering::Relaxed);
        if let Some(calls) = conversation.tool_calls() {
            // Pending calls were never approved; running ones are cut short.
            for record in calls.iter_mut().filter(|r| !r.status.is_finished()) {
                record.status = if record.status == ToolStatus::Running { ToolStatus::Failed } else { ToolStatus::Denied };
                "Stopped by the user.".clone_into(&mut record.output);
                stopped = true;
            }
        }
        if stopped {
            actions.push(Action::SaveSession(conversation.to_session()));
        }
    }

    /// Sends due retries and gives up on streams that went silent.
    pub fn tick(&mut self, now: Instant, actions: &mut Vec<Action>) {
        let mut due = Vec::new();
        let mut silent = Vec::new();
        for c in &self.conversations {
            if let Some(retry) = c.retry.as_ref().filter(|r| r.at <= now) {
                due.push((c.id, retry.compaction, retry.attempt));
            }
            if let Some(stream) = c.stream.as_ref().filter(|s| now.saturating_duration_since(s.last_event) > IDLE_TIMEOUT) {
                stream.cancel.store(true, Ordering::Relaxed);
                silent.push((c.id, stream.id));
            }
        }
        for (id, compaction, attempt) in due {
            self.start_request(id, compaction, attempt, actions);
        }
        for (id, stream) in silent {
            let quiet = Error::Response { code: None, message: "The connection went quiet.".into() };
            self.stream_end(id, stream, Err(quiet), actions);
        }
    }

    /// When [`Chat::tick`] next has something to do.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        let deadline = |c: &Conversation| c.retry.as_ref().map(|r| r.at).or_else(|| c.stream.as_ref().map(|s| s.last_event + IDLE_TIMEOUT));
        self.conversations.iter().filter_map(deadline).min()
    }

    /// Asks for the user's attention when conversation `id` stopped working:
    /// the run finished, failed, or waits for an approval.
    fn attention_if_waiting(&mut self, id: u64, actions: &mut Vec<Action>) {
        if self.find(id).is_some_and(|c| !c.busy()) {
            actions.push(Action::Attention);
        }
    }

    /// Whether anything streams or runs, which animates the screen.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.conversations.iter().any(|c| c.stream.is_some() || c.running())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_budget() {
        assert_eq!(compact_limit(0), 96_000);
        assert_eq!(compact_limit(200_000), 150_000);
        assert_eq!(compact_limit(1_000_000), MAX_CONTEXT);
    }

    #[test]
    fn history_starts_at_the_latest_summary() {
        let mut summary = StoredMessage::new(Role::Assistant, "did A".into());
        summary.compaction = true;
        let mut failed = summary.clone();
        failed.failed = true;
        let history = [
            StoredMessage::new(Role::User, "old".into()),
            summary,
            StoredMessage::new(Role::User, "next".into()),
            failed,
        ];
        let items = input_items(&history).unwrap();
        assert_eq!(items.len(), 2, "old messages and the failed summary are left out");
        assert!(matches!(&items[0], InputItem::Message { role: Role::User, parts } if matches!(&parts[0], Part::Text(t) if t.ends_with("did A"))));
    }

    #[test]
    fn only_the_latest_screenshots_are_sent() {
        let file = std::env::temp_dir().join(format!("serechat-shot-{}.jpg", std::process::id()));
        fs::write(&file, [0xFF, 0xD8]).unwrap();
        let shot = |id: &str, path: &std::path::Path| ToolRecord {
            call: ToolCall { call_id: id.into(), name: "browser_screenshot".into(), arguments: "{}".into() },
            status: ToolStatus::Done,
            output: "Took a screenshot.".into(),
            image: Some(Attachment { name: "screenshot.jpg".into(), mime: "image/jpeg".into(), size: 2, path: path.to_string_lossy().into_owned(), dimensions: None }),
        };
        let mut first = StoredMessage::new(Role::Assistant, String::new());
        first.tool_calls = vec![shot("a", &file), shot("b", &file)];
        let mut second = StoredMessage::new(Role::Assistant, String::new());
        second.tool_calls = vec![shot("c", &file), shot("d", &file.with_extension("gone"))];
        let items = input_items(&[StoredMessage::new(Role::User, "look".into()), first, second]).unwrap();
        let images: Vec<&[Part]> = items
            .iter()
            .filter_map(|item| match item {
                InputItem::Message { role: Role::User, parts } if parts.len() > 1 => Some(parts.as_slice()),
                _ => None,
            })
            .collect();
        // `a` is too old; `b` follows its own reply's outputs; `d`'s file is gone.
        assert_eq!(images.len(), 2);
        assert!(matches!(images[0], [Part::Text(t), Part::Image(url)] if t.contains("call b") && url.starts_with("data:image/jpeg;base64,")));
        assert!(matches!(images[1], [_, Part::Image(_), Part::Text(t), Part::Text(missing)] if t.contains("call d") && missing.contains("missing")));
        let position = |want: &InputItem| items.iter().position(|i| i == want);
        assert!(position(&InputItem::ToolOutput { call_id: "b".into(), output: "Took a screenshot.".into() }) < items.iter().position(|i| matches!(i, InputItem::Message { role: Role::User, parts } if parts.len() == 2)));
        fs::remove_file(&file).unwrap();
    }
}
