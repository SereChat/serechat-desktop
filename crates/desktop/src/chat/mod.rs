//! Main screen: sidebar of saved sessions, messages, composer, menus,
//! Spotlight and the settings page.
//!
//! The screen never touches the disk or network itself: it emits [`Action`]s
//! (send, save, run a tool, import files, …) that the app carries out on
//! worker threads, and receives their results through the `*_done` /
//! `*_loaded` methods. The agent loop lives in `agent.rs`.

mod agent;
mod composer;
mod find;
mod menu;
mod messages;
mod sidebar;
mod skills;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use arboard::Clipboard;
use serechat::{
    Attachment, Error, MediaJob, MediaKind, MediaModel, MediaTicket, Model, Project, Role, SPARK_USD, Session, SessionSummary, StoredMessage,
    ToolRecord, ToolStatus, Usage, new_session_id, unix_now,
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
use crate::tools;

pub use agent::{SendJob, ToolJob, input_items};
use agent::{ActiveStream, Retry};
use composer::Command;
use messages::DiffView;

/// Model used until the user picks one.
pub const DEFAULT_MODEL: &str = "claude-sonnet-5.5";
/// Generation models used until the user picks one (or the list says
/// otherwise), by [`MediaKind::index`].
const DEFAULT_MEDIA: [&str; 3] = ["gpt-image-2.5-flare", "veo-3.1-fast", "elevenlabs-v4-turbo"];
/// Name of the platform's primary shortcut modifier.
const PRIMARY_KEY: &str = if cfg!(target_os = "macos") { "Cmd" } else { "Ctrl" };

/// How much the model should think before answering. Models accept
/// different efforts (see [`Model::reasoning_levels`]); one the selected
/// model does not accept falls back to [`Reasoning::Auto`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reasoning {
    /// Leave it to the model.
    #[default]
    Auto,
    /// No reasoning.
    Off,
    /// The least reasoning.
    Minimal,
    /// Brief reasoning.
    Low,
    /// Balanced reasoning.
    Medium,
    /// Thorough reasoning.
    High,
    /// More than high.
    ExtraHigh,
    /// As much as the model can.
    Max,
}

impl Reasoning {
    const ALL: [Self; 8] = [Self::Auto, Self::Off, Self::Minimal, Self::Low, Self::Medium, Self::High, Self::ExtraHigh, Self::Max];

    /// Value stored in the config file; also the API's effort name.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Off => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::ExtraHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Parses a config value; unknown values mean [`Reasoning::Auto`].
    #[must_use]
    pub fn from_key(key: Option<&str>) -> Self {
        Self::ALL.into_iter().find(|r| Some(r.key()) == key).unwrap_or_default()
    }

    /// The choices for `model`: Auto plus the efforts it accepts, or every
    /// effort while the model list is unknown.
    fn choices(model: Option<&Model>) -> Vec<Self> {
        Self::ALL.into_iter().filter(|r| *r == Self::Auto || model.is_none_or(|m| m.reasoning_levels.iter().any(|l| l == r.key()))).collect()
    }

    fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Off => "Off",
            Self::Minimal => "Minimal",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::ExtraHigh => "Extra high",
            Self::Max => "Max",
        }
    }

    fn detail(self) -> &'static str {
        match self {
            Self::Auto => "Model default",
            Self::Off => "Answer right away",
            Self::Minimal => "Barely think",
            Self::Low => "Think briefly",
            Self::Medium => "Balanced",
            Self::High => "Think it through",
            Self::ExtraHigh => "Think longer",
            Self::Max => "Think as long as needed",
        }
    }
}

/// How replies show the model's reasoning.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReasoningView {
    /// Never shown.
    Hidden,
    /// A "Thought for …" row that expands on click.
    #[default]
    Collapsed,
    /// Shown in full, streaming live when the server sends it.
    Expanded,
}

impl ReasoningView {
    /// Every choice, in settings order.
    pub const ALL: [Self; 3] = [Self::Hidden, Self::Collapsed, Self::Expanded];

    /// Value stored in the config file.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Hidden => "hidden",
            Self::Collapsed => "collapsed",
            Self::Expanded => "expanded",
        }
    }

    /// Parses a config value; unknown values mean [`ReasoningView::Collapsed`].
    #[must_use]
    pub fn from_key(key: Option<&str>) -> Self {
        Self::ALL.into_iter().find(|v| Some(v.key()) == key).unwrap_or_default()
    }

    /// Name shown in settings.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Hidden => "Hidden",
            Self::Collapsed => "Collapsed",
            Self::Expanded => "Expanded",
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
    /// Slash commands completing the composer text; open while any match.
    Commands,
    /// Generation models of one kind, while the composer holds its command.
    Media(MediaKind),
}

/// A generation for a worker thread to run: submit it (or pick up a job
/// already submitted), wait for it, and download the file.
pub struct MediaRequest {
    /// Conversation it belongs to.
    pub conversation: u64,
    /// Entry the file goes into.
    pub entry: u64,
    /// What it makes.
    pub kind: MediaKind,
    /// Model id.
    pub model: String,
    /// Mode that takes a prompt alone, or `None` for the model's default.
    pub mode: Option<String>,
    /// The prompt.
    pub prompt: String,
    /// A job already submitted (after a restart): only wait for it.
    pub job: Option<String>,
    /// Raised when nobody waits for the result any more.
    pub cancel: Arc<AtomicBool>,
}

/// One message in a conversation, with its cached layouts.
struct Entry {
    id: u64,
    message: StoredMessage,
    /// The user opened (`true`) or closed the reasoning block; `None`
    /// follows the [`ReasoningView`] setting.
    reasoning_open: Option<bool>,
    /// Tool cards whose output is expanded.
    open_tools: HashSet<usize>,
    doc: Option<Doc>,
    /// Content length and boxed-ness the doc was built for.
    doc_key: (usize, bool),
    reasoning_doc: Option<Doc>,
    reasoning_len: usize,
    /// Per tool card: body layout and what it was laid out for.
    tool_bodies: Vec<ToolBody>,
    /// Tool calls the model is writing right now (never saved).
    streaming_calls: Vec<StreamingCall>,
    /// Diffs of the file changes among the tool calls, by call id.
    diffs: HashMap<String, DiffView>,
    /// Since when this app has been waiting for the entry's generation.
    media_since: Option<Instant>,
}

/// A tool call still being written, shown as it streams in.
struct StreamingCall {
    /// Its position in the response, as the stream identifies it.
    index: u64,
    name: String,
    /// The JSON arguments so far.
    arguments: String,
    /// How the card reads, laid out for this many bytes of arguments.
    shown: Option<(usize, tools::CallView, Option<TextLayout>)>,
}

impl Entry {
    fn new(id: u64, message: StoredMessage) -> Self {
        Self {
            id,
            message,
            reasoning_open: None,
            open_tools: HashSet::new(),
            doc: None,
            doc_key: (0, false),
            reasoning_doc: None,
            reasoning_len: 0,
            tool_bodies: Vec::new(),
            streaming_calls: Vec::new(),
            diffs: HashMap::new(),
            media_since: None,
        }
    }

    /// Drawn inside a bordered box (prompts and errors) rather than bare.
    fn boxed(&self) -> bool {
        self.message.role == Role::User || self.message.failed
    }

    /// Whether the reasoning text is shown under `view`.
    fn reasoning_shown(&self, view: ReasoningView) -> bool {
        view != ReasoningView::Hidden && !self.message.reasoning.is_empty() && self.reasoning_open.unwrap_or(view == ReasoningView::Expanded)
    }
}

/// A tool card body layout with the (text length, status, width) it shows,
/// and how far it is scrolled when taller than the card.
type ToolBody = Option<((usize, ToolStatus, u32), TextLayout, f32)>;

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
    /// A failed request waiting to be sent again.
    retry: Option<Retry>,
    /// USD billed for failed requests, not yet added to a message.
    carried_cost: f64,
    /// Tools the user allowed to run without asking, for this session.
    allowed: HashSet<String>,
    /// Tool rounds since the run started or was continued.
    steps: u32,
    /// Raised to stop running tools.
    tool_cancel: Arc<AtomicBool>,
    /// Raised to stop waiting for generations (the chat was deleted).
    media_cancel: Arc<AtomicBool>,
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
            retry: None,
            carried_cost: 0.0,
            allowed: HashSet::new(),
            steps: 0,
            tool_cancel: Arc::default(),
            media_cancel: Arc::default(),
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
        let mut allowed_tools: Vec<String> = self.allowed.iter().cloned().collect();
        allowed_tools.sort();
        Session {
            allowed_tools,
            id: self.session_id.clone(),
            title: self.title.clone(),
            created: self.created,
            updated: self.updated,
            project: self.project.clone(),
            messages: self
                .entries
                .iter()
                .filter(|e| {
                    let m = &e.message;
                    !m.content.is_empty() || !m.tool_calls.is_empty() || !m.attachments.is_empty() || m.media.is_some()
                })
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

    /// A tool of the last reply is running.
    fn running(&self) -> bool {
        self.entries.last().is_some_and(|e| e.message.tool_calls.iter().any(|t| t.status == ToolStatus::Running))
    }

    /// Streaming, waiting to retry, or a tool is running.
    fn busy(&self) -> bool {
        self.stream.is_some() || self.retry.is_some() || self.running()
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
    /// Generation models that work from a prompt, by [`MediaKind::index`].
    media_models: [Vec<MediaModel>; 3],
    /// The chosen generation model of each kind.
    media_model: [String; 3],
    reasoning: Reasoning,
    reasoning_view: ReasoningView,
    projects: Vec<Project>,
    /// The folder picked last, which the app reopens in.
    project: Option<String>,
    menu: Option<Menu>,
    /// Where the open menu was drawn last frame; blocks hover beneath it.
    menu_rect: Option<Rect>,
    menu_scroll: f32,
    /// The slash command Enter runs, among those matching.
    command_pick: usize,
    /// Composer text the command menu was closed for; it stays closed
    /// until the text changes.
    command_dismissed: Option<String>,
    /// The header's folder chip, anchoring the project menu.
    project_button: Rect,
    scroll: f32,
    scroll_target: f32,
    stick_to_bottom: bool,
    /// Scroll offset when the scrollbar was grabbed, while it is dragged.
    bar_drag: Option<f32>,
    /// Message selection: anchor and focus.
    selection: Option<(SelPos, SelPos)>,
    /// Dragging a message selection.
    dragging: bool,
    sidebar_scroll: f32,
    /// A working conversation the user asked to delete, awaiting confirmation.
    confirm_delete: Option<u64>,
    /// What was just copied (entry, code block or whole message) and when.
    copied: Option<(u64, Option<usize>, f32)>,
    /// A file is dragged over the window; `true` if it is a folder.
    drop_hover: Option<bool>,
    spotlight: Option<Spotlight>,
    /// Skill catalogs, by location.
    skills: skills::SkillCache,
    /// The find bar, while open.
    find: Option<find::Find>,
    /// A version installed by the updater, waiting for a restart.
    update_ready: Option<String>,
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
            media_models: Default::default(),
            media_model: DEFAULT_MEDIA.map(str::to_owned),
            reasoning,
            reasoning_view: ReasoningView::default(),
            projects,
            project: project.clone(),
            menu: None,
            menu_rect: None,
            menu_scroll: 0.0,
            command_pick: 0,
            command_dismissed: None,
            project_button: Rect::default(),
            scroll: 0.0,
            scroll_target: 0.0,
            stick_to_bottom: true,
            bar_drag: None,
            selection: None,
            dragging: false,
            sidebar_scroll: 0.0,
            confirm_delete: None,
            copied: None,
            drop_hover: None,
            spotlight: None,
            skills: skills::SkillCache::default(),
            find: None,
            update_ready: None,
        };
        for summary in sessions {
            let id = chat.next_id();
            chat.conversations.push(Conversation::from_summary(id, summary));
        }
        chat.new_conversation();
        // Only the chat opened at startup resumes the folder picked last.
        chat.current().project = project;
        chat
    }

    /// The selected model, once the model list has loaded.
    fn selected_model(&self) -> Option<&Model> {
        self.models.iter().find(|m| m.id == self.model)
    }

    /// The effort requests use: the chosen one if the model accepts it.
    fn reasoning_in_use(&self) -> Reasoning {
        if Reasoning::choices(self.selected_model()).contains(&self.reasoning) { self.reasoning } else { Reasoning::Auto }
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
        let project = conversation.project.clone();
        self.ensure_skills(project.as_deref(), actions);
    }

    /// Opens an empty conversation with no project, reusing an empty one.
    fn new_conversation(&mut self) {
        if let Some(fresh) = self.conversations.iter_mut().find(|c| c.is_fresh()) {
            fresh.project = None;
            let id = fresh.id;
            self.select(id);
            return;
        }
        let id = self.next_id();
        self.conversations.insert(0, Conversation::new(id, None));
        self.select(id);
    }

    /// Changes the open conversation's working folder (`None`: plain chat,
    /// no tools). The app reopens in the folder picked last.
    fn set_project(&mut self, project: Option<String>, actions: &mut Vec<Action>) {
        let conversation = self.current();
        if conversation.load != Load::Loaded {
            return;
        }
        if conversation.busy() {
            self.notify("Stop the reply before changing the folder.");
            return;
        }
        if conversation.project != project {
            conversation.project.clone_from(&project);
            // Approvals were given for the old folder.
            conversation.allowed.clear();
            if let Some(calls) = conversation.tool_calls() {
                for record in calls.iter_mut().filter(|r| r.status == ToolStatus::Pending) {
                    record.status = ToolStatus::Denied;
                    "Skipped: the user changed the working folder.".clone_into(&mut record.output);
                }
            }
            if !conversation.is_fresh() {
                actions.push(Action::SaveSession(conversation.to_session()));
            }
        }
        if let Some(path) = &project {
            // Most recently used first.
            if let Some(i) = self.projects.iter().position(|p| &p.path == path) {
                let mut p = self.projects.remove(i);
                p.last_used = unix_now();
                self.projects.insert(0, p);
            }
        }
        self.ensure_skills(project.as_deref(), actions);
        self.project.clone_from(&project);
        actions.push(Action::SetProject(project));
    }

    /// Adds a project the app just opened (after a folder pick or drop) and
    /// makes it the open conversation's folder.
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
        conversation.media_cancel.store(true, Ordering::Relaxed);
        actions.push(Action::DeleteSession(conversation.session_id));
        actions.push(Action::StopProcesses(conversation.id));
        if self.current == id {
            self.new_conversation();
        }
    }

    /// Slash commands the composer text completes to, unless the user
    /// closed their menu for this text.
    fn commands(&self) -> Vec<Command> {
        let text = self.composer.text();
        if self.page != Page::Chat || self.command_dismissed.as_deref() == Some(text) {
            return Vec::new();
        }
        Command::matching(text)
    }

    /// Opens the command menu while commands match and closes it otherwise.
    fn sync_command_menu(&mut self) {
        let open = !self.commands().is_empty();
        match self.menu {
            None if open => {
                self.menu = Some(Menu::Commands);
                self.command_pick = 0;
                self.menu_scroll = 0.0;
            }
            Some(Menu::Commands) if !open => self.menu = None,
            _ => {}
        }
    }

    /// Closes the open menu; the command menu stays closed for this text.
    fn close_menu(&mut self) {
        if self.menu == Some(Menu::Commands) {
            self.command_dismissed = Some(self.composer.text().to_owned());
        }
        self.menu = None;
    }

    /// Runs a slash command, taking it out of the composer. Commands that
    /// need a prompt are completed instead, ready for it.
    fn run_command(&mut self, command: Command, actions: &mut Vec<Action>) {
        self.composer.take();
        self.command_dismissed = None;
        self.menu = None;
        match command {
            Command::Media(kind) => self.composer.insert(&format!("/{} ", kind.noun())),
            Command::New => {
                let project = self.current().project.clone();
                self.new_conversation();
                self.current().project.clone_from(&project);
                self.ensure_skills(project.as_deref(), actions);
            }
            Command::Clear => self.clear(actions),
            Command::Compact => self.compact(actions),
            Command::Model => {
                self.menu = Some(Menu::Model);
                self.menu_scroll = 0.0;
            }
            // Sent as a message; the model gets its prompt (see `input_items`).
            Command::Init => {
                self.composer.insert("/init");
                self.send(actions);
            }
        }
    }

    /// Deletes the open chat (stopping anything it runs) and opens an empty
    /// one in the same folder.
    fn clear(&mut self, actions: &mut Vec<Action>) {
        let conversation = self.current();
        if conversation.is_fresh() {
            return;
        }
        let (id, project) = (conversation.id, conversation.project.clone());
        self.delete_conversation(id, actions);
        self.current().project = project;
    }

    /// Stores the messages of a session read on a worker thread, and picks up
    /// the generations it was still waiting for.
    pub fn session_loaded(&mut self, conversation: u64, result: Result<Session, Error>, actions: &mut Vec<Action>) {
        let Some(index) = self.conversations.iter().position(|c| c.id == conversation && c.load == Load::Loading) else {
            return;
        };
        let (mut messages, allowed) = match result {
            Ok(session) => (session.messages, session.allowed_tools),
            Err(e) => {
                self.conversations[index].load = Load::Failed(format!("This session could not be opened: {e}"));
                return;
            }
        };
        // Tools that were running, or about to run, when the app closed never
        // finished. Calls waiting for approval still can be approved.
        for record in messages.iter_mut().flat_map(|m| m.tool_calls.iter_mut()) {
            let unstarted = record.status == ToolStatus::Pending && !tools::needs_approval(&record.call.name);
            if record.status == ToolStatus::Running || unstarted {
                record.status = ToolStatus::Failed;
                "Interrupted: the app closed while this was running.".clone_into(&mut record.output);
            }
        }
        // A generation the server never accepted is lost; one it did is
        // still running there, or finished, and is picked up again.
        for message in messages.iter_mut().filter(|m| m.media_pending() && m.media.as_ref().is_some_and(|j| j.id.is_empty())) {
            message.failed = true;
            "Interrupted: the app closed before the generation started.".clone_into(&mut message.content);
        }
        let first = self.next_id;
        self.next_id += messages.len() as u64;
        let entries: Vec<Entry> = messages.into_iter().zip(first + 1..).map(|(m, id)| Entry::new(id, m)).collect();
        let target = &mut self.conversations[index];
        target.entries = entries;
        target.allowed = allowed.into_iter().collect();
        target.load = Load::Loaded;
        for entry in &mut target.entries {
            if let (true, Some(job)) = (entry.message.media_pending(), &entry.message.media) {
                entry.media_since = Some(Instant::now());
                actions.push(Action::Generate(MediaRequest {
                    conversation,
                    entry: entry.id,
                    kind: job.kind,
                    model: entry.message.model.clone().unwrap_or_default(),
                    mode: None,
                    prompt: String::new(),
                    job: Some(job.id.clone()),
                    cancel: Arc::clone(&target.media_cancel),
                }));
            }
        }
        // Changes still waiting for approval show their diff against the file.
        if let Some(root) = target.project.clone().map(PathBuf::from) {
            let calls = target.entries.iter().flat_map(|e| &e.message.tool_calls).filter(|r| r.status == ToolStatus::Pending);
            actions.extend(calls.filter_map(|r| preview(&root, &r.call, conversation)));
        }
    }

    /// The diff of a file change arrived from a worker.
    pub fn diff_ready(&mut self, conversation: u64, call_id: &str, diff: Arc<crate::diff::Diff>) {
        let Some(target) = self.find(conversation) else { return };
        if let Some(entry) = target.entries.iter_mut().rev().find(|e| e.message.tool_calls.iter().any(|r| r.call.call_id == call_id)) {
            let open = entry.diffs.get(call_id).is_some_and(DiffView::full);
            entry.diffs.insert(call_id.to_owned(), DiffView::new(diff, true, open));
        }
    }

    /// Sets how replies show reasoning. Blocks the user opened or closed
    /// by hand start following the setting again.
    pub fn set_reasoning_view(&mut self, view: ReasoningView) {
        self.reasoning_view = view;
        for entry in self.conversations.iter_mut().flat_map(|c| c.entries.iter_mut()) {
            entry.reasoning_open = None;
        }
        self.selection = None;
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
            conversation.retry = None;
            conversation.tool_cancel.store(true, Ordering::Relaxed);
            conversation.media_cancel.store(true, Ordering::Relaxed);
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

    /// Takes the composer text and starts a reply (or runs its slash
    /// command, or starts its generation), if sending is possible.
    fn send(&mut self, actions: &mut Vec<Action>) {
        let typed = self.composer.text().trim().to_owned();
        let command = Command::parse(&typed);
        let mut media = None;
        match command {
            // `/init` is sent as a message; other bare commands just run.
            Some((Command::Init, "")) if self.current().project.is_none() => {
                self.notify(format!("Open a project folder first ({PRIMARY_KEY}+O), then run /init in it."));
                return;
            }
            Some((command, "")) if command != Command::Init => {
                self.run_command(command, actions);
                return;
            }
            Some((Command::Media(kind), prompt)) => media = Some((kind, prompt.to_owned())),
            _ => {}
        }
        let has_content = !typed.is_empty() || !self.pending.is_empty();
        let conversation = self.current();
        // Sending into an unloaded session would save it without its history.
        if conversation.load != Load::Loaded || conversation.busy() || !has_content {
            return;
        }
        if self.importing > 0 {
            self.notify("Wait for the attachments to finish loading.");
            return;
        }
        if let (Some((kind, _)), false) = (&media, self.pending.is_empty()) {
            self.notify(format!("/{} works from the text alone. Remove the attachments to generate.", kind.noun()));
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
        match media {
            Some((kind, prompt)) => self.generate(id, kind, prompt, actions),
            None => self.request_reply(id, actions),
        }
    }

    /// The chosen generation model of `kind`, and the mode to ask for.
    fn media_choice(&self, kind: MediaKind) -> (String, Option<String>) {
        let id = &self.media_model[kind.index()];
        let mode = self.media_models[kind.index()].iter().find(|m| &m.id == id).and_then(MediaModel::prompt_mode);
        (id.clone(), mode.map(str::to_owned))
    }

    /// Display name of a text or generation model.
    fn model_label<'a>(&'a self, id: &'a str) -> &'a str {
        label_of(&self.models, &self.media_models, id)
    }

    /// Adds the reply that waits for a generation of `kind` from `prompt` to
    /// conversation `id`, and starts it.
    fn generate(&mut self, id: u64, kind: MediaKind, prompt: String, actions: &mut Vec<Action>) {
        let (model, mode) = self.media_choice(kind);
        let entry_id = self.next_id();
        let Some(conversation) = self.find(id) else { return };
        let mut message = StoredMessage::new(Role::Assistant, String::new());
        message.model = Some(model.clone());
        message.media = Some(MediaJob { kind, id: String::new() });
        let mut entry = Entry::new(entry_id, message);
        entry.media_since = Some(Instant::now());
        conversation.entries.push(entry);
        conversation.updated = unix_now();
        actions.push(Action::SaveSession(conversation.to_session()));
        let cancel = Arc::clone(&conversation.media_cancel);
        actions.push(Action::Generate(MediaRequest { conversation: id, entry: entry_id, kind, model, mode, prompt, job: None, cancel }));
        if id == self.current {
            self.stick_to_bottom = true;
        }
    }

    /// The server accepted a generation: remember its job, so it survives a
    /// restart, and what it costs.
    pub fn media_started(&mut self, conversation: u64, entry: u64, ticket: MediaTicket, actions: &mut Vec<Action>) {
        let Some(target) = self.find(conversation) else { return };
        let Some(message) = target.entries.iter_mut().find(|e| e.id == entry).map(|e| &mut e.message) else { return };
        if let Some(job) = &mut message.media {
            job.id = ticket.id;
        }
        message.cost = ticket.sparks * SPARK_USD;
        actions.push(Action::SaveSession(target.to_session()));
    }

    /// A generation finished: its file, or why there is none. `Ok(None)`
    /// means nobody waits for it any more.
    pub fn media_done(&mut self, conversation: u64, entry: u64, result: Result<Option<Attachment>, String>, actions: &mut Vec<Action>) {
        let Some(target) = self.find(conversation) else { return };
        let Some(found) = target.entries.iter_mut().find(|e| e.id == entry) else { return };
        let message = &mut found.message;
        match result {
            Ok(None) => return,
            Ok(Some(file)) => message.attachments = vec![file],
            Err(error) => {
                let noun = message.media.as_ref().map_or("file", |job| job.kind.noun());
                message.content = format!("The {noun} could not be generated: {error}");
                message.failed = true;
                // The server refunds generations that fail.
                message.cost = 0.0;
                found.doc = None;
            }
        }
        found.media_since = None;
        actions.push(Action::SaveSession(target.to_session()));
        actions.push(Action::Attention);
    }

    /// Stores the generation models of `kind` that work from a prompt,
    /// keeping the choice valid.
    pub fn set_media_models(&mut self, kind: MediaKind, mut models: Vec<MediaModel>) {
        models.retain(|m| m.prompt_mode().is_some());
        let chosen = &mut self.media_model[kind.index()];
        if !models.iter().any(|m| &m.id == chosen)
            && let Some(fallback) = models.iter().find(|m| m.id == DEFAULT_MEDIA[kind.index()]).or(models.first())
        {
            chosen.clone_from(&fallback.id);
        }
        self.media_models[kind.index()] = models;
    }

    /// Restores the generation model of `kind` picked in an earlier run.
    pub fn set_media_choice(&mut self, kind: MediaKind, model: Option<String>) {
        if let Some(model) = model {
            self.media_model[kind.index()] = model;
        }
    }

    fn toggle_settings(&mut self, actions: &mut Vec<Action>) {
        if self.page == Page::Settings {
            self.menu = None;
            self.page = Page::Chat;
        } else {
            self.open_settings(actions);
        }
    }

    /// Shows the settings page with the open chat's skills as known, and
    /// rescans them in case the files changed.
    fn open_settings(&mut self, actions: &mut Vec<Action>) {
        self.menu = None;
        self.page = Page::Settings;
        self.show_skills();
        self.refresh_skills(true, actions);
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
            let reasoning = entry.reasoning_doc.as_ref().filter(|_| entry.reasoning_shown(self.reasoning_view));
            for (doc_id, doc) in [(0u8, reasoning), (1, entry.doc.as_ref())] {
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

    /// Shows the MCP servers' state, and a message about them, in settings.
    pub fn set_mcp(&mut self, servers: Vec<crate::mcp::ServerView>, note: Option<(String, bool)>) {
        self.settings.set_mcp(servers);
        self.settings.set_mcp_note(note);
    }

    /// Shows the browsers found for settings, and the one picked (`None`: Auto).
    pub fn set_browsers(&mut self, found: Vec<crate::browser::Installed>, choice: Option<String>) {
        self.settings.set_browsers(found, choice);
    }

    /// Shows the updater's status in settings, and a restart button in the
    /// sidebar once an update is installed.
    pub fn set_update(&mut self, status: crate::update::Status, auto: bool) {
        self.update_ready = match &status {
            crate::update::Status::Ready(version) => Some(version.clone()),
            _ => None,
        };
        self.settings.set_update(status, auto);
    }

    /// Opens the find bar, or selects its text if it is open.
    fn open_find(&mut self) {
        let find = self.find.get_or_insert_with(find::Find::default);
        find.focused = true;
        find.field.editor.select_all();
        find.reveal = true;
    }

    /// Text committed by an input method (IME).
    pub fn ime_commit(&mut self, text: &str) {
        self.preedit = None;
        match &mut self.spotlight {
            Some(spotlight) => spotlight.insert(text),
            None if self.page == Page::Settings => self.settings.insert(text),
            None => match &mut self.find {
                Some(find) if find.focused => find.field.editor.insert(text),
                _ if self.page == Page::Chat => self.composer.insert(text),
                _ => {}
            },
        }
    }

    /// Text being composed by an input method; empty clears it.
    pub fn ime_preedit(&mut self, text: String, cursor: Option<(usize, usize)>) {
        self.preedit = (!text.is_empty()).then_some((text, cursor));
    }

    /// Where the input method's candidate window should appear.
    #[must_use]
    pub fn ime_area(&self) -> Option<Rect> {
        if self.page == Page::Settings {
            return self.settings.caret();
        }
        match &self.find {
            Some(find) if find.focused => find.caret,
            _ => self.caret_rect,
        }
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
            Pick::Settings => self.open_settings(actions),
            Pick::Theme(scheme) => actions.push(Action::SetTheme(scheme)),
            Pick::OpenFolder => actions.push(Action::OpenProject(None)),
            Pick::Attach => actions.push(Action::PickFiles),
            Pick::Project(path) => self.set_project(path, actions),
            Pick::Session(session) => {
                if let Some(id) = self.conversations.iter().find(|c| c.session_id == session).map(|c| c.id) {
                    self.open(id, actions);
                }
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
        // The delete dialog takes every key: Enter deletes, Escape cancels.
        if let Some(id) = self.confirm_delete {
            match &event.logical_key {
                Key::Named(NamedKey::Enter) => {
                    self.confirm_delete = None;
                    self.delete_conversation(id, actions);
                }
                Key::Named(NamedKey::Escape) => self.confirm_delete = None,
                _ => {}
            }
            return;
        }
        let is = |c: &str, ch: &str| c.eq_ignore_ascii_case(ch);
        // A focused settings field takes typing first.
        if self.page == Page::Settings && self.menu.is_none() && self.settings.key(event, mods, cb, actions) {
            return;
        }
        if self.page == Page::Chat && self.menu.is_none() && self.find_key(event, mods, cb) {
            return;
        }
        let commands = self.commands();
        match &event.logical_key {
            Key::Named(NamedKey::Escape) if self.menu.is_some() => self.close_menu(),
            Key::Named(NamedKey::Escape) if self.page == Page::Settings => self.page = Page::Chat,
            Key::Character(c) if primary && is(c, "k") => self.open_spotlight(),
            Key::Character(c) if primary && is(c, ",") => self.toggle_settings(actions),
            Key::Character(c) if primary && is(c, "n") => self.new_conversation(),
            Key::Character(c) if primary && is(c, "o") => actions.push(Action::OpenProject(None)),
            Key::Character(c) if primary && is(c, "f") && self.page == Page::Chat => self.open_find(),
            _ if self.page == Page::Settings => {}
            // Copy a message selection; otherwise the composer handles it.
            Key::Character(c) if primary && is(c, "c") && self.composer.selection().is_empty() && self.selection.is_some() => {
                if let Some(text) = self.selected_text() {
                    copy(cb, &text);
                }
            }
            // Enter runs the picked slash command, Tab completes it.
            Key::Named(key @ (NamedKey::Enter | NamedKey::Tab)) if !commands.is_empty() && !mods.shift_key() => {
                let command = commands[self.command_pick.min(commands.len() - 1)];
                // Commands that take a prompt complete either way.
                if *key == NamedKey::Enter || matches!(command, Command::Media(_)) {
                    self.run_command(command, actions);
                } else {
                    self.composer.take();
                    self.composer.insert(&format!("/{}", command.name()));
                }
            }
            Key::Named(key @ (NamedKey::ArrowUp | NamedKey::ArrowDown)) if !commands.is_empty() => {
                let step = if *key == NamedKey::ArrowUp { commands.len() - 1 } else { 1 };
                self.command_pick = (self.command_pick.min(commands.len() - 1) + step) % commands.len();
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

    /// Keys for the find bar: stepping through matches, closing it, and
    /// typing while it has focus. Returns whether the key was used.
    fn find_key(&mut self, event: &KeyEvent, mods: ModifiersState, cb: &mut Option<Clipboard>) -> bool {
        let Some(find) = &mut self.find else { return false };
        match &event.logical_key {
            Key::Named(NamedKey::F3) => find.step(mods.shift_key()),
            Key::Named(NamedKey::Escape) => self.find = None,
            Key::Named(NamedKey::Enter) if find.focused => find.step(mods.shift_key()),
            // Tab hands typing back to the composer.
            Key::Named(NamedKey::Tab) if find.focused => find.focused = false,
            _ if find.focused => {
                let primary = if cfg!(target_os = "macos") { mods.super_key() } else { mods.control_key() };
                let key = |k: &str| matches!(&event.logical_key, Key::Character(c) if c.eq_ignore_ascii_case(k));
                // Global shortcuts (Ctrl+K, Ctrl+N, …) still reach the screen,
                // and so does copying a message selection.
                let shortcut = primary && !["a", "c", "x", "v"].iter().any(|k| key(k));
                let copy_messages = primary && key("c") && find.field.editor.selection().is_empty();
                return !shortcut && !copy_messages && edit_key(&mut find.field.editor, event, mods, cb);
            }
            _ => return false,
        }
        true
    }

    /// Draws the screen.
    pub fn draw(&mut self, p: &mut Painter, ui: &mut Ui, view: Rect, scheme: Scheme, actions: &mut Vec<Action>) {
        let sidebar = Rect::new(0.0, 0.0, theme::SIDEBAR_WIDTH, view.h);
        let main = Rect::new(sidebar.w, 0.0, view.w - sidebar.w, view.h);

        // Overlays are drawn last, on top. Spotlight blocks everything
        // beneath it; an open menu blocks its own area (rect from last frame).
        self.sync_command_menu();
        // A dialog opened this frame shows from the next, so the click that
        // opened it doesn't also count as a click outside it.
        let confirming = self.confirm_delete.is_some();
        let modal = self.spotlight.is_some() || confirming;
        let find_bar = self.find.as_ref().filter(|_| self.page == Page::Chat).map(|f| f.rect);
        ui.blocker = if modal { Some(view) } else if self.menu.is_some() { self.menu_rect } else { find_bar };
        self.draw_sidebar(p, ui, sidebar, actions);
        let toolbar = if self.page == Page::Settings {
            let totals = self.totals();
            self.settings.draw(p, ui, main, scheme, self.reasoning_view, totals, actions);
            [Rect::default(); 3]
        } else {
            self.draw_header(p, ui, main);
            let (composer_top, toolbar) = self.draw_composer(p, ui, main, actions);
            let messages = Rect::new(main.x, theme::HEADER_HEIGHT, main.w, composer_top - theme::HEADER_HEIGHT - 12.0);
            self.draw_messages(p, ui, messages, actions);
            // The find bar floats over the messages' top right.
            if !modal && self.menu.is_none() {
                ui.blocker = None;
            }
            if let Some(find) = &mut self.find {
                let (event, step) = find.draw(p, ui, messages, self.current);
                if let Some(back) = step {
                    find.step(back);
                }
                if event == find::BarEvent::Close {
                    self.find = None;
                }
            }
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
        if confirming {
            ui.blocker = None;
            self.draw_confirm_delete(p, ui, view, actions);
        } else if self.confirm_delete.is_some() {
            ui.animating = true;
        }
    }

    /// Title bar of the chat area: session title, project folder and cost.
    fn draw_header(&mut self, p: &mut Painter, ui: &mut Ui, main: Rect) {
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
        // The working directory the agent's tools operate in; click to change it.
        let (label, style) = match &project {
            Some(path) => (display_path(path), crate::text::Style::mono(12.0)),
            None => ("No project".to_owned(), theme::SMALL),
        };
        let mut dir = p.layout(&label, style, None);
        dir.truncate(p.fonts, (bar.w * 0.4).max(80.0));
        let chip = Rect::new(right - dir.width() - 46.0, bar.y + 10.0, dir.width() + 46.0, bar.h - 20.0);
        self.project_button = chip;
        let open = self.menu == Some(Menu::Project);
        let hovered = ui.hovered(chip);
        let hover = ui.anim(crate::ui::id("folder-chip"), f32::from(u8::from(hovered || open)));
        p.bordered(chip, crate::paint::mix(t.surface, t.hover, hover), theme::RADIUS_SM, 1.0, t.border);
        crate::ui::folder_icon(p, chip.x + 8.0, chip.y + (chip.h - 10.0) * 0.5, t.text_faint);
        p.text(&dir, chip.x + 24.0, chip.y + (chip.h - dir.height()) * 0.5, t.text_muted);
        crate::ui::chevron(p, chip.right() - 16.0, chip.y + chip.h * 0.5 - 2.0, true, t.text_faint);
        if hovered {
            ui.cursor = winit::window::CursorIcon::Pointer;
            if ui.clicked(chip) {
                self.menu = if open { None } else { Some(Menu::Project) };
                self.menu_scroll = 0.0;
            }
        }
        right = chip.x - 12.0;
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

/// The action computing the diff of `call` against its file in `root`, if
/// it is a file change waiting for approval.
fn preview(root: &std::path::Path, call: &serechat::ToolCall, conversation: u64) -> Option<Action> {
    (tools::changes_file(&call.name) && tools::needs_approval(&call.name))
        .then(|| Action::PreviewChange { conversation, root: root.to_path_buf(), call: call.clone() })
}

/// Display name of a text or generation model id.
fn label_of<'a>(models: &'a [Model], media: &'a [Vec<MediaModel>; 3], id: &'a str) -> &'a str {
    media.iter().flatten().find(|m| m.id == id).map_or_else(|| model_name(models, id), MediaModel::label)
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
        (0, false) => model.to_owned(),
        // Generations are billed per file, not per token.
        (0, true) => format!("{model}  ·  {}", format_cost(cost)),
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
    use serechat::{Completion, InputItem, Part, StreamEvent, ToolCall};

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
        let usage = Usage::new(1_000, 204);
        assert_eq!(usage_caption("M", usage, 0.0031), "M  ·  1,204 tokens  ·  $0.0031");
        assert_eq!(usage_caption("M", usage, 0.0), "M  ·  1,204 tokens");
        assert_eq!(usage_caption("M", Usage::default(), 0.0), "M");
        assert_eq!(usage_caption("M", Usage::default(), 0.04), "M  ·  $0.04");
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
        assert_eq!(Reasoning::from_key(Some("bogus")), Reasoning::Auto);
    }

    #[test]
    fn reasoning_follows_the_model() {
        let model = |levels: &[&str]| Model {
            id: "m".into(),
            name: String::new(),
            input_cost_per_million: 0.0,
            output_cost_per_million: 0.0,
            cache_read_cost_per_million: None,
            cache_write_cost_per_million: None,
            input_types: Vec::new(),
            context_window: 0,
            reasoning_levels: levels.iter().map(|l| (*l).to_owned()).collect(),
        };
        let claude = model(&["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(Reasoning::choices(Some(&claude))[..2], [Reasoning::Auto, Reasoning::Low]);
        assert_eq!(Reasoning::choices(Some(&model(&[]))), [Reasoning::Auto], "a model that cannot reason");
        assert_eq!(Reasoning::choices(None).len(), Reasoning::ALL.len());

        let mut chat = Chat::new(Some("m".into()), Reasoning::Off, Vec::new(), Vec::new(), None);
        assert_eq!(chat.reasoning_in_use(), Reasoning::Off, "unknown models keep the choice");
        chat.set_models(vec![claude]);
        assert_eq!(chat.reasoning_in_use(), Reasoning::Auto, "Off is not a level this model accepts");
        chat.reasoning = Reasoning::Max;
        assert_eq!(chat.reasoning_in_use(), Reasoning::Max);
    }

    #[test]
    fn reasoning_visibility_follows_the_setting() {
        for v in ReasoningView::ALL {
            assert_eq!(ReasoningView::from_key(Some(v.key())), v);
        }
        assert_eq!(ReasoningView::from_key(None), ReasoningView::Collapsed);

        let mut message = StoredMessage::new(Role::Assistant, "42".into());
        message.reasoning = "6 × 7".into();
        let mut entry = Entry::new(1, message);
        assert!(!entry.reasoning_shown(ReasoningView::Collapsed));
        assert!(entry.reasoning_shown(ReasoningView::Expanded));
        entry.reasoning_open = Some(true);
        assert!(entry.reasoning_shown(ReasoningView::Collapsed), "a click overrides the setting");
        assert!(!entry.reasoning_shown(ReasoningView::Hidden), "hidden always wins");
    }

    #[test]
    fn only_reasoning_replies_record_thinking_time() {
        let mut chat = Chat::new(None, Reasoning::Auto, Vec::new(), Vec::new(), None);
        for reasoning in ["", "thought"] {
            chat.composer.insert("hi");
            let mut actions = Vec::new();
            chat.send(&mut actions);
            let Some(Action::Send(job)) = actions.pop() else { panic!("no send") };
            if let Some(stream) = &mut chat.current().stream {
                stream.started -= std::time::Duration::from_secs(3);
            }
            chat.stream_event(job.conversation, job.stream, StreamEvent::Text("answer".into()));
            let completion = Completion { reasoning: reasoning.into(), ..Completion::default() };
            chat.stream_event(job.conversation, job.stream, StreamEvent::Completed(completion));
            chat.stream_end(job.conversation, job.stream, Ok(true), &mut Vec::new());
            let ms = chat.current().entries.last().map(|e| e.message.reasoning_ms);
            assert_eq!(ms.is_some_and(|ms| ms >= 3000), !reasoning.is_empty(), "reasoning {reasoning:?}");
        }
    }

    fn reply_with(calls: &[(&str, &str)]) -> StoredMessage {
        let mut reply = StoredMessage::new(Role::Assistant, "Let me look.".into());
        reply.tool_calls = calls
            .iter()
            .map(|(id, name)| ToolRecord { call: ToolCall { call_id: (*id).into(), name: (*name).into(), arguments: "{}".into() }, status: ToolStatus::Pending, output: String::new(), image: None })
            .collect();
        reply
    }

    #[test]
    fn sessions_round_trip_through_the_screen() {
        let mut reply = StoredMessage::new(Role::Assistant, "hello".into());
        reply.cost = 0.5;
        reply.usage = Usage::new(3, 4);
        let session = Session {
            id: "abc".into(),
            title: "Hi".into(),
            created: 1,
            updated: 2,
            project: None,
            messages: vec![StoredMessage::new(Role::User, "hi".into()), reply],
            allowed_tools: Vec::new(),
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
        // Opening also looks for the skills it can use.
        actions.retain(|a| !matches!(a, Action::ScanSkills { .. }));
        assert!(matches!(&actions[..], [Action::LoadSession { session, .. }] if session == "abc"));

        // Sending must wait for the history, or saving would drop it.
        chat.composer.insert("more");
        let mut actions = Vec::new();
        chat.send(&mut actions);
        assert!(actions.is_empty());

        chat.session_loaded(id, Ok(session.clone()), &mut Vec::new());
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
        chat.tool_done(job.conversation, "r", Ok("contents".into()), None, &mut actions);
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
    fn the_folder_belongs_to_the_session() {
        let project = std::env::temp_dir().to_string_lossy().into_owned();
        let mut chat = Chat::new(None, Reasoning::Auto, Vec::new(), Vec::new(), Some(project.clone()));
        chat.composer.insert("go");
        let mut actions = Vec::new();
        chat.send(&mut actions);
        let Some(Action::Send(job)) = actions.pop() else { panic!() };
        let completion = Completion { tool_calls: reply_with(&[("a", "write_file")]).tool_calls.into_iter().map(|r| r.call).collect(), ..Completion::default() };
        chat.stream_event(job.conversation, job.stream, StreamEvent::Completed(completion));
        chat.stream_end(job.conversation, job.stream, Ok(true), &mut Vec::new());
        chat.current().allowed.insert("run_command".into());

        let mut actions = Vec::new();
        chat.set_project(None, &mut actions);
        let session = chat.current();
        assert_eq!(session.project, None);
        assert!(session.allowed.is_empty(), "approvals don't carry over to another folder");
        assert!(session.tool_calls().unwrap().iter().all(|r| r.status == ToolStatus::Denied));
        actions.retain(|a| !matches!(a, Action::ScanSkills { .. }));
        assert!(matches!(&actions[..], [Action::SaveSession(s), Action::SetProject(None)] if s.project.is_none()));

        // A new chat opens without a folder.
        chat.set_project(Some(project), &mut Vec::new());
        chat.new_conversation();
        assert_eq!(chat.current().project, None);
        assert_eq!(chat.conversations.iter().filter(|c| c.project.is_some()).count(), 1);
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
        assert!(chat.current().resumable(), "a stopped run can be continued");
    }

    fn project_chat() -> Chat {
        Chat::new(None, Reasoning::Auto, Vec::new(), Vec::new(), Some(std::env::temp_dir().to_string_lossy().into_owned()))
    }

    /// Sends `prompt` and returns the request.
    fn start(chat: &mut Chat, prompt: &str) -> SendJob {
        chat.composer.insert(prompt);
        let mut actions = Vec::new();
        chat.send(&mut actions);
        next_send(actions).expect("a request")
    }

    /// Completes `job` with `completion` and returns what the chat asked for.
    fn finish(chat: &mut Chat, job: &SendJob, completion: Completion) -> Vec<Action> {
        chat.stream_event(job.conversation, job.stream, StreamEvent::Completed(completion));
        let mut actions = Vec::new();
        chat.stream_end(job.conversation, job.stream, Ok(true), &mut actions);
        actions
    }

    fn next_send(actions: Vec<Action>) -> Option<SendJob> {
        actions.into_iter().find_map(|a| if let Action::Send(job) = a { Some(job) } else { None })
    }

    fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
        ToolCall { call_id: id.into(), name: name.into(), arguments: arguments.into() }
    }

    #[test]
    fn failed_requests_retry_then_give_up() {
        use std::time::Instant;
        let mut chat = project_chat();
        let mut job = start(&mut chat, "hi");
        for attempt in 1..=agent::MAX_RETRIES {
            chat.stream_event(job.conversation, job.stream, StreamEvent::Text("partial".into()));
            let error = Error::Response { code: Some("server_error".into()), message: "boom".into() };
            chat.stream_end(job.conversation, job.stream, Err(error), &mut Vec::new());
            let c = chat.current();
            assert_eq!(c.retry.as_ref().map(|r| r.attempt), Some(attempt));
            assert_eq!(c.entries.len(), 1, "the partial reply is dropped");
            assert!(c.busy() && !c.resumable());
            let mut actions = Vec::new();
            chat.tick(Instant::now(), &mut actions);
            assert!(actions.is_empty(), "not due yet");
            chat.tick(chat.next_deadline().unwrap(), &mut actions);
            job = next_send(actions).expect("retried");
        }
        let error = Error::Response { code: Some("server_error".into()), message: "boom".into() };
        chat.stream_end(job.conversation, job.stream, Err(error), &mut Vec::new());
        let c = chat.current();
        assert!(c.retry.is_none() && c.entries.last().is_some_and(|e| e.message.failed && e.message.content == "boom"));
        assert!(c.resumable(), "Continue tries again by hand");
        let id = c.id;
        let mut actions = Vec::new();
        chat.resume(id, &mut actions);
        let job = next_send(actions).expect("continued");

        // Final errors are shown at once.
        let error = Error::Response { code: Some("invalid_request_error".into()), message: "bad".into() };
        chat.stream_end(job.conversation, job.stream, Err(error), &mut Vec::new());
        assert!(chat.current().retry.is_none() && !chat.current().busy());
    }

    #[test]
    fn silent_streams_are_retried() {
        let mut chat = project_chat();
        let job = start(&mut chat, "hi");
        let mut actions = Vec::new();
        chat.tick(chat.next_deadline().unwrap() + std::time::Duration::from_secs(1), &mut actions);
        assert!(job.cancel.load(Ordering::Relaxed), "the dead stream is abandoned");
        assert_eq!(chat.current().retry.as_ref().map(|r| r.attempt), Some(1));
        // Its late end is ignored.
        chat.stream_end(job.conversation, job.stream, Ok(true), &mut Vec::new());
        assert!(chat.current().retry.is_some());
    }

    #[test]
    fn replies_cut_off_never_run_their_calls() {
        let mut chat = project_chat();
        let job = start(&mut chat, "write it");
        let cut = Some("max_output_tokens".to_owned());
        let completion = Completion { tool_calls: vec![call("w", "write_file", r#"{"path":"a","con"#)], incomplete: cut.clone(), ..Completion::default() };
        let actions = finish(&mut chat, &job, completion);
        assert!(!actions.iter().any(|a| matches!(a, Action::RunTool(_))));
        let record = chat.current().entries.iter().flat_map(|e| &e.message.tool_calls).next().cloned().unwrap();
        assert!(record.status == ToolStatus::Failed && record.output.contains("output limit"));
        let next = next_send(actions).expect("the model is told and tries again");

        // A text reply that was cut off just says so.
        chat.stream_event(next.conversation, next.stream, StreamEvent::Text("Long".into()));
        let actions = finish(&mut chat, &next, Completion { incomplete: cut, ..Completion::default() });
        assert!(next_send(actions).is_none());
        assert!(chat.current().entries.last().is_some_and(|e| e.message.failed && e.message.content.contains("cut off")));
    }

    #[test]
    fn long_conversations_are_summarised_with_their_plan() {
        let mut chat = project_chat();
        let job = start(&mut chat, "refactor");
        let plan = call("p", "update_plan", r#"{"steps":[{"step":"Read","status":"done"},{"step":"Change","status":"in_progress"}]}"#);
        let skill = call("s", "use_skill", r#"{"name":"review"}"#);
        let skill_file = call("f", "use_skill", r#"{"name":"review","file":"references/GUIDE.md"}"#);
        // The reply used most of the (default) window.
        let completion = Completion { usage: Usage::new(120_000, 1_000), tool_calls: vec![plan, skill, skill_file], ..Completion::default() };
        let actions = finish(&mut chat, &job, completion);
        assert_eq!(actions.iter().filter(|a| matches!(a, Action::RunTool(_))).count(), 3, "none of them needs approval");
        chat.tool_done(job.conversation, "p", Ok("Plan updated: 2 steps.".into()), None, &mut Vec::new());
        chat.tool_done(job.conversation, "f", Ok("The guide.".into()), None, &mut Vec::new());
        let mut actions = Vec::new();
        chat.tool_done(job.conversation, "s", Ok("<skill_content name=\"review\">Check everything.</skill_content>".into()), None, &mut actions);

        let summary = next_send(actions).expect("a summary request");
        assert_eq!(summary.tool_choice, Some("none"));
        assert!(summary.tools, "tools stay declared for the tool history");
        assert!(summary.history.last().is_some_and(|m| m.role == Role::User && m.content.contains("summary")));
        chat.stream_event(summary.conversation, summary.stream, StreamEvent::Text("Read the code.".into()));
        let actions = finish(&mut chat, &summary, Completion { usage: Usage::new(121_000, 50), ..Completion::default() });

        let reply = next_send(actions).expect("the reply it was made for");
        assert_eq!(reply.tool_choice, None);
        let items = input_items(&reply.history).unwrap();
        assert_eq!(items.len(), 1, "only the summary reaches the model");
        let InputItem::Message { parts, .. } = &items[0] else { panic!("a message") };
        assert!(matches!(&parts[0], Part::Text(t) if t.contains("Read the code.") && t.contains("→ Change")));
        // The skills in use keep their instructions; files they read don't.
        assert!(matches!(&parts[0], Part::Text(t) if t.contains("Check everything.") && !t.contains("The guide.")));
        // The history itself is kept for the user.
        assert!(chat.current().entries.len() >= 3);
    }

    #[test]
    fn skills_work_without_a_project() {
        let mut chat = Chat::new(None, Reasoning::Auto, Vec::new(), Vec::new(), None);
        let skill = crate::skills::Skill { name: "notes".into(), description: "d".into(), path: "x".into(), scope: crate::skills::Scope::User };
        let mine = Arc::new(crate::skills::Catalog { skills: vec![skill], ..crate::skills::Catalog::default() });
        chat.skills_scanned(None, None, Arc::clone(&mine));

        let job = start(&mut chat, "take notes");
        assert!(!job.tools && job.project.is_none());
        assert_eq!(job.user_skills.as_deref(), Some(&*mine), "the request uses the cached catalog");
        let calls = vec![
            call("s", "use_skill", r#"{"name":"notes"}"#),
            call("r", "read_file", r#"{"path":"a"}"#),
            call("f", "fetch_url", r#"{"url":"https://example.com"}"#),
        ];
        let actions = finish(&mut chat, &job, Completion { tool_calls: calls, ..Completion::default() });
        let runs: Vec<&ToolJob> = actions.iter().filter_map(|a| if let Action::RunTool(t) = a { Some(t) } else { None }).collect();
        assert_eq!(runs.len(), 1);
        assert!(runs[0].call.name == "use_skill" && runs[0].root.is_none() && runs[0].skills.len() == 1);
        let calls = chat.current().tool_calls().cloned().unwrap();
        assert!(calls[1].status == ToolStatus::Failed && calls[1].output.contains("only available in a project"));
        assert_eq!(calls[2].status, ToolStatus::Pending, "fetching works without a project, after approval");
    }

    #[test]
    fn tool_calls_show_while_written_and_runs_add_up() {
        let mut chat = project_chat();
        let priced = Model {
            id: DEFAULT_MODEL.into(),
            name: String::new(),
            input_cost_per_million: 1_000_000.0,
            output_cost_per_million: 0.0,
            cache_read_cost_per_million: None,
            cache_write_cost_per_million: None,
            input_types: Vec::new(),
            context_window: 0,
            reasoning_levels: Vec::new(),
        };
        chat.set_models(vec![priced]);
        let job = start(&mut chat, "write it");
        chat.stream_event(job.conversation, job.stream, StreamEvent::ToolCallStarted { index: 1, name: "write_file".into() });
        chat.stream_event(job.conversation, job.stream, StreamEvent::ToolCallDelta { index: 1, delta: r#"{"path":"a.txt","content":"hi"#.into() });
        let live = &chat.current().entries.last().unwrap().streaming_calls;
        assert_eq!((live.len(), live[0].arguments.as_str()), (1, r#"{"path":"a.txt","content":"hi"#));

        // The attempt fails after being billed $2; the retry succeeds for $1.
        chat.stream_event(job.conversation, job.stream, StreamEvent::Charged(Usage::new(2, 0)));
        let error = Error::Response { code: Some("server_error".into()), message: "boom".into() };
        chat.stream_end(job.conversation, job.stream, Err(error), &mut Vec::new());
        let mut actions = Vec::new();
        chat.tick(chat.next_deadline().unwrap(), &mut actions);
        let retry = next_send(actions).unwrap();
        let completion = Completion { usage: Usage::new(1, 0), tool_calls: vec![call("w", "write_file", "{}")], ..Completion::default() };
        finish(&mut chat, &retry, completion);
        let reply = chat.current().entries.iter().find(|e| !e.message.tool_calls.is_empty()).unwrap();
        assert!(reply.streaming_calls.is_empty(), "the finished call replaces its preview");
        assert!((reply.message.cost - 3.0).abs() < 1e-9, "failed attempts are billed with the reply");
        let (steps, cost) = chat.current().run_stats();
        assert_eq!(steps, 1);
        assert!((cost - 3.0).abs() < 1e-9);
    }

    #[test]
    fn overflowing_requests_compact_and_keep_the_new_prompt() {
        let mut chat = project_chat();
        let job = start(&mut chat, "first");
        chat.stream_event(job.conversation, job.stream, StreamEvent::Text("ok".into()));
        finish(&mut chat, &job, Completion { usage: Usage::new(1_000, 10), ..Completion::default() });

        let job = start(&mut chat, "second");
        let mut actions = Vec::new();
        let overflow = Error::Response { code: Some("context_length_exceeded".into()), message: "too long".into() };
        chat.stream_end(job.conversation, job.stream, Err(overflow), &mut actions);
        let summary = next_send(actions).expect("a summary request");
        assert!(summary.history.iter().all(|m| m.content != "second"), "the new prompt is not summarised");
        chat.stream_event(summary.conversation, summary.stream, StreamEvent::Text("They said first.".into()));
        let reply = next_send(finish(&mut chat, &summary, Completion::default())).expect("the reply");
        let items = input_items(&reply.history).unwrap();
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[1], InputItem::Message { parts, .. } if matches!(&parts[0], Part::Text(t) if t == "second")));
    }

    #[test]
    fn replies_keep_streaming_in_the_background() {
        let mut chat = project_chat();
        let first = start(&mut chat, "one");
        chat.new_conversation();
        let second = start(&mut chat, "two");
        assert_ne!(first.conversation, second.conversation);
        assert_eq!(chat.conversations.iter().filter(|c| c.busy()).count(), 2, "both run at once");

        // Events for the chat in the background land in it, not the open one.
        chat.stream_event(first.conversation, first.stream, StreamEvent::Text("a".into()));
        chat.stream_event(second.conversation, second.stream, StreamEvent::Text("b".into()));
        finish(&mut chat, &first, Completion::default());
        finish(&mut chat, &second, Completion::default());
        let reply = |id| chat.conversations.iter().find(|c| c.id == id).and_then(|c| c.entries.last()).map(|e| e.message.content.clone());
        assert_eq!(reply(first.conversation).as_deref(), Some("a"));
        assert_eq!(reply(second.conversation).as_deref(), Some("b"));
        assert!(!chat.is_busy());
    }

    #[test]
    fn clear_starts_over_in_the_same_folder() {
        let mut chat = project_chat();
        let job = start(&mut chat, "hi");
        let project = chat.current().project.clone();
        chat.composer.insert("/cl");
        let mut actions = Vec::new();
        let enter = |chat: &mut Chat, actions: &mut Vec<Action>| {
            let command = chat.commands()[chat.command_pick];
            chat.run_command(command, actions);
        };
        enter(&mut chat, &mut actions);
        assert!(job.cancel.load(Ordering::Relaxed), "the running reply is stopped");
        assert!(matches!(&actions[..], [Action::DeleteSession(_), Action::StopProcesses(_)]));
        assert!(chat.current().is_fresh() && chat.current().project == project);
        assert_eq!(chat.composer.text(), "");

        // Closing the menu keeps it closed until the text changes.
        chat.composer.insert("/");
        chat.sync_command_menu();
        assert!(chat.menu == Some(Menu::Commands));
        chat.close_menu();
        chat.sync_command_menu();
        assert!(chat.menu.is_none() && chat.commands().is_empty());
        chat.composer.insert("cl");
        assert_eq!(chat.commands(), [Command::Clear]);
    }

    #[test]
    fn long_runs_pause_and_continue() {
        let mut chat = project_chat();
        let job = start(&mut chat, "go");
        chat.current().steps = agent::STEP_BUDGET;
        let actions = finish(&mut chat, &job, Completion { tool_calls: vec![call("r", "read_file", "{}")], ..Completion::default() });
        assert!(next_send(actions).is_none());
        let mut actions = Vec::new();
        chat.tool_done(job.conversation, "r", Ok("x".into()), None, &mut actions);
        assert!(next_send(actions).is_none(), "paused");
        assert!(chat.current().resumable());
        let mut actions = Vec::new();
        chat.resume(job.conversation, &mut actions);
        assert!(next_send(actions).is_some());
        assert_eq!(chat.current().steps, 0);
    }

    /// Types `text` and presses Enter.
    fn enter(chat: &mut Chat, text: &str) -> Vec<Action> {
        chat.composer.insert(text);
        let mut actions = Vec::new();
        chat.send(&mut actions);
        actions
    }

    #[test]
    fn generations_wait_for_their_file_and_survive_a_restart() {
        let mut chat = Chat::new(None, Reasoning::Auto, Vec::new(), Vec::new(), None);
        chat.set_media_choice(MediaKind::Video, Some("veo".into()));
        let actions = enter(&mut chat, "/video  waves at dusk ");
        let Some(Action::Generate(request)) = actions.iter().find(|a| matches!(a, Action::Generate(_))) else { panic!("no generation") };
        assert_eq!((request.kind, request.model.as_str(), request.prompt.as_str(), request.job.as_deref()), (MediaKind::Video, "veo", "waves at dusk", None));
        let (id, entry) = (request.conversation, request.entry);
        assert!(!chat.current().busy(), "chatting goes on while it is made");
        assert!(!chat.current().resumable(), "a generation is not a prompt the model owes");

        let mut actions = Vec::new();
        chat.media_started(id, entry, MediaTicket { id: "job-1".into(), sparks: 80.0 }, &mut actions);
        let Some(Action::SaveSession(saved)) = actions.pop() else { panic!("not saved") };
        let reply = saved.messages.last().unwrap();
        assert_eq!(reply.media.as_ref().map(|j| j.id.as_str()), Some("job-1"));
        assert!((reply.cost - 0.8).abs() < 1e-9);

        // The app closes; the session reopens and keeps waiting for the job.
        let mut reopened = Chat::new(None, Reasoning::Auto, vec![saved.summary()], Vec::new(), None);
        let conversation = reopened.conversations.iter().find(|c| c.session_id == saved.id).map(|c| c.id).unwrap();
        reopened.open(conversation, &mut Vec::new());
        let mut actions = Vec::new();
        reopened.session_loaded(conversation, Ok(saved.clone()), &mut actions);
        assert!(actions.iter().any(|a| matches!(a, Action::Generate(r) if r.job.as_deref() == Some("job-1"))));

        let file = Attachment { name: "waves.mp4".into(), mime: "video/mp4".into(), size: 9, path: "x".into(), dimensions: None };
        let mut actions = Vec::new();
        chat.media_done(id, entry, Ok(Some(file)), &mut actions);
        assert!(chat.current().entries.last().is_some_and(|e| !e.message.media_pending() && e.message.attachments.len() == 1));
        let history: Vec<StoredMessage> = chat.current().entries.iter().map(|e| e.message.clone()).collect();
        let items = input_items(&history).unwrap();
        assert!(matches!(&items[1], InputItem::Message { parts, .. } if matches!(&parts[0], Part::Text(t) if t.contains("video waves.mp4"))));

        // A request the server never accepted cannot be picked up again.
        let mut lost = saved;
        if let Some(job) = &mut lost.messages.last_mut().unwrap().media {
            job.id.clear();
        }
        let mut chat = Chat::new(None, Reasoning::Auto, vec![lost.summary()], Vec::new(), None);
        let conversation = chat.conversations.iter().find(|c| c.session_id == lost.id).map(|c| c.id).unwrap();
        chat.open(conversation, &mut Vec::new());
        let mut actions = Vec::new();
        chat.session_loaded(conversation, Ok(lost), &mut actions);
        assert!(!actions.iter().any(|a| matches!(a, Action::Generate(_))));
        assert!(chat.current().entries.last().is_some_and(|e| e.message.failed));

        // Generating takes text only.
        let mut chat = Chat::new(None, Reasoning::Auto, Vec::new(), Vec::new(), None);
        chat.pending.push(Attachment { name: "a.png".into(), mime: "image/png".into(), size: 1, path: "a".into(), dimensions: None });
        assert!(enter(&mut chat, "/image a fox").is_empty());
        assert!(chat.notice.is_some());
    }

    #[test]
    fn compact_on_request_summarises_without_replying() {
        let mut chat = project_chat();
        let job = start(&mut chat, "hello");
        chat.stream_event(job.conversation, job.stream, StreamEvent::Text("hi".into()));
        finish(&mut chat, &job, Completion { usage: Usage::new(500, 20), ..Completion::default() });

        let summary = next_send(enter(&mut chat, "/compact")).expect("a summary request");
        assert_eq!(summary.tool_choice, Some("none"));
        chat.stream_event(summary.conversation, summary.stream, StreamEvent::Text("They said hello.".into()));
        let actions = finish(&mut chat, &summary, Completion { usage: Usage::new(600, 10), ..Completion::default() });
        assert!(next_send(actions).is_none(), "nothing is owed after /compact");
        assert!(!chat.current().resumable());
        assert!(chat.current().entries.last().is_some_and(|e| e.message.compaction));

        // Twice in a row has nothing left to summarise.
        assert!(next_send(enter(&mut chat, "/compact")).is_none());
        assert!(chat.notice.is_some());
    }

    #[test]
    fn init_new_and_model_commands() {
        let mut chat = Chat::new(None, Reasoning::Auto, Vec::new(), Vec::new(), None);
        assert!(enter(&mut chat, "/init").is_empty(), "/init needs a project");

        let mut chat = project_chat();
        let job = next_send(enter(&mut chat, "/init")).expect("/init is sent");
        assert!(job.history[0].content == "/init", "the chat shows what was typed");
        let items = input_items(&job.history).unwrap();
        assert!(matches!(&items[0], InputItem::Message { parts, .. } if matches!(&parts[0], Part::Text(t) if t.contains("AGENTS.md"))));

        let project = chat.current().project.clone();
        let before = chat.current;
        enter(&mut chat, "/new");
        assert!(chat.current != before && chat.current().is_fresh() && chat.current().project == project, "a new chat in the same folder");

        enter(&mut chat, "/model");
        assert!(chat.menu == Some(Menu::Model) && chat.composer.text().is_empty());
        // A command followed by text is just a message.
        assert!(next_send(enter(&mut chat, "/new plans")).is_some());
    }

    #[test]
    fn mcp_tools_work_without_a_project_and_ask_first() {
        let mut chat = Chat::new(None, Reasoning::Auto, Vec::new(), Vec::new(), None);
        let job = start(&mut chat, "file an issue");
        let calls = vec![call("m", "mcp__tracker__create_issue", r#"{"title":"Bug"}"#), call("r", "read_file", "{}")];
        let actions = finish(&mut chat, &job, Completion { tool_calls: calls, ..Completion::default() });
        assert!(!actions.iter().any(|a| matches!(a, Action::RunTool(_))), "an MCP tool not known to be read-only waits");
        let records = chat.current().tool_calls().cloned().unwrap();
        assert_eq!(records[0].status, ToolStatus::Pending);
        assert!(records[1].status == ToolStatus::Failed && records[1].output.contains("only available in a project"));
        let view = tools::view(&records[0].call);
        assert_eq!((view.verb, view.target.as_str()), ("Use", "tracker · create_issue"));

        let entry = chat.current().entries.len() - 1;
        let mut actions = Vec::new();
        chat.decide(entry, 0, Decision::Allow, &mut actions);
        let runs: Vec<&ToolJob> = actions.iter().filter_map(|a| if let Action::RunTool(t) = a { Some(t) } else { None }).collect();
        assert!(runs.len() == 1 && runs[0].root.is_none() && runs[0].call.name == "mcp__tracker__create_issue");
    }

    #[test]
    fn file_changes_are_diffed_before_approval() {
        let mut chat = project_chat();
        let job = start(&mut chat, "write it");
        let calls = vec![call("w", "write_file", r#"{"path":"x.txt","content":"one\ntwo"}"#), call("r", "read_file", r#"{"path":"x.txt"}"#)];
        let actions = finish(&mut chat, &job, Completion { tool_calls: calls, ..Completion::default() });
        let previews: Vec<&str> = actions.iter().filter_map(|a| if let Action::PreviewChange { call, .. } = a { Some(call.call_id.as_str()) } else { None }).collect();
        assert_eq!(previews, ["w"], "only the change waiting for approval");

        let diff = Arc::new(crate::diff::diff("one\n", "one\ntwo\n", true));
        chat.diff_ready(job.conversation, "w", diff);
        let entry = chat.current().entries.iter().find(|e| e.diffs.contains_key("w")).expect("stored with its call");
        assert_eq!(entry.message.tool_calls[0].call.call_id, "w");
    }
}
