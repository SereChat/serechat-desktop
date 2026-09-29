//! Main screen: sidebar of saved sessions, messages, composer, menus,
//! Spotlight and the settings page.
//!
//! The screen never touches the disk or network itself: it emits [`Action`]s
//! (send, save, run a tool, import files, …) that the app carries out on
//! worker threads, and receives their results through the `*_done` /
//! `*_loaded` methods.
//!
//! Agent loop: a reply may request tool calls. Reading tools start at once;
//! the rest wait for approval in the chat. When every call of a reply has a
//! result, the conversation continues automatically with those results, up to
//! [`MAX_STEPS`] times per prompt.

mod composer;
mod menu;
mod messages;
mod sidebar;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arboard::Clipboard;
use serechat::{
    Attachment, Completion, Error, InputItem, Model, Part, Project, Role, Session, SessionSummary, StoredMessage, StreamEvent,
    ToolCall, ToolRecord, ToolStatus, Usage, new_session_id, unix_now,
};
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::app::Action;
use crate::doc::Doc;
use crate::editor::Editor;
use crate::paint::{Painter, Rect};
use crate::settings::{SettingsView, Totals};
use crate::spotlight::{Outcome, Pick, Spotlight};
use crate::text::TextLayout;
use crate::theme::{self, Scheme};
use crate::ui::{Ui, copy, edit_key};
use crate::{attachments, tools};

/// Model used until the user picks one.
pub const DEFAULT_MODEL: &str = "claude-sonnet-5.5";
/// Most tool rounds the agent may take for one prompt.
const MAX_STEPS: u32 = 40;
/// Name of the platform's primary shortcut modifier.
const PRIMARY_KEY: &str = if cfg!(target_os = "macos") { "Cmd" } else { "Ctrl" };

/// How much the model should think before answering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reasoning {
    /// Leave it to the model.
    #[default]
    Auto,
    /// No reasoning.
    Off,
    /// Brief reasoning.
    Low,
    /// Balanced reasoning.
    Medium,
    /// Thorough reasoning.
    High,
}

impl Reasoning {
    const ALL: [Self; 5] = [Self::Auto, Self::Off, Self::Low, Self::Medium, Self::High];

    /// Value stored in the config file; also the API's effort name.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Off => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// Parses a config value; unknown values mean [`Reasoning::Auto`].
    #[must_use]
    pub fn from_key(key: Option<&str>) -> Self {
        Self::ALL.into_iter().find(|r| Some(r.key()) == key).unwrap_or_default()
    }

    /// The API's `reasoning.effort`, or `None` to omit it.
    fn effort(self) -> Option<&'static str> {
        (self != Self::Auto).then(|| self.key())
    }

    fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Off => "Off",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
        }
    }

    fn detail(self) -> &'static str {
        match self {
            Self::Auto => "Model default",
            Self::Off => "Answer right away",
            Self::Low => "Think briefly",
            Self::Medium => "Balanced",
            Self::High => "Think it through",
        }
    }
}

/// What the main area shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Chat,
    Settings,
}

/// Drop-down menus.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    Model,
    Reasoning,
    Project,
}

/// A reply being streamed.
struct ActiveStream {
    id: u64,
    cancel: Arc<AtomicBool>,
    /// Model answering, for pricing the reply.
    model: String,
}

/// One message in a conversation, with its cached layouts.
struct Entry {
    id: u64,
    message: StoredMessage,
    /// The reasoning block is expanded.
    show_reasoning: bool,
    /// Tool cards whose output is expanded.
    open_tools: HashSet<usize>,
    doc: Option<Doc>,
    /// Content length and boxed-ness the doc was built for.
    doc_key: (usize, bool),
    reasoning_doc: Option<Doc>,
    reasoning_len: usize,
    /// Per tool card: body layout and what it was laid out for.
    tool_bodies: Vec<ToolBody>,
}

impl Entry {
    fn new(id: u64, message: StoredMessage) -> Self {
        Self {
            id,
            message,
            show_reasoning: false,
            open_tools: HashSet::new(),
            doc: None,
            doc_key: (0, false),
            reasoning_doc: None,
            reasoning_len: 0,
            tool_bodies: Vec::new(),
        }
    }

    /// Drawn inside a bordered box (prompts and errors) rather than bare.
    fn boxed(&self) -> bool {
        self.message.role == Role::User || self.message.failed
    }
}

/// A tool card body layout with the (text length, status, width) it shows.
type ToolBody = Option<((usize, ToolStatus, u32), TextLayout)>;

/// Whether a conversation's messages are in memory.
#[derive(Clone, Debug, PartialEq)]
enum Load {
    /// Known from the index only.
    Summary,
    /// Being read on a worker thread.
    Loading,
    /// `entries` holds every message.
    Loaded,
    /// The file could not be read; shown instead of the messages.
    Failed(String),
}

/// One conversation; saved as a [`Session`] once it has messages.
struct Conversation {
    id: u64,
    session_id: String,
    title: String,
    created: u64,
    updated: u64,
    /// Working directory; enables the agent's tools.
    project: Option<String>,
    load: Load,
    /// Totals from the index, used until the messages are loaded.
    indexed_cost: f64,
    indexed_tokens: u64,
    entries: Vec<Entry>,
    stream: Option<ActiveStream>,
    /// Tools the user allowed to run without asking, for this session.
    allowed: HashSet<String>,
    /// Tool rounds since the last prompt.
    steps: u32,
    /// Raised to stop running tools.
    tool_cancel: Arc<AtomicBool>,
}

impl Conversation {
    fn new(id: u64, project: Option<String>) -> Self {
        let now = unix_now();
        Self {
            id,
            session_id: new_session_id(),
            title: String::new(),
            created: now,
            updated: now,
            project,
            load: Load::Loaded,
            indexed_cost: 0.0,
            indexed_tokens: 0,
            entries: Vec::new(),
            stream: None,
            allowed: HashSet::new(),
            steps: 0,
            tool_cancel: Arc::default(),
        }
    }

    /// A saved session whose messages are read when it is opened.
    fn from_summary(id: u64, summary: SessionSummary) -> Self {
        Self {
            session_id: summary.id,
            title: summary.title,
            created: summary.created,
            updated: summary.updated,
            load: Load::Summary,
            indexed_cost: summary.cost,
            indexed_tokens: summary.tokens,
            ..Self::new(id, summary.project)
        }
    }

    /// A new conversation nobody has written in yet (never saved or listed).
    fn is_fresh(&self) -> bool {
        self.load == Load::Loaded && self.entries.is_empty()
    }

    /// Snapshot for saving; the empty placeholder of a pending reply is skipped.
    fn to_session(&self) -> Session {
        debug_assert_eq!(self.load, Load::Loaded, "saving would drop unloaded messages");
        Session {
            id: self.session_id.clone(),
            title: self.title.clone(),
            created: self.created,
            updated: self.updated,
            project: self.project.clone(),
            messages: self
                .entries
                .iter()
                .filter(|e| !e.message.content.is_empty() || !e.message.tool_calls.is_empty() || !e.message.attachments.is_empty())
                .map(|e| e.message.clone())
                .collect(),
        }
    }

    fn cost(&self) -> f64 {
        if self.load == Load::Loaded { self.entries.iter().map(|e| e.message.cost).sum() } else { self.indexed_cost }
    }

    fn tokens(&self) -> u64 {
        if self.load == Load::Loaded {
            self.entries.iter().map(|e| e.message.usage.input_tokens + e.message.usage.output_tokens).sum()
        } else {
            self.indexed_tokens
        }
    }

    /// The last reply's tool calls, if it made any.
    fn tool_calls(&mut self) -> Option<&mut Vec<ToolRecord>> {
        self.entries
            .last_mut()
            .filter(|e| e.message.role == Role::Assistant && !e.message.tool_calls.is_empty())
            .map(|e| &mut e.message.tool_calls)
    }

    /// Streaming, or a tool is running.
    fn busy(&self) -> bool {
        self.stream.is_some()
            || self.entries.last().is_some_and(|e| e.message.tool_calls.iter().any(|t| t.status == ToolStatus::Running))
    }
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
    /// Offer the agent's tools.
    pub tools: bool,
    /// Raised to abort the stream.
    pub cancel: Arc<AtomicBool>,
}

/// A tool call to run on a worker thread.
pub struct ToolJob {
    /// Conversation that made the call.
    pub conversation: u64,
    /// The call.
    pub call: ToolCall,
    /// Project folder the tool is confined to.
    pub root: PathBuf,
    /// Raised to stop it.
    pub cancel: Arc<AtomicBool>,
}

/// Converts saved messages into API input. Reads attachments from disk, so
/// call it on a worker thread.
///
/// # Errors
/// An attachment could not be read.
pub fn input_items(history: &[StoredMessage]) -> Result<Vec<InputItem>, String> {
    let mut items = Vec::with_capacity(history.len());
    for message in history.iter().filter(|m| !m.failed) {
        match message.role {
            Role::User => items.push(InputItem::Message { role: Role::User, parts: attachments::parts(&message.content, &message.attachments)? }),
            Role::Assistant => {
                if !message.content.is_empty() {
                    items.push(InputItem::Message { role: Role::Assistant, parts: vec![Part::Text(message.content.clone())] });
                }
                // Only answered calls go back; the API needs an output for each.
                for record in message.tool_calls.iter().filter(|r| r.status.is_finished()) {
                    items.push(InputItem::ToolCall(record.call.clone()));
                    items.push(InputItem::ToolOutput { call_id: record.call.call_id.clone(), output: record.output.clone() });
                }
            }
        }
    }
    Ok(items)
}

/// System instructions for a conversation.
fn instructions(project: Option<&str>) -> String {
    let os = std::env::consts::OS;
    match project {
        Some(root) => format!(
            "You are SereChat, an AI assistant and coding agent in a desktop app on {os}. You are working in the project folder `{root}`; \
             tool paths are relative to it. Look at the files with your tools before answering questions about the project, and use \
             them to make changes when asked. Writing files, editing, running commands and fetching URLs need the user's approval, so \
             say briefly what you are about to do. Answer in Markdown and keep answers focused."
        ),
        None => format!("You are SereChat, a helpful AI assistant in a desktop app on {os}. Answer in Markdown and keep answers focused."),
    }
}

/// A position in the open conversation: entry, document (0 reasoning,
/// 1 content), text piece and byte.
type SelPos = (usize, u8, usize, usize);

/// One row of a drop-down menu.
struct MenuItem {
    label: String,
    detail: String,
    selected: bool,
}

/// State of the chat screen.
pub struct Chat {
    conversations: Vec<Conversation>,
    current: u64,
    next_id: u64,
    page: Page,
    settings: SettingsView,
    composer: Editor,
    /// Composer layout and text origin from the last frame, for keyboard
    /// navigation and mouse hit-testing.
    composer_layout: Option<(TextLayout, (f32, f32))>,
    composer_scroll: f32,
    /// Dragging a selection in the composer.
    selecting: bool,
    /// Attachments for the next message.
    pending: Vec<Attachment>,
    /// Files being imported on worker threads.
    importing: usize,
    /// Text being composed by an input method, and its caret.
    preedit: Option<(String, Option<(usize, usize)>)>,
    /// Composer caret rectangle, for placing the input method's window.
    caret_rect: Option<Rect>,
    /// A transient message above the composer and when it appeared.
    notice: Option<(String, Option<f32>)>,
    models: Vec<Model>,
    model: String,
    reasoning: Reasoning,
    projects: Vec<Project>,
    /// Project new conversations open in, and whose sessions are listed.
    project: Option<String>,
    menu: Option<Menu>,
    /// Where the open menu was drawn last frame; blocks hover beneath it.
    menu_rect: Option<Rect>,
    menu_scroll: f32,
    /// The sidebar's project switcher, anchoring the project menu.
    project_button: Rect,
    scroll: f32,
    scroll_target: f32,
    stick_to_bottom: bool,
    /// Message selection: anchor and focus.
    selection: Option<(SelPos, SelPos)>,
    /// Dragging a message selection.
    dragging: bool,
    sidebar_scroll: f32,
    /// Conversation whose delete button was clicked once, awaiting confirmation.
    confirm_delete: Option<u64>,
    /// What was just copied (entry, code block or whole message) and when.
    copied: Option<(u64, Option<usize>, f32)>,
    /// A file is dragged over the window; `true` if it is a folder.
    drop_hover: Option<bool>,
    spotlight: Option<Spotlight>,
}

impl Chat {
    /// The chat screen with the saved `sessions` (newest first) in the
    /// sidebar and a fresh conversation open in `project`.
    #[must_use]
    pub fn new(model: Option<String>, reasoning: Reasoning, sessions: Vec<SessionSummary>, projects: Vec<Project>, project: Option<String>) -> Self {
        // A project whose folder disappeared falls back to plain chat.
        let project = project.filter(|p| std::path::Path::new(p).is_dir());
        let mut chat = Self {
            conversations: Vec::new(),
            current: 0,
            next_id: 1,
            page: Page::Chat,
            settings: SettingsView::default(),
            composer: Editor::default(),
            composer_layout: None,
            composer_scroll: 0.0,
            selecting: false,
            pending: Vec::new(),
            importing: 0,
            preedit: None,
            caret_rect: None,
            notice: None,
            models: Vec::new(),
            model: model.unwrap_or_else(|| DEFAULT_MODEL.to_owned()),
            reasoning,
            projects,
            project,
            menu: None,
            menu_rect: None,
            menu_scroll: 0.0,
            project_button: Rect::default(),
            scroll: 0.0,
            scroll_target: 0.0,
            stick_to_bottom: true,
            selection: None,
            dragging: false,
            sidebar_scroll: 0.0,
            confirm_delete: None,
            copied: None,
            drop_hover: None,
            spotlight: None,
        };
        for summary in sessions {
            let id = chat.next_id();
            chat.conversations.push(Conversation::from_summary(id, summary));
        }
        chat.new_conversation();
        chat
    }

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn current(&mut self) -> &mut Conversation {
        let current = self.current;
        self.conversations
            .iter_mut()
            .find(|c| c.id == current)
            .expect("the current conversation always exists")
    }

    fn find(&mut self, id: u64) -> Option<&mut Conversation> {
        self.conversations.iter_mut().find(|c| c.id == id)
    }

    fn select(&mut self, id: u64) {
        self.current = id;
        self.page = Page::Chat;
        self.menu = None;
        self.selection = None;
        self.scroll = 0.0;
        self.scroll_target = 0.0;
        self.stick_to_bottom = true;
    }

    /// Selects a saved conversation, requesting its messages if needed.
    fn open(&mut self, id: u64, actions: &mut Vec<Action>) {
        self.select(id);
        let conversation = self.current();
        if conversation.load == Load::Summary {
            conversation.load = Load::Loading;
            actions.push(Action::LoadSession { conversation: id, session: conversation.session_id.clone() });
        }
    }

    /// Opens an empty conversation in the current project, reusing an empty one.
    fn new_conversation(&mut self) {
        let project = self.project.clone();
        if let Some(fresh) = self.conversations.iter_mut().find(|c| c.is_fresh()) {
            fresh.project = project;
            let id = fresh.id;
            self.select(id);
            return;
        }
        let id = self.next_id();
        self.conversations.insert(0, Conversation::new(id, project));
        self.select(id);
    }

    /// Switches the active project (or plain chat) and opens a fresh chat in it.
    fn set_project(&mut self, project: Option<String>, actions: &mut Vec<Action>) {
        if let Some(path) = &project {
            // Most recently used first.
            if let Some(i) = self.projects.iter().position(|p| &p.path == path) {
                let mut p = self.projects.remove(i);
                p.last_used = unix_now();
                self.projects.insert(0, p);
            }
        }
        self.project.clone_from(&project);
        self.sidebar_scroll = 0.0;
        actions.push(Action::SetProject(project));
        self.new_conversation();
    }

    /// Adds a project the app just opened (after a folder pick or drop).
    pub fn project_opened(&mut self, project: Project, actions: &mut Vec<Action>) {
        self.projects.retain(|p| p.path != project.path);
        let path = project.path.clone();
        self.projects.insert(0, project);
        self.set_project(Some(path), actions);
    }

    /// Removes a conversation and its saved session.
    fn delete_conversation(&mut self, id: u64, actions: &mut Vec<Action>) {
        let Some(index) = self.conversations.iter().position(|c| c.id == id) else {
            return;
        };
        let conversation = self.conversations.remove(index);
        if let Some(stream) = conversation.stream {
            stream.cancel.store(true, Ordering::Relaxed);
        }
        conversation.tool_cancel.store(true, Ordering::Relaxed);
        actions.push(Action::DeleteSession(conversation.session_id));
        if self.current == id {
            self.new_conversation();
        }
    }

    /// Stores the messages of a session read on a worker thread.
    pub fn session_loaded(&mut self, conversation: u64, result: Result<Session, Error>) {
        let Some(index) = self.conversations.iter().position(|c| c.id == conversation && c.load == Load::Loading) else {
            return;
        };
        let mut messages = match result {
            Ok(session) => session.messages,
            Err(e) => {
                self.conversations[index].load = Load::Failed(format!("This session could not be opened: {e}"));
                return;
            }
        };
        // Tools that were running when the app closed never finished.
        for record in messages.iter_mut().flat_map(|m| m.tool_calls.iter_mut()) {
            if record.status == ToolStatus::Running {
                record.status = ToolStatus::Failed;
                "Interrupted: the app closed while this was running.".clone_into(&mut record.output);
            }
        }
        let first = self.next_id;
        self.next_id += messages.len() as u64;
        let entries = messages.into_iter().zip(first + 1..).map(|(m, id)| Entry::new(id, m)).collect();
        let conversation = &mut self.conversations[index];
        conversation.entries = entries;
        conversation.load = Load::Loaded;
    }

    /// Stores the model list, keeping the selection valid.
    pub fn set_models(&mut self, models: Vec<Model>) {
        if !models.iter().any(|m| m.id == self.model)
            && let Some(fallback) = models.iter().find(|m| m.id == DEFAULT_MODEL).or(models.first())
        {
            self.model.clone_from(&fallback.id);
        }
        self.models = models;
    }

    /// Cancels every running stream and tool (used on sign-out).
    pub fn cancel_all(&mut self) {
        for conversation in &mut self.conversations {
            if let Some(stream) = conversation.stream.take() {
                stream.cancel.store(true, Ordering::Relaxed);
            }
            conversation.tool_cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Usage summed over every conversation with messages.
    fn totals(&self) -> Totals {
        let saved = self.conversations.iter().filter(|c| !c.is_fresh());
        saved.fold(Totals::default(), |t, c| Totals { cost: t.cost + c.cost(), tokens: t.tokens + c.tokens(), sessions: t.sessions + 1 })
    }

    /// Shows a transient message above the composer.
    fn notify(&mut self, message: impl Into<String>) {
        self.notice = Some((message.into(), None));
    }

    /// Starts importing files as attachments (from the picker or a drop).
    pub fn attach(&mut self, paths: Vec<PathBuf>, actions: &mut Vec<Action>) {
        if paths.is_empty() {
            return;
        }
        self.importing += paths.len();
        actions.push(Action::ImportFiles(paths));
    }

    /// Receives imported attachments (or why they failed).
    pub fn attachments_imported(&mut self, results: Vec<Result<Attachment, String>>) {
        self.importing = self.importing.saturating_sub(results.len());
        let mut errors = Vec::new();
        for result in results {
            match result {
                Ok(attachment) => self.pending.push(attachment),
                Err(e) => errors.push(e),
            }
        }
        if !errors.is_empty() {
            self.notify(errors.join("\n"));
        }
    }

    /// A file was dropped on the window: folders open as projects, files attach.
    pub fn dropped(&mut self, path: PathBuf, actions: &mut Vec<Action>) {
        self.drop_hover = None;
        if path.is_dir() {
            actions.push(Action::OpenProject(Some(path)));
        } else {
            self.attach(vec![path], actions);
        }
    }

    /// A file is being dragged over the window (`None` when it left).
    pub fn drag_hover(&mut self, path: Option<&std::path::Path>) {
        self.drop_hover = path.map(std::path::Path::is_dir);
    }

    /// Takes the composer text and starts a reply, if sending is possible.
    fn send(&mut self, actions: &mut Vec<Action>) {
        let has_content = !self.composer.text().trim().is_empty() || !self.pending.is_empty();
        let conversation = self.current();
        // Sending into an unloaded session would save it without its history.
        if conversation.load != Load::Loaded || conversation.busy() || !has_content {
            return;
        }
        if self.importing > 0 {
            self.notify("Wait for the attachments to finish loading.");
            return;
        }
        let images = self.pending.iter().any(Attachment::is_image);
        if images && self.models.iter().find(|m| m.id == self.model).is_some_and(|m| !m.accepts_images()) {
            let name = model_name(&self.models, &self.model).to_owned();
            self.notify(format!("{name} can't read images. Pick a model that can, or remove the image."));
            return;
        }
        let text = self.composer.take().trim().to_owned();
        let attachments = std::mem::take(&mut self.pending);
        let user_id = self.next_id();
        let conversation = self.current();
        // Tool calls still waiting for approval are answered by the new prompt.
        if let Some(calls) = conversation.tool_calls() {
            for record in calls.iter_mut().filter(|r| r.status == ToolStatus::Pending) {
                record.status = ToolStatus::Denied;
                "Skipped: the user sent a new message instead.".clone_into(&mut record.output);
            }
        }
        if conversation.title.is_empty() {
            let first = text.lines().next().unwrap_or_default();
            let fallback = attachments.first().map_or("Attachment", |a| a.name.as_str());
            conversation.title = if first.is_empty() { fallback } else { first }.chars().take(80).collect();
        }
        let mut message = StoredMessage::new(Role::User, text);
        message.attachments = attachments;
        conversation.entries.push(Entry::new(user_id, message));
        conversation.steps = 0;
        let id = conversation.id;

        // Most recently used conversation moves to the top of the sidebar.
        if let Some(index) = self.conversations.iter().position(|c| c.id == id) {
            let moved = self.conversations.remove(index);
            self.conversations.insert(0, moved);
        }
        self.composer_scroll = 0.0;
        self.selection = None;
        self.request_reply(id, actions);
    }

    /// Appends an empty reply to conversation `id` and streams into it.
    fn request_reply(&mut self, id: u64, actions: &mut Vec<Action>) {
        let (reply_id, stream_id) = (self.next_id(), self.next_id());
        let model = self.model.clone();
        let reasoning = self.reasoning.effort();
        let Some(conversation) = self.find(id) else { return };
        let cancel = Arc::new(AtomicBool::new(false));
        conversation.updated = unix_now();
        conversation.tool_cancel = Arc::default();
        let history: Vec<StoredMessage> = conversation.entries.iter().map(|e| e.message.clone()).collect();
        conversation.entries.push(Entry::new(reply_id, StoredMessage::new(Role::Assistant, String::new())));
        conversation.stream = Some(ActiveStream { id: stream_id, cancel: Arc::clone(&cancel), model: model.clone() });
        actions.push(Action::SaveSession(conversation.to_session()));
        let job = SendJob {
            conversation: id,
            stream: stream_id,
            model,
            reasoning,
            instructions: instructions(conversation.project.as_deref()),
            history,
            tools: conversation.project.is_some(),
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
        let Some(conversation) = Self::stream_target(&mut self.conversations, conversation, stream) else {
            return;
        };
        let (Some(stream), Some(entry)) = (&conversation.stream, conversation.entries.last_mut()) else {
            return;
        };
        let message = &mut entry.message;
        match event {
            StreamEvent::Text(delta) => {
                // Models often open with blank lines; don't render them.
                let delta = if message.content.is_empty() { delta.trim_start() } else { &delta };
                message.content.push_str(delta);
            }
            StreamEvent::Reasoning(delta) => message.reasoning.push_str(&delta),
            StreamEvent::Completed(Completion { usage, reasoning, tool_calls }) => {
                if message.reasoning.is_empty() {
                    message.reasoning = reasoning;
                }
                message.cost = self.models.iter().find(|m| m.id == stream.model).map_or(0.0, |m| m.cost(usage));
                message.model = Some(stream.model.clone());
                message.usage = usage;
                message.tool_calls = tool_calls.into_iter().map(|call| ToolRecord { call, status: ToolStatus::Pending, output: String::new() }).collect();
            }
        }
    }

    /// Finishes a stream, queues a save and starts any tool calls. Returns
    /// `true` if the server rejected our token.
    pub fn stream_end(&mut self, conversation: u64, stream: u64, result: Result<bool, Error>, actions: &mut Vec<Action>) -> bool {
        let unauthorized = result.as_ref().is_err_and(Error::is_unauthorized);
        let next = self.next_id();
        let Some(target) = Self::stream_target(&mut self.conversations, conversation, stream) else {
            return false;
        };
        // A stream the user stopped is already detached, so anything short of
        // a completed reply here is a failure worth showing.
        target.stream = None;
        target.updated = unix_now();
        let reply = target.entries.last_mut().filter(|e| e.message.role == Role::Assistant && !e.message.failed);
        let empty = reply.as_ref().is_none_or(|e| e.message.content.is_empty() && e.message.tool_calls.is_empty());
        let error = match result {
            Err(error) => Some(error.to_string()),
            Ok(true) if empty => Some("The model returned an empty response.".to_owned()),
            Ok(true) => None,
            Ok(false) => Some("The connection closed before the reply finished.".to_owned()),
        };
        if let Some(error) = error {
            match reply {
                Some(entry) if entry.message.content.is_empty() && entry.message.tool_calls.is_empty() => {
                    entry.message.content = error;
                    entry.message.failed = true;
                    entry.doc = None;
                }
                _ => {
                    let mut message = StoredMessage::new(Role::Assistant, error);
                    message.failed = true;
                    target.entries.push(Entry::new(next, message));
                }
            }
        }
        actions.push(Action::SaveSession(target.to_session()));
        self.advance(conversation, actions);
        unauthorized
    }

    /// Starts tool calls that may run on their own, and continues the
    /// conversation once every call of the last reply has a result.
    fn advance(&mut self, id: u64, actions: &mut Vec<Action>) {
        let Some(conversation) = self.find(id) else { return };
        let root = conversation.project.clone().map(PathBuf::from);
        let allowed = conversation.allowed.clone();
        let cancel = Arc::clone(&conversation.tool_cancel);
        let Some(calls) = conversation.tool_calls() else { return };
        for record in calls.iter_mut().filter(|r| r.status == ToolStatus::Pending) {
            let Some(root) = &root else {
                record.status = ToolStatus::Failed;
                "Tools are only available in a project.".clone_into(&mut record.output);
                continue;
            };
            if !tools::needs_approval(&record.call.name) || allowed.contains(&record.call.name) {
                record.status = ToolStatus::Running;
                actions.push(Action::RunTool(ToolJob { conversation: id, call: record.call.clone(), root: root.clone(), cancel: Arc::clone(&cancel) }));
            }
        }
        if !calls.iter().all(|r| r.status.is_finished()) {
            return;
        }
        conversation.steps += 1;
        if conversation.steps > MAX_STEPS {
            let next = self.next_id();
            let Some(conversation) = self.find(id) else { return };
            let mut message = StoredMessage::new(Role::Assistant, format!("Stopped after {MAX_STEPS} tool rounds. Send a message to continue."));
            message.failed = true;
            conversation.entries.push(Entry::new(next, message));
            actions.push(Action::SaveSession(conversation.to_session()));
            return;
        }
        self.request_reply(id, actions);
    }

    /// Stores a finished tool call's result and moves the agent on.
    pub fn tool_done(&mut self, conversation: u64, call_id: &str, result: Result<String, String>, actions: &mut Vec<Action>) {
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
        actions.push(Action::SaveSession(target.to_session()));
        self.advance(conversation, actions);
    }

    /// Answers an approval prompt for tool call `index` of entry `entry`.
    fn decide(&mut self, entry: usize, index: usize, decision: Decision, actions: &mut Vec<Action>) {
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
                    actions.push(Action::RunTool(ToolJob {
                        conversation: id,
                        call: record.call.clone(),
                        root,
                        cancel: Arc::clone(&conversation.tool_cancel),
                    }));
                }
            }
        }
        actions.push(Action::SaveSession(self.current().to_session()));
        self.advance(id, actions);
    }

    /// Stops the current conversation's stream and tools, keeping partial output.
    fn stop(&mut self, actions: &mut Vec<Action>) {
        let conversation = self.current();
        let mut stopped = false;
        if let Some(stream) = conversation.stream.take() {
            stream.cancel.store(true, Ordering::Relaxed);
            if conversation
                .entries
                .last()
                .is_some_and(|e| e.message.role == Role::Assistant && e.message.content.is_empty() && e.message.tool_calls.is_empty())
            {
                conversation.entries.pop();
            }
            stopped = true;
        }
        conversation.tool_cancel.store(true, Ordering::Relaxed);
        if let Some(calls) = conversation.tool_calls() {
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

    /// Whether anything is streaming or running (drives redraws for animations).
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.conversations.iter().any(Conversation::busy)
    }

    fn toggle_settings(&mut self) {
        self.menu = None;
        self.page = if self.page == Page::Settings { Page::Chat } else { Page::Settings };
    }

    /// Text of the message selection, if any.
    fn selected_text(&self) -> Option<String> {
        let (a, b) = self.selection?;
        let (from, to) = if a <= b { (a, b) } else { (b, a) };
        if from == to {
            return None;
        }
        let conversation = self.conversations.iter().find(|c| c.id == self.current)?;
        let mut out = String::new();
        for (index, entry) in conversation.entries.iter().enumerate().take(to.0 + 1).skip(from.0) {
            for (doc_id, doc) in [(0u8, entry.reasoning_doc.as_ref().filter(|_| entry.show_reasoning)), (1, entry.doc.as_ref())] {
                let Some(doc) = doc else { continue };
                if (index, doc_id) < (from.0, from.1) || (index, doc_id) > (to.0, to.1) {
                    continue;
                }
                let start = if (index, doc_id) == (from.0, from.1) { (from.2, from.3) } else { (0, 0) };
                let end = if (index, doc_id) == (to.0, to.1) { (to.2, to.3) } else { doc.end() };
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&doc.text_between(start, end));
            }
        }
        (!out.is_empty()).then_some(out)
    }

    /// Text committed by an input method (IME).
    pub fn ime_commit(&mut self, text: &str) {
        self.preedit = None;
        match &mut self.spotlight {
            Some(spotlight) => spotlight.insert(text),
            None if self.page == Page::Chat => self.composer.insert(text),
            None => {}
        }
    }

    /// Text being composed by an input method; empty clears it.
    pub fn ime_preedit(&mut self, text: String, cursor: Option<(usize, usize)>) {
        self.preedit = (!text.is_empty()).then_some((text, cursor));
    }

    /// Where the input method's candidate window should appear.
    #[must_use]
    pub fn ime_area(&self) -> Option<Rect> {
        self.caret_rect
    }

    /// Opens Spotlight.
    fn open_spotlight(&mut self) {
        self.menu = None;
        self.spotlight = Some(Spotlight::default());
    }

    /// Content-search results for Spotlight.
    pub fn search_results(&mut self, generation: u64, hits: Vec<serechat::SearchHit>) {
        if let Some(spotlight) = &mut self.spotlight {
            spotlight.set_hits(generation, hits);
        }
    }

    /// Applies what the user picked in Spotlight.
    fn apply_pick(&mut self, pick: Pick, actions: &mut Vec<Action>) {
        self.spotlight = None;
        match pick {
            Pick::NewChat => self.new_conversation(),
            Pick::Settings => {
                self.page = Page::Settings;
            }
            Pick::Theme(scheme) => actions.push(Action::SetTheme(scheme)),
            Pick::OpenFolder => actions.push(Action::OpenProject(None)),
            Pick::Attach => actions.push(Action::PickFiles),
            Pick::Project(path) => self.set_project(path, actions),
            Pick::Session(session) => {
                let Some((id, project)) = self.conversations.iter().find(|c| c.session_id == session).map(|c| (c.id, c.project.clone())) else {
                    return;
                };
                if project != self.project {
                    self.set_project(project, actions);
                }
                self.open(id, actions);
            }
            Pick::Model(model) => {
                self.model.clone_from(&model);
                actions.push(Action::SelectModel(model));
            }
        }
    }

    /// Keyboard input.
    pub fn key(&mut self, event: &KeyEvent, mods: ModifiersState, cb: &mut Option<Clipboard>, actions: &mut Vec<Action>) {
        let primary = if cfg!(target_os = "macos") { mods.super_key() } else { mods.control_key() };
        if let Some(spotlight) = &mut self.spotlight {
            match spotlight.key(event, mods, cb) {
                Outcome::Pick(pick) => self.apply_pick(pick, actions),
                Outcome::Close => self.spotlight = None,
                Outcome::Stay => {}
            }
            return;
        }
        let is = |c: &str, ch: &str| c.eq_ignore_ascii_case(ch);
        match &event.logical_key {
            Key::Named(NamedKey::Escape) if self.menu.is_some() => self.menu = None,
            Key::Named(NamedKey::Escape) if self.page == Page::Settings => self.page = Page::Chat,
            Key::Character(c) if primary && is(c, "k") => self.open_spotlight(),
            Key::Character(c) if primary && is(c, ",") => self.toggle_settings(),
            Key::Character(c) if primary && is(c, "n") => self.new_conversation(),
            Key::Character(c) if primary && is(c, "o") => actions.push(Action::OpenProject(None)),
            _ if self.page == Page::Settings => {}
            // Copy a message selection; otherwise the composer handles it.
            Key::Character(c) if primary && is(c, "c") && self.composer.selection().is_empty() && self.selection.is_some() => {
                if let Some(text) = self.selected_text() {
                    copy(cb, &text);
                }
            }
            Key::Named(NamedKey::Enter) if mods.shift_key() => self.composer.insert("\n"),
            Key::Named(NamedKey::Enter) => self.send(actions),
            Key::Named(NamedKey::Escape) if self.selection.is_some() => self.selection = None,
            Key::Named(NamedKey::Escape) => self.stop(actions),
            Key::Named(key @ (NamedKey::ArrowUp | NamedKey::ArrowDown)) => {
                if let Some((layout, _)) = &self.composer_layout {
                    let (x, y) = layout.caret(self.composer.cursor());
                    let line = layout.line_height();
                    let target = if *key == NamedKey::ArrowUp { y - line * 0.5 } else { y + line * 1.5 };
                    let byte = if target < 0.0 {
                        0
                    } else if target > layout.height() {
                        self.composer.text().len()
                    } else {
                        layout.hit(x, target)
                    };
                    self.composer.set_cursor(byte, mods.shift_key());
                }
            }
            _ => {
                if edit_key(&mut self.composer, event, mods, cb) {
                    self.selection = None;
                }
            }
        }
    }

    /// Draws the screen.
    pub fn draw(&mut self, p: &mut Painter, ui: &mut Ui, view: Rect, scheme: Scheme, actions: &mut Vec<Action>) {
        let sidebar = Rect::new(0.0, 0.0, theme::SIDEBAR_WIDTH, view.h);
        let main = Rect::new(sidebar.w, 0.0, view.w - sidebar.w, view.h);

        // Overlays are drawn last, on top. Spotlight blocks everything
        // beneath it; an open menu blocks its own area (rect from last frame).
        let modal = self.spotlight.is_some();
        ui.blocker = if modal { Some(view) } else if self.menu.is_some() { self.menu_rect } else { None };
        self.draw_sidebar(p, ui, sidebar, actions);
        let toolbar = if self.page == Page::Settings {
            let totals = self.totals();
            self.settings.draw(p, ui, main, scheme, totals, actions);
            [Rect::default(); 2]
        } else {
            self.draw_header(p, main);
            let (composer_top, toolbar) = self.draw_composer(p, ui, main, actions);
            let messages = Rect::new(main.x, theme::HEADER_HEIGHT, main.w, composer_top - theme::HEADER_HEIGHT - 12.0);
            self.draw_messages(p, ui, messages, actions);
            toolbar
        };
        if !modal {
            ui.blocker = None;
        }
        self.draw_open_menu(p, ui, toolbar, actions);
        self.draw_drop_overlay(p, view);
        if let Some(spotlight) = &mut self.spotlight {
            ui.blocker = None;
            let context = crate::spotlight::Context {
                sessions: self
                    .conversations
                    .iter()
                    .filter(|c| !c.is_fresh())
                    .map(|c| crate::spotlight::SessionRef {
                        id: &c.session_id,
                        title: &c.title,
                        project: c.project.as_deref(),
                        updated: c.updated,
                    })
                    .collect(),
                projects: &self.projects,
                models: &self.models,
                model: &self.model,
                scheme,
            };
            match spotlight.draw(p, ui, view, &context, actions) {
                Outcome::Pick(pick) => self.apply_pick(pick, actions),
                Outcome::Close => self.spotlight = None,
                Outcome::Stay => {}
            }
        }
    }

    /// Title bar of the chat area: session title, project folder and cost.
    fn draw_header(&mut self, p: &mut Painter, main: Rect) {
        let t = p.theme;
        let bar = Rect::new(main.x, 0.0, main.w, theme::HEADER_HEIGHT);
        p.rect(Rect::new(bar.x, bar.bottom() - 1.0, bar.w, 1.0), t.border, 0.0);
        let conversation = self.current();
        let (tokens, cost) = (conversation.tokens(), conversation.cost());
        let title = if conversation.title.is_empty() { "New chat".to_owned() } else { conversation.title.clone() };
        let project = conversation.project.clone();

        let mut right = bar.right() - 16.0;
        if tokens > 0 {
            let spent = p.layout(&format!("{} tokens  ·  {}", group_digits(tokens), format_cost(cost)), theme::SMALL, None);
            right -= spent.width();
            p.text(&spent, right, bar.y + (bar.h - spent.height()) * 0.5, t.text_faint);
            right -= 20.0;
        }
        if let Some(path) = project {
            // The working directory the agent's tools operate in.
            let mut dir = p.layout(&display_path(&path), crate::text::Style::mono(12.0), None);
            dir.truncate(p.fonts, (bar.w * 0.4).max(80.0));
            let chip = Rect::new(right - dir.width() - 30.0, bar.y + 10.0, dir.width() + 30.0, bar.h - 20.0);
            p.bordered(chip, t.surface, theme::RADIUS_SM, 1.0, t.border);
            crate::ui::folder_icon(p, chip.x + 8.0, chip.y + (chip.h - 10.0) * 0.5, t.text_faint);
            p.text(&dir, chip.x + 24.0, chip.y + (chip.h - dir.height()) * 0.5, t.text_muted);
            right = chip.x - 12.0;
        }
        let mut layout = p.layout(&title, theme::LABEL, None);
        layout.truncate(p.fonts, (right - bar.x - 16.0).max(40.0));
        p.text(&layout, bar.x + 16.0, bar.y + (bar.h - layout.height()) * 0.5, t.text);
    }

    /// Hint shown while files are dragged over the window.
    fn draw_drop_overlay(&self, p: &mut Painter, view: Rect) {
        let Some(folder) = self.drop_hover else { return };
        let t = p.theme;
        p.rect(view, crate::paint::fade(t.bg, 0.85), 0.0);
        let area = Rect::new(view.x + 24.0, view.y + 24.0, view.w - 48.0, view.h - 48.0);
        p.bordered(area, [0.0; 4], theme::RADIUS * 2.0, 2.0, t.accent);
        let text = if folder { "Drop to open this folder as a project" } else { "Drop to attach" };
        p.label_centered(text, crate::text::Style::semibold(18.0), area, t.text);
    }

    /// Column holding messages and the composer inside `main`.
    fn column(main: Rect) -> (f32, f32) {
        let width = theme::COLUMN_WIDTH.min(main.w - 64.0);
        (main.x + ((main.w - width) * 0.5).round(), width)
    }
}

/// An answer to a tool approval prompt.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Decision {
    Allow,
    Always,
    Deny,
}

/// Display name of a model id.
fn model_name<'a>(models: &'a [Model], id: &'a str) -> &'a str {
    models.iter().find(|m| m.id == id && !m.name.is_empty()).map_or(id, |m| m.name.as_str())
}

/// A model's prices, e.g. `$2 / $10`; empty when the server sent none.
fn price(model: &Model) -> String {
    if model.input_cost_per_million > 0.0 || model.output_cost_per_million > 0.0 {
        format!("${} / ${}", model.input_cost_per_million, model.output_cost_per_million)
    } else {
        String::new()
    }
}

/// `path` with the home directory shortened to `~`.
#[must_use]
pub fn display_path(path: &str) -> String {
    match std::env::home_dir().and_then(|home| std::path::Path::new(path).strip_prefix(home).ok().map(std::path::Path::to_path_buf)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => path.to_owned(),
    }
}

/// Caption for a finished reply, e.g. `Claude Sonnet 5.5 · 1,204 tokens · $0.0031`.
fn usage_caption(model: &str, usage: Usage, cost: f64) -> String {
    let tokens = usage.input_tokens + usage.output_tokens;
    match (tokens, cost > 0.0) {
        (0, _) => model.to_owned(),
        (_, true) => format!("{model}  ·  {} tokens  ·  {}", group_digits(tokens), format_cost(cost)),
        (_, false) => format!("{model}  ·  {} tokens", group_digits(tokens)),
    }
}

/// Compact age of a timestamp: `now`, `5m`, `3h`, `2d`, `6w`, `1y`.
fn ago(now: u64, then: u64) -> String {
    let secs = now.saturating_sub(then);
    match secs {
        0..60 => "now".to_owned(),
        60..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h", secs / 3_600),
        86_400..604_800 => format!("{}d", secs / 86_400),
        604_800..31_536_000 => format!("{}w", secs / 604_800),
        _ => format!("{}y", secs / 31_536_000),
    }
}

/// `1234567` -> `1,234,567`.
pub fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A USD amount with enough precision to be meaningful for tiny requests.
pub fn format_cost(usd: f64) -> String {
    if usd <= 0.0 {
        "$0.00".to_owned()
    } else if usd < 0.0001 {
        "<$0.0001".to_owned()
    } else if usd < 0.01 {
        format!("${usd:.4}")
    } else {
        format!("${usd:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captions() {
        assert_eq!(group_digits(7), "7");
        assert_eq!(group_digits(1_234), "1,234");
        assert_eq!(group_digits(1_234_567), "1,234,567");
        assert_eq!(format_cost(0.0), "$0.00");
        assert_eq!(format_cost(0.000_01), "<$0.0001");
        assert_eq!(format_cost(0.003_14), "$0.0031");
        assert_eq!(format_cost(1.5), "$1.50");
        let usage = Usage { input_tokens: 1_000, output_tokens: 204 };
        assert_eq!(usage_caption("M", usage, 0.0031), "M  ·  1,204 tokens  ·  $0.0031");
        assert_eq!(usage_caption("M", usage, 0.0), "M  ·  1,204 tokens");
        assert_eq!(usage_caption("M", Usage::default(), 1.0), "M");
    }

    #[test]
    fn ages() {
        assert_eq!(ago(100, 100), "now");
        assert_eq!(ago(100, 200), "now", "clock skew must not underflow");
        assert_eq!(ago(3_600 + 59, 0), "1h");
        assert_eq!(ago(86_400 * 3, 0), "3d");
        assert_eq!(ago(604_800 * 5, 0), "5w");
        assert_eq!(ago(31_536_000 * 2, 0), "2y");
    }

    #[test]
    fn reasoning_keys() {
        for r in Reasoning::ALL {
            assert_eq!(Reasoning::from_key(Some(r.key())), r);
        }
        assert_eq!(Reasoning::Auto.effort(), None);
        assert_eq!(Reasoning::Off.effort(), Some("none"));
        assert_eq!(Reasoning::from_key(Some("bogus")), Reasoning::Auto);
    }

    fn reply_with(calls: &[(&str, &str)]) -> StoredMessage {
        let mut reply = StoredMessage::new(Role::Assistant, "Let me look.".into());
        reply.tool_calls = calls
            .iter()
            .map(|(id, name)| ToolRecord { call: ToolCall { call_id: (*id).into(), name: (*name).into(), arguments: "{}".into() }, status: ToolStatus::Pending, output: String::new() })
            .collect();
        reply
    }

    #[test]
    fn sessions_round_trip_through_the_screen() {
        let mut reply = StoredMessage::new(Role::Assistant, "hello".into());
        reply.cost = 0.5;
        reply.usage = Usage { input_tokens: 3, output_tokens: 4 };
        let session = Session {
            id: "abc".into(),
            title: "Hi".into(),
            created: 1,
            updated: 2,
            project: None,
            messages: vec![StoredMessage::new(Role::User, "hi".into()), reply],
        };
        let mut chat = Chat::new(None, Reasoning::Auto, vec![session.summary()], Vec::new(), None);
        // The saved session plus a fresh, unsaved conversation.
        assert_eq!(chat.conversations.len(), 2);
        // Totals come from the index without loading any messages.
        let totals = chat.totals();
        assert_eq!((totals.sessions, totals.tokens), (1, 7));

        let id = chat.conversations.iter().find(|c| c.session_id == "abc").map(|c| c.id).unwrap();
        let mut actions = Vec::new();
        chat.open(id, &mut actions);
        assert!(matches!(&actions[..], [Action::LoadSession { session, .. }] if session == "abc"));

        // Sending must wait for the history, or saving would drop it.
        chat.composer.insert("more");
        let mut actions = Vec::new();
        chat.send(&mut actions);
        assert!(actions.is_empty());

        chat.session_loaded(id, Ok(session.clone()));
        assert_eq!(chat.current().to_session(), session);
        chat.send(&mut actions);
        assert!(matches!(&actions[..], [Action::SaveSession(saved), Action::Send(job)] if saved.messages.len() == 3 && !job.tools));
    }

    #[test]
    fn agent_loop_runs_reads_asks_for_writes_and_continues() {
        let dir = std::env::temp_dir();
        let project = dir.to_string_lossy().into_owned();
        let mut chat = Chat::new(None, Reasoning::Auto, Vec::new(), Vec::new(), Some(project));
        chat.composer.insert("fix it");
        let mut actions = Vec::new();
        chat.send(&mut actions);
        let Some(Action::Send(job)) = actions.pop() else { panic!("no send") };
        assert!(job.tools && job.instructions.contains("project folder"));

        // The reply asks to read one file and write another.
        let calls = reply_with(&[("r", "read_file"), ("w", "write_file")]).tool_calls;
        let completion = Completion { tool_calls: calls.into_iter().map(|r| r.call).collect(), ..Completion::default() };
        chat.stream_event(job.conversation, job.stream, StreamEvent::Completed(completion));
        let mut actions = Vec::new();
        chat.stream_end(job.conversation, job.stream, Ok(true), &mut actions);
        let runs: Vec<&str> = actions.iter().filter_map(|a| if let Action::RunTool(t) = a { Some(t.call.name.as_str()) } else { None }).collect();
        assert_eq!(runs, ["read_file"], "reads run at once, writes wait");

        let mut actions = Vec::new();
        chat.tool_done(job.conversation, "r", Ok("contents".into()), &mut actions);
        assert!(!actions.iter().any(|a| matches!(a, Action::Send(_))), "still waiting for approval");

        let entry = chat.current().entries.len() - 1;
        let mut actions = Vec::new();
        chat.decide(entry, 1, Decision::Deny, &mut actions);
        let Some(Action::Send(next)) = actions.pop() else { panic!("the agent should continue") };
        let items = input_items(&next.history).unwrap();
        // The prompt, then (call + output) for both calls; the reply had no text.
        assert_eq!(items.len(), 5);
        assert!(matches!(&items[2], InputItem::ToolOutput { call_id, output } if call_id == "r" && output == "contents"));
        assert!(matches!(&items[4], InputItem::ToolOutput { call_id, output } if call_id == "w" && output.contains("declined")));
    }

    #[test]
    fn stopping_answers_every_open_call() {
        let project = std::env::temp_dir().to_string_lossy().into_owned();
        let mut chat = Chat::new(None, Reasoning::Auto, Vec::new(), Vec::new(), Some(project));
        chat.composer.insert("go");
        let mut actions = Vec::new();
        chat.send(&mut actions);
        let Some(Action::Send(job)) = actions.pop() else { panic!() };
        let completion = Completion { tool_calls: reply_with(&[("a", "run_command")]).tool_calls.into_iter().map(|r| r.call).collect(), ..Completion::default() };
        chat.stream_event(job.conversation, job.stream, StreamEvent::Completed(completion));
        chat.stream_end(job.conversation, job.stream, Ok(true), &mut Vec::new());
        chat.stop(&mut Vec::new());
        let calls = chat.current().tool_calls().cloned().unwrap();
        assert!(calls.iter().all(|r| r.status.is_finished()));
        assert!(!chat.is_busy());
    }
}
