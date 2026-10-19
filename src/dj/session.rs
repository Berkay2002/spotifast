//! What the app holds for the DJ: the chat as lines, the provider, and the two channels that cross
//! threads.
//!
//! * Events (text, tool log lines, the end of a turn) come from the turn's thread. [`Dj::poll`]
//!   applies them on the UI thread, so the page only ever reads plain data.
//! * Tool calls come from the MCP server's request threads. Each waits for an answer from the UI
//!   thread (`crate::app::dj` runs it against the app and replies); [`Dj::take_calls`] hands them
//!   over. Both directions wake the window through the shell's [`Waker`], which also works when the
//!   window is hidden in the tray.

use super::Card;
use super::agent::{self, Chats, Feed, Memory, Schedules};
use super::claude::{ClaudeProvider, ToolHost};
use super::tools::{Call, ToolReply};
use super::{AskRequest, ChatEvent, Provider, Sink, Status};
use crate::i18n::{Locale, gettext};
use fastframe_shell::Waker;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, atomic::Ordering};
use std::time::{Duration, Instant};

/// How long a tool call waits for the app before it is told the app did not answer. The CLI's own
/// limit for a call is longer (`claude.rs`).
const TOOL_WAIT: Duration = Duration::from_secs(30);
/// What a change waiting for approval is told when the turn that asked for it is over: the model
/// has been told it failed, so approving later must not run it.
const TURN_ENDED: &str = "The request ended before the listener answered, so nothing was changed.";
/// How long the listener has to approve a change before it counts as declined.
pub const APPROVAL_WAIT: Duration = Duration::from_secs(240);
/// The chat keeps this many lines; older ones are dropped.
const KEPT_LINES: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    User,
    Assistant,
    /// Something the DJ did ("Queued 8 songs").
    Tool,
    /// The model's reasoning for the turn, right after the listener's message.
    Thought,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub text: String,
    /// What the page draws for a tool's result (not saved with the chat).
    pub card: Option<Card>,
    /// On a `Thought`: the seconds spent thinking. On a `User` line: how long its whole answer took.
    pub secs: u32,
    /// The DJ's own turn (autopilot, schedule): it answers no message of the listener's.
    pub auto: bool,
}

impl Line {
    pub fn new(kind: LineKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            card: None,
            secs: 0,
            auto: false,
        }
    }
}

/// A tool call waiting for the UI thread.
pub struct PendingCall {
    pub call: Call,
    pub reply: Sender<Result<ToolReply, String>>,
}

/// A change to the library the DJ asked for and the listener has not answered yet.
pub struct Approval {
    pub id: u64,
    /// Exactly what would change, in the listener's words.
    pub summary: String,
    pub call: Call,
    pub reply: Sender<Result<ToolReply, String>>,
    pub asked: Instant,
}

enum Msg {
    Chat(ChatEvent),
    Status(Status),
}

pub struct Dj {
    provider: Arc<dyn Provider>,
    waker: Waker,
    events_tx: Sender<Msg>,
    events: Receiver<Msg>,
    calls: Receiver<PendingCall>,
    pub lines: Vec<Line>,
    /// The last line is an assistant message still being written.
    live: bool,
    pub busy: bool,
    /// "connecting" or "thinking" while a turn runs.
    pub phase: Option<&'static str>,
    /// `None` until asked; refreshed with [`Dj::refresh_status`].
    pub status: Option<Status>,
    status_loading: bool,
    /// The hint of the last error (`login`, `install`, `env`, `limit`, `dead`).
    pub hint: Option<String>,
    /// The chat box's text.
    pub input: String,
    /// Bumped whenever lines change, so the page can scroll to the end.
    pub revision: u64,
    /// Whether the model may search the web (the setting, kept current by the app).
    pub web: bool,
    /// The app's language, kept current by the app, for the lines the chat adds itself.
    pub locale: Locale,
    /// Changes waiting for the listener's yes or no, oldest first.
    pub approvals: Vec<Approval>,
    next_approval: u64,
    /// What the DJ remembers, its scheduled tasks and the events that wake it.
    pub memory: Memory,
    pub schedules: Schedules,
    pub feed: Feed,
    chats: Chats,
    chat_id: u64,
    /// When the open chat began (seconds since the epoch), for the stamp above it.
    pub(crate) chat_at: Option<i64>,
    /// The turn in flight is the DJ's own (an event or a schedule), not an answer to a message.
    background: bool,
    /// Where the DJ keeps its files, and the account they are open for.
    dir: Option<(std::path::PathBuf, Arc<fastframe_file::Writer>)>,
    account: Option<String>,
    /// Stop was pressed: calls still arriving for the turn are refused, not run.
    stopping: bool,
    /// The DJ setting is off and the child was shut down for it.
    off: bool,
    /// The notes block the conversation was last given.
    notes_sent: Option<String>,
    /// When the turn in flight began, and when its current stretch of thinking did.
    turn_started: Option<Instant>,
    thought_started: Option<Instant>,
}

impl Dj {
    pub fn new(waker: Waker) -> Self {
        Self::build(waker, |host| ClaudeProvider::new(host) as Arc<dyn Provider>)
    }

    /// `make` gets the tool host (what the provider runs a call through) and returns the provider.
    pub fn build(waker: Waker, make: impl FnOnce(ToolHost) -> Arc<dyn Provider>) -> Self {
        let (events_tx, events) = mpsc::channel();
        let (calls_tx, calls) = mpsc::channel::<PendingCall>();
        let wake = waker.clone();
        let host: ToolHost = Arc::new(move |call: Call, cancel: &AtomicBool| {
            if cancel.load(Ordering::SeqCst) {
                return Err("The listener stopped the DJ, so nothing was run.".to_string());
            }
            let needs_approval = call.needs_approval();
            let (reply, answer) = mpsc::channel();
            calls_tx
                .send(PendingCall { call, reply })
                .map_err(|_| "Spotifast is closing.".to_string())?;
            wake.wake();
            // a change waits for the listener, who may be away from the screen for a while
            let wait = if needs_approval {
                APPROVAL_WAIT + TOOL_WAIT
            } else {
                TOOL_WAIT
            };
            let deadline = Instant::now() + wait;
            loop {
                match answer.recv_timeout(Duration::from_millis(200)) {
                    Ok(r) => return r,
                    Err(RecvTimeoutError::Disconnected) => {
                        return Err("Spotifast is closing.".to_string());
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if cancel.load(Ordering::SeqCst) {
                            return Err("The listener stopped the DJ.".to_string());
                        }
                        if Instant::now() > deadline {
                            return Err("Spotifast did not answer in time. Try again.".to_string());
                        }
                    }
                }
            }
        });
        Self {
            provider: make(host),
            waker,
            events_tx,
            events,
            calls,
            lines: Vec::new(),
            live: false,
            busy: false,
            phase: None,
            status: None,
            status_loading: false,
            hint: None,
            input: String::new(),
            revision: 0,
            web: false,
            locale: Locale::default(),
            approvals: Vec::new(),
            next_approval: 0,
            memory: Memory::default(),
            schedules: Schedules::default(),
            feed: Feed::default(),
            chats: Chats::default(),
            chat_id: 1,
            chat_at: None,
            background: false,
            dir: None,
            account: None,
            stopping: false,
            off: false,
            notes_sent: None,
            turn_started: None,
            thought_started: None,
        }
    }

    /// Where the DJ keeps its files. What it kept is opened per account (`set_account`), so
    /// another account on this machine does not inherit its notes, schedules and chats.
    /// They are written through `files`, the app's writer, off the UI thread.
    pub fn attach(&mut self, dir: &Path, files: Arc<fastframe_file::Writer>) {
        self.dir = Some((dir.to_path_buf(), files));
    }

    /// Opens the notes, schedules and saved chats of the signed-in account. The newest chat is
    /// not reopened: every start is a new conversation. Files from before there were accounts
    /// (`dj-memory.json` and the others) become the first account's, once.
    ///
    /// Returns whether it replaced another account. That account's conversation, child and
    /// waiting approvals go first (`shutdown`), so nothing of theirs runs under this one; the same
    /// account signing in again keeps everything.
    pub fn set_account(&mut self, id: &str) -> bool {
        if self.account.as_deref() == Some(id) {
            return false;
        }
        let Some((dir, files)) = self.dir.clone() else {
            return false;
        };
        let replaced = self.account.is_some();
        if replaced {
            self.shutdown();
        }
        // Spotify's ids are plain letters and digits; anything else cannot reach a file name
        let tag: String = id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let file = |base: &str| {
            let mine = dir.join(format!("{base}.{tag}.json"));
            let old = dir.join(format!("{base}.json"));
            if !mine.exists() && old.exists() {
                let _ = std::fs::rename(&old, &mine);
            }
            (mine, files.clone())
        };
        self.memory = Memory::open(file("dj-memory"));
        self.schedules = Schedules::open(file("dj-schedules"));
        self.chats = Chats::open(file("dj-chats"));
        self.chat_id = self.chats.next_id();
        self.account = Some(id.to_string());
        replaced
    }

    /// The saved chats, newest first.
    pub fn chats(&self) -> &[agent::Chat] {
        self.chats.all()
    }

    /// The open chat's id (the saved one has the same).
    pub fn chat_id(&self) -> u64 {
        self.chat_id
    }

    /// When the open chat began, once it has a message.
    pub fn chat_time(&self) -> Option<i64> {
        self.chat_at
    }

    /// Starts an empty conversation; the one that was open stays saved.
    pub fn new_chat(&mut self) {
        if self.busy {
            return;
        }
        self.save_chat();
        self.provider.set_session(None);
        self.notes_sent = None;
        self.lines.clear();
        self.live = false;
        self.chat_id = self.chats.next_id();
        self.chat_at = None;
        self.revision += 1;
    }

    /// Reopens a saved chat and continues its conversation.
    pub fn open_chat(&mut self, id: u64) {
        if self.busy {
            return;
        }
        let Some(chat) = self.chats.get(id).cloned() else {
            return;
        };
        self.save_chat();
        self.lines = chat.lines();
        self.live = false;
        self.chat_id = chat.id;
        self.chat_at = Some(chat.updated);
        self.provider.set_session(chat.session);
        self.notes_sent = None;
        self.revision += 1;
    }

    pub fn delete_chat(&mut self, id: u64) {
        self.chats.delete(id);
        if id == self.chat_id && !self.busy {
            // not `new_chat`: that would save the chat that was just deleted
            self.provider.set_session(None);
            self.notes_sent = None;
            self.lines.clear();
            self.live = false;
            self.chat_id = self.chats.next_id();
            self.chat_at = None;
            self.revision += 1;
        }
    }

    fn save_chat(&mut self) {
        let now = jiff::Timestamp::now().as_second();
        self.chats
            .put(self.chat_id, &self.lines, self.provider.session(), now);
    }

    /// Puts a change the DJ asked for in front of the listener.
    pub fn ask_approval(
        &mut self,
        summary: String,
        call: Call,
        reply: Sender<Result<ToolReply, String>>,
    ) {
        self.next_approval += 1;
        self.approvals.push(Approval {
            id: self.next_approval,
            summary,
            call,
            reply,
            asked: Instant::now(),
        });
        self.revision += 1;
    }

    /// The approval `id`, removed from the waiting list for the app to run or decline.
    pub fn take_approval(&mut self, id: u64) -> Option<Approval> {
        let at = self.approvals.iter().position(|a| a.id == id)?;
        self.revision += 1;
        Some(self.approvals.remove(at))
    }

    /// Answers every waiting approval with a decline.
    pub fn decline_all(&mut self, why: &str) {
        for a in self.approvals.drain(..) {
            let _ = a.reply.send(Err(why.to_string()));
        }
        self.revision += 1;
    }

    /// Declines the approvals nobody answered in time; returns their summaries.
    pub fn expire_approvals(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        let mut kept = Vec::new();
        for a in self.approvals.drain(..) {
            if a.asked.elapsed() > APPROVAL_WAIT {
                let _ = a.reply.send(Err(
                    "The listener did not answer in time, so nothing was changed.".to_string(),
                ));
                out.push(a.summary);
            } else {
                kept.push(a);
            }
        }
        self.approvals = kept;
        if !out.is_empty() {
            self.revision += 1;
        }
        out
    }

    /// Whether the turn in flight is the DJ's own (an event or a schedule) and not an answer to
    /// something the listener just typed.
    pub fn unprompted(&self) -> bool {
        self.busy && self.background
    }

    /// Adds a line the DJ did not say (the app's own note in the activity log).
    pub fn note(&mut self, text: String) {
        self.push(LineKind::Tool, text);
    }

    /// Whether the provider has been asked for its status and has not answered yet.
    pub fn checking(&self) -> bool {
        self.status_loading
    }

    /// Asks the provider whether it is installed and signed in, off the UI thread.
    pub fn refresh_status(&mut self) {
        if self.status_loading {
            return;
        }
        self.status_loading = true;
        let provider = self.provider.clone();
        let tx = self.events_tx.clone();
        let waker = self.waker.clone();
        let started = std::thread::Builder::new()
            .name("dj-status".into())
            .spawn(move || {
                let st = provider.status();
                let _ = tx.send(Msg::Status(st));
                waker.wake();
            });
        if started.is_err() {
            self.status_loading = false;
        }
    }

    /// Sends `prompt` as the listener's message.
    pub fn ask(&mut self, prompt: &str, model: &str, effort: &str) {
        self.start(prompt, model, effort, true);
    }

    /// Runs a turn of the DJ's own (an event, a schedule): no message of the listener's is shown,
    /// and a plain "ok" answer leaves no trace.
    pub fn ask_unprompted(&mut self, prompt: &str, model: &str, effort: &str) {
        self.start(prompt, model, effort, false);
    }

    fn start(&mut self, prompt: &str, model: &str, effort: &str, from_listener: bool) {
        let prompt = prompt.trim();
        if prompt.is_empty() || self.busy {
            return;
        }
        if from_listener {
            self.push(LineKind::User, prompt.to_string());
            self.chat_at
                .get_or_insert_with(|| jiff::Timestamp::now().as_second());
        }
        self.background = !from_listener;
        self.stopping = false;
        self.turn_started = Some(Instant::now());
        self.thought_started = None;
        // the notes go along when they are new to this conversation
        let sent = match self.memory.block() {
            Some(notes) if self.notes_sent.as_deref() != Some(notes.as_str()) => {
                let sent = format!("{notes}\n{prompt}");
                self.notes_sent = Some(notes);
                sent
            }
            _ => prompt.to_string(),
        };
        self.hint = None;
        self.busy = true;
        self.phase = Some("connecting");
        let tx = self.events_tx.clone();
        let waker = self.waker.clone();
        let sink: Sink = Arc::new(move |ev| {
            let _ = tx.send(Msg::Chat(ev));
            waker.wake();
        });
        let req = AskRequest {
            prompt: sent,
            web: self.web,
            model: Some(model.to_string()),
            effort: Some(effort.to_string()),
        };
        if let Err(e) = self.provider.clone().ask(req, sink) {
            self.busy = false;
            self.phase = None;
            self.push(LineKind::Error, e);
        }
    }

    /// Stops the turn in flight. It ends as a `Cancelled` event.
    pub fn cancel(&mut self) {
        self.stopping = true;
        self.decline_all("The listener stopped the DJ.");
        self.refuse_calls("The listener stopped the DJ, so nothing was changed.");
        self.provider.cancel();
    }

    /// Answers the calls still queued with an error; none of them is run.
    fn refuse_calls(&mut self, why: &str) {
        for pending in self.calls.try_iter() {
            let _ = pending.reply.send(Err(why.to_string()));
        }
    }

    /// The DJ setting is off: the child and its tool server go, once, busy or not.
    pub fn switch_off(&mut self) {
        if !self.off {
            self.off = true;
            self.shutdown();
        }
    }

    pub fn switch_on(&mut self) {
        self.off = false;
    }

    /// App exit and sign-out: the child and its tool server go, and the chat starts over.
    pub fn shutdown(&mut self) {
        self.decline_all("Spotifast is closing.");
        self.feed.clear();
        self.provider.shutdown();
        self.lines.clear();
        self.live = false;
        self.busy = false;
        self.phase = None;
        self.hint = None;
        // the next conversation is a new chat: the saved one keeps its id, and the notes go again
        self.chat_id = self.chats.next_id();
        self.notes_sent = None;
        self.chat_at = None;
        self.revision += 1;
    }

    /// Applies what the turn's thread and the status check have said since the last frame.
    pub fn poll(&mut self) {
        while let Ok(m) = self.events.try_recv() {
            match m {
                Msg::Status(st) => {
                    self.status_loading = false;
                    self.status = Some(st);
                }
                Msg::Chat(ev) => self.apply(ev),
            }
        }
    }

    /// The tool calls waiting for an answer, oldest first.
    pub fn take_calls(&mut self) -> Vec<PendingCall> {
        // after Stop, or with no turn in flight, a call that still arrives is not run
        if self.stopping || !self.busy {
            self.refuse_calls("No request is being answered right now, so nothing was run.");
            return Vec::new();
        }
        self.calls.try_iter().collect()
    }

    fn push(&mut self, kind: LineKind, text: String) {
        let mut line = Line::new(kind, text);
        line.auto = self.background;
        self.lines.push(line);
        if self.lines.len() > KEPT_LINES {
            let extra = self.lines.len() - KEPT_LINES;
            self.lines.drain(..extra);
        }
        self.revision += 1;
    }

    /// The line of the listener's message the turn in flight answers.
    fn turn_user(&self) -> Option<usize> {
        self.lines.iter().rposition(|l| l.kind == LineKind::User)
    }

    /// Adds to the turn's reasoning, which sits right after the listener's message. The DJ's own
    /// turns have no message to hang it on, so they show none.
    fn think(&mut self, text: &str) {
        if self.background {
            return;
        }
        let Some(at) = self.turn_user().map(|user| user + 1) else {
            return;
        };
        if self
            .lines
            .get(at)
            .is_none_or(|l| l.kind != LineKind::Thought)
        {
            self.lines.insert(at, Line::new(LineKind::Thought, ""));
        }
        self.thought_started.get_or_insert_with(Instant::now);
        self.lines[at].text.push_str(text);
        self.revision += 1;
    }

    /// The model stopped thinking (it spoke, called a tool or finished): its time is added up.
    fn rest(&mut self) {
        let Some(started) = self.thought_started.take() else {
            return;
        };
        let secs = started.elapsed().as_secs_f32().round() as u32;
        if let Some(user) = self.turn_user()
            && let Some(line) = self.lines.get_mut(user + 1)
            && line.kind == LineKind::Thought
        {
            line.secs += secs;
        }
    }

    /// The turn is over: how long it took goes on the listener's message.
    fn finish_turn(&mut self) {
        self.rest();
        let Some(started) = self.turn_started.take() else {
            return;
        };
        if self.background {
            return;
        }
        if let Some(user) = self.turn_user() {
            self.lines[user].secs = started.elapsed().as_secs_f32().round() as u32;
        }
    }

    fn apply(&mut self, ev: ChatEvent) {
        match ev {
            ChatEvent::Status { phase } => self.phase = Some(phase),
            ChatEvent::Thinking { text } => self.think(&text),
            ChatEvent::Delta { text } => {
                self.rest();
                if self.live {
                    if let Some(last) = self.lines.last_mut() {
                        last.text.push_str(&text);
                        self.revision += 1;
                    }
                } else {
                    self.push(LineKind::Assistant, text);
                    self.live = true;
                }
            }
            ChatEvent::Reset => {
                if self.live {
                    self.lines.pop();
                    self.live = false;
                    self.revision += 1;
                }
            }
            ChatEvent::Tool { text, card } => {
                self.rest();
                self.live = false;
                self.push(LineKind::Tool, text);
                if let Some(line) = self.lines.last_mut() {
                    line.card = card;
                }
            }
            ChatEvent::Done { answer } => {
                self.finish_turn();
                self.decline_all(TURN_ENDED);
                if self.background && agent::is_nothing(&answer) {
                    if self.live {
                        self.lines.pop();
                        self.revision += 1;
                    }
                } else if !self.live && !answer.is_empty() {
                    self.push(LineKind::Assistant, answer);
                }
                self.live = false;
                self.busy = false;
                self.phase = None;
                self.save_chat();
            }
            ChatEvent::Cancelled => {
                self.finish_turn();
                self.decline_all(TURN_ENDED);
                self.live = false;
                self.busy = false;
                self.phase = None;
                self.push(
                    LineKind::Tool,
                    gettext(self.locale, "Stopped.").into_owned(),
                );
                self.save_chat();
            }
            ChatEvent::Error { message, hint } => {
                self.finish_turn();
                self.decline_all(TURN_ENDED);
                self.live = false;
                self.busy = false;
                self.phase = None;
                if let (Some(h), Some(st)) = (hint.as_deref(), self.status.as_mut()) {
                    match h {
                        "install" => st.installed = false,
                        "login" => st.logged_in = false,
                        _ => {}
                    }
                }
                self.hint = hint;
                self.push(LineKind::Error, error_text(&message, self.locale));
            }
        }
    }

    /// Whether the DJ can be started: the provider is installed and signed in, or not yet known
    /// (the first message finds out).
    pub fn ready(&self) -> bool {
        match &self.status {
            Some(st) => st.installed && st.logged_in && st.error.is_none(),
            None => true,
        }
    }
}

impl Drop for Dj {
    fn drop(&mut self) {
        self.provider.shutdown();
    }
}

/// An error message from the provider in the listener's language. The provider's fixed messages
/// (`claude::NOT_INSTALLED` and the others) are translated; text that comes from Claude Code
/// itself, or from the system, is shown as it arrived.
pub fn error_text(message: &str, locale: Locale) -> String {
    use super::claude as c;
    let fixed = match message {
        c::NOT_INSTALLED => gettext(locale, "Claude Code is not installed."),
        c::NOT_RUNNING => gettext(locale, "Claude Code is not running. Try again."),
        c::STOPPED => gettext(
            locale,
            "Claude Code stopped unexpectedly. Try again to restart it.",
        ),
        c::NO_RESPONSE => gettext(locale, "Claude stopped responding."),
        c::CLOSING => gettext(locale, "Spotifast is closing."),
        c::LISTENER_STOPPED => gettext(locale, "The listener stopped the DJ."),
        c::REQUEST_FAILED => gettext(locale, "The request failed."),
        c::NOT_SIGNED_IN => gettext(
            locale,
            "You are not signed in to Claude. Run `claude login` in a terminal, then try again.",
        ),
        c::API_KEY => gettext(
            locale,
            "Claude Code is set to use an API key, not your Claude plan. Spotifast only uses your plan (`claude login`), so it stopped.",
        ),
        c::BAD_MODEL => gettext(
            locale,
            "That model name is not one Claude Code offers. Pick a model in Settings.",
        ),
        _ => {
            // fixed words in front of text that arrived with the error
            return if let Some(tail) = message.strip_prefix(c::STOPPED_WITH) {
                // Translators: Keep {reason} exactly as written. It becomes the reason Claude Code gave.
                gettext(locale, "Claude Code stopped: {reason}").replace("{reason}", tail)
            } else if let Some(tail) = message.strip_prefix(c::LIMIT) {
                // Translators: Keep {details} exactly as written. It becomes what Claude said about the
                // limit, such as when it resets, and may be empty.
                gettext(locale, "Your Claude usage limit is reached.{details}")
                    .replace("{details}", tail)
            } else if let Some(tail) = message.strip_prefix(c::PREPARE) {
                // Translators: Keep {error} exactly as written. It becomes the system's error text.
                gettext(locale, "Could not prepare Claude: {error}").replace("{error}", tail)
            } else if let Some(tail) = message.strip_prefix(c::TOOL_SERVER) {
                // Translators: Keep {error} exactly as written. It becomes the system's error text.
                gettext(locale, "Could not start Spotifast's tool server: {error}")
                    .replace("{error}", tail)
            } else if let Some(names) = message
                .strip_prefix("Your environment sets ")
                .and_then(|rest| rest.split_once(", which would"))
                .map(|(names, _)| names)
            {
                // Translators: Keep {names} exactly as written. It becomes the names of environment
                // variables, such as ANTHROPIC_API_KEY.
                gettext(
                    locale,
                    "Your environment sets {names}, which would make Claude Code use that instead of your Claude plan. Spotifast uses only the plan you sign in to with `claude login`, so it will not start Claude while that is set. Remove it (or start Spotifast without it) and try again.",
                )
                .replace("{names}", names)
            } else {
                message.to_string()
            };
        }
    };
    fixed.into_owned()
}

/// What to do about the last error, by its hint code (`login`, `install`, `env`, `limit`,
/// `dead`); `None` for a code with nothing to add.
pub fn hint_text(hint: &str, locale: Locale) -> Option<String> {
    Some(
        match hint {
            "login" => gettext(
                locale,
                "Run `claude login` in a terminal, then send your message again.",
            ),
            "install" => gettext(
                locale,
                "Install Claude Code from claude.com/claude-code, then run `claude login` in a terminal.",
            ),
            "env" => gettext(
                locale,
                "Remove the API key from Spotifast's environment, then start it again.",
            ),
            "limit" => gettext(
                locale,
                "Wait for your Claude usage limit to reset, or pick a smaller model in Settings.",
            ),
            "dead" => gettext(locale, "Send your message again to restart Claude Code."),
            _ => return None,
        }
        .into_owned(),
    )
}

/// The page's explanation when the provider cannot be used, `None` when it can.
pub fn blocker(st: &Status, locale: Locale) -> Option<String> {
    if !st.installed {
        return Some(
            gettext(
                locale,
                "Claude Code isn't installed. The DJ uses your Claude subscription through the Claude Code CLI. Install it from claude.com/claude-code, then run `claude login` in a terminal.",
            )
            .into_owned(),
        );
    }
    if let Some(e) = &st.error {
        return Some(error_text(e, locale));
    }
    if !st.logged_in {
        return Some(
            gettext(
                locale,
                "Sign in to Claude Code. Run `claude login` in a terminal and sign in with your Claude account. Spotifast never sees your login.",
            )
            .into_owned(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A provider that replays scripted events and lets a test inspect what it was asked.
    struct Fake {
        host: ToolHost,
        script: Mutex<Vec<ChatEvent>>,
        asked: Mutex<Vec<AskRequest>>,
        status: Status,
    }

    impl Provider for Fake {
        fn id(&self) -> super::super::ProviderId {
            super::super::ProviderId::Claude
        }
        fn status(&self) -> Status {
            self.status.clone()
        }
        fn ask(self: Arc<Self>, req: AskRequest, sink: Sink) -> Result<(), String> {
            self.asked.lock().unwrap().push(req);
            for ev in self.script.lock().unwrap().drain(..) {
                sink(ev);
            }
            Ok(())
        }
        fn cancel(&self) {}
        fn shutdown(&self) {}
    }

    fn dj_with(script: Vec<ChatEvent>, status: Status) -> (Dj, Arc<Mutex<Option<Arc<Fake>>>>) {
        let slot: Arc<Mutex<Option<Arc<Fake>>>> = Arc::new(Mutex::new(None));
        let s = slot.clone();
        let dj = Dj::build(Waker::default(), move |host| {
            let f = Arc::new(Fake {
                host,
                script: Mutex::new(script),
                asked: Mutex::new(Vec::new()),
                status,
            });
            *s.lock().unwrap() = Some(f.clone());
            f
        });
        (dj, slot)
    }

    fn ok_status() -> Status {
        Status {
            installed: true,
            logged_in: true,
            ..Status::default()
        }
    }

    #[test]
    fn a_turn_becomes_lines_in_order() {
        let (mut dj, slot) = dj_with(
            vec![
                ChatEvent::Status { phase: "thinking" },
                ChatEvent::Delta {
                    text: "Let me look.".into(),
                },
                ChatEvent::Reset,
                ChatEvent::Tool {
                    text: "Searched for \"bonobo\"".into(),
                    card: None,
                },
                ChatEvent::Tool {
                    text: "Queued 8 songs".into(),
                    card: None,
                },
                ChatEvent::Delta {
                    text: "Eight ".into(),
                },
                ChatEvent::Delta {
                    text: "queued.".into(),
                },
                ChatEvent::Done {
                    answer: "Eight queued.".into(),
                },
            ],
            ok_status(),
        );
        dj.ask("  start a set  ", "sonnet", "low");
        assert!(dj.busy);
        dj.poll();
        assert!(!dj.busy);
        assert_eq!(dj.phase, None);
        let got: Vec<(LineKind, &str)> =
            dj.lines.iter().map(|l| (l.kind, l.text.as_str())).collect();
        assert_eq!(
            got,
            vec![
                (LineKind::User, "start a set"),
                (LineKind::Tool, "Searched for \"bonobo\""),
                (LineKind::Tool, "Queued 8 songs"),
                (LineKind::Assistant, "Eight queued."),
            ]
        );
        let f = slot.lock().unwrap().clone().unwrap();
        let asked = f.asked.lock().unwrap();
        assert_eq!(asked[0].prompt, "start a set");
        assert_eq!(asked[0].model.as_deref(), Some("sonnet"));
        assert_eq!(asked[0].effort.as_deref(), Some("low"));
    }

    #[test]
    fn thinking_sits_right_after_the_message_and_cards_ride_on_tool_lines() {
        let card = Card::Queued {
            songs: Vec::new(),
            total: 3,
        };
        let (mut dj, _) = dj_with(
            vec![
                ChatEvent::Thinking {
                    text: "They want ".into(),
                },
                ChatEvent::Thinking {
                    text: "something calm.".into(),
                },
                ChatEvent::Tool {
                    text: "Queued 3 songs".into(),
                    card: Some(card.clone()),
                },
                ChatEvent::Done {
                    answer: "Queued.".into(),
                },
            ],
            ok_status(),
        );
        dj.ask("something calm", "sonnet", "low");
        dj.poll();
        let got: Vec<(LineKind, &str)> =
            dj.lines.iter().map(|l| (l.kind, l.text.as_str())).collect();
        assert_eq!(
            got,
            vec![
                (LineKind::User, "something calm"),
                (LineKind::Thought, "They want something calm."),
                (LineKind::Tool, "Queued 3 songs"),
                (LineKind::Assistant, "Queued."),
            ]
        );
        assert_eq!(dj.lines[2].card, Some(card));
        assert!(dj.chat_time().is_some());
    }

    #[test]
    fn the_djs_own_turns_show_no_thinking_and_new_chat_clears_the_stamp() {
        let (mut dj, _) = dj_with(
            vec![
                ChatEvent::Thinking {
                    text: "quiet queue".into(),
                },
                ChatEvent::Done {
                    answer: "ok".into(),
                },
            ],
            ok_status(),
        );
        dj.ask_unprompted("the queue is low", "sonnet", "low");
        dj.poll();
        assert!(dj.lines.iter().all(|l| l.kind != LineKind::Thought));
        assert!(dj.chat_time().is_none());
        dj.chat_at = Some(1);
        dj.new_chat();
        assert!(dj.chat_time().is_none());
    }

    #[test]
    fn an_answer_with_no_streamed_text_is_still_shown_and_an_empty_one_is_not() {
        let (mut dj, _) = dj_with(
            vec![ChatEvent::Done {
                answer: "Done.".into(),
            }],
            ok_status(),
        );
        dj.ask("x", "sonnet", "low");
        dj.poll();
        assert_eq!(dj.lines.last().unwrap().text, "Done.");
        let (mut dj, _) = dj_with(
            vec![ChatEvent::Done {
                answer: String::new(),
            }],
            ok_status(),
        );
        dj.ask("x", "sonnet", "low");
        dj.poll();
        assert_eq!(dj.lines.len(), 1);
    }

    #[test]
    fn an_error_ends_the_turn_and_marks_the_status() {
        let (mut dj, _) = dj_with(
            vec![ChatEvent::Error {
                message: "not signed in".into(),
                hint: Some("login".into()),
            }],
            ok_status(),
        );
        dj.status = Some(ok_status());
        dj.ask("x", "sonnet", "low");
        dj.poll();
        assert!(!dj.busy);
        assert_eq!(dj.hint.as_deref(), Some("login"));
        assert!(!dj.ready());
        assert_eq!(dj.lines.last().unwrap().kind, LineKind::Error);
    }

    #[test]
    fn a_second_message_waits_for_the_first() {
        let (mut dj, slot) = dj_with(Vec::new(), ok_status());
        dj.ask("one", "sonnet", "low");
        assert!(dj.busy);
        dj.ask("two", "sonnet", "low");
        assert_eq!(dj.lines.len(), 1);
        assert_eq!(
            slot.lock()
                .unwrap()
                .clone()
                .unwrap()
                .asked
                .lock()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_tool_call_crosses_to_the_ui_thread_and_back() {
        let (mut dj, slot) = dj_with(Vec::new(), ok_status());
        dj.busy = true;
        let f = slot.lock().unwrap().clone().unwrap();
        let cancel = AtomicBool::new(false);
        let host = f.host.clone();
        let t = std::thread::spawn(move || host(Call::Skip, &cancel));
        let calls = loop {
            let c = dj.take_calls();
            if !c.is_empty() {
                break c;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(calls[0].call, Call::Skip);
        calls[0]
            .reply
            .send(Ok(ToolReply {
                text: "skipped".into(),
                ..Default::default()
            }))
            .unwrap();
        assert_eq!(t.join().unwrap().unwrap().text, "skipped");
    }

    #[test]
    fn a_call_with_no_turn_in_flight_is_refused_and_not_handed_to_the_app() {
        let (mut dj, slot) = dj_with(Vec::new(), ok_status());
        let f = slot.lock().unwrap().clone().unwrap();
        let cancel = AtomicBool::new(false);
        let host = f.host.clone();
        let t = std::thread::spawn(move || host(Call::Skip, &cancel));
        while !t.is_finished() {
            assert!(dj.take_calls().is_empty());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(t.join().unwrap().unwrap_err().contains("nothing was run"));
    }

    #[test]
    fn stop_refuses_calls_that_were_queued_and_those_that_still_arrive() {
        let (mut dj, slot) = dj_with(Vec::new(), ok_status());
        dj.busy = true;
        let f = slot.lock().unwrap().clone().unwrap();
        let (host, cancel) = (f.host.clone(), Arc::new(AtomicBool::new(false)));
        let c = cancel.clone();
        let queued = std::thread::spawn(move || host(Call::Skip, &c));
        std::thread::sleep(Duration::from_millis(50));
        dj.cancel();
        assert!(queued.join().unwrap().unwrap_err().contains("stopped"));
        // the turn has not ended yet, and a call the model still sends is not run either
        let (host, cancel) = (f.host.clone(), AtomicBool::new(false));
        let late = std::thread::spawn(move || host(Call::Skip, &cancel));
        while !late.is_finished() {
            assert!(dj.take_calls().is_empty());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(late.join().unwrap().is_err());
    }

    #[test]
    fn each_account_has_its_own_files_and_the_old_ones_go_to_the_first() {
        let dir = std::env::temp_dir().join(format!("spotifast-dj-acc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("dj-memory.json"), br#"{"notes":["no country"]}"#).unwrap();
        let (mut dj, _) = dj_with(Vec::new(), ok_status());
        let files = Arc::new(fastframe_file::Writer::new());
        dj.attach(&dir, files.clone());
        dj.set_account("alice");
        assert_eq!(dj.memory.notes(), ["no country"]);
        assert!(!dir.join("dj-memory.json").exists());
        dj.set_account("bob");
        assert!(dj.memory.notes().is_empty());
        dj.memory.remember("likes jazz").unwrap();
        assert!(files.flush(fastframe_file::FLUSH_TIMEOUT));
        dj.set_account("alice");
        assert_eq!(dj.memory.notes(), ["no country"]);
        dj.set_account("bob");
        assert_eq!(dj.memory.notes(), ["likes jazz"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_waiting_tool_call_ends_when_the_turn_is_cancelled() {
        let (dj, slot) = dj_with(Vec::new(), ok_status());
        let f = slot.lock().unwrap().clone().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let (host, c2) = (f.host.clone(), cancel.clone());
        let t = std::thread::spawn(move || host(Call::Queue, &c2));
        std::thread::sleep(Duration::from_millis(50));
        cancel.store(true, Ordering::SeqCst);
        assert!(t.join().unwrap().unwrap_err().contains("stopped"));
        drop(dj);
    }

    #[test]
    fn the_status_check_runs_off_thread_and_lands_on_poll() {
        let (mut dj, _) = dj_with(
            Vec::new(),
            Status {
                installed: false,
                ..Status::default()
            },
        );
        dj.refresh_status();
        assert!(dj.checking());
        let t0 = Instant::now();
        while dj.status.is_none() && t0.elapsed() < Duration::from_secs(5) {
            dj.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!dj.checking());
        assert!(!dj.ready());
        assert!(
            blocker(dj.status.as_ref().unwrap(), Locale::default())
                .unwrap()
                .contains("isn't installed")
        );
    }

    #[test]
    fn provider_errors_are_translated_by_their_fixed_text() {
        use super::super::claude as c;
        let de = Locale::German;
        for fixed in [
            c::NOT_INSTALLED,
            c::NOT_RUNNING,
            c::STOPPED,
            c::NO_RESPONSE,
            c::CLOSING,
            c::LISTENER_STOPPED,
            c::REQUEST_FAILED,
            c::NOT_SIGNED_IN,
            c::API_KEY,
            c::BAD_MODEL,
        ] {
            assert_ne!(error_text(fixed, de), fixed, "{fixed}");
            assert_eq!(error_text(fixed, Locale::default()), fixed);
        }
        let limit = format!("{} Resets at 9pm.", c::LIMIT);
        assert!(error_text(&limit, de).contains("Resets at 9pm."));
        assert_ne!(error_text(&limit, de), limit);
        let env = super::super::exe::refusal(&["ANTHROPIC_API_KEY".into()]);
        assert!(error_text(&env, de).contains("ANTHROPIC_API_KEY"));
        assert_ne!(error_text(&env, de), env);
        // text from Claude Code itself is shown as it came
        assert_eq!(error_text("boom", de), "boom");
        for hint in ["login", "install", "env", "limit", "dead"] {
            assert_ne!(
                hint_text(hint, de).unwrap(),
                hint_text(hint, Locale::default()).unwrap()
            );
        }
        assert!(hint_text("other", de).is_none());
    }

    #[test]
    fn blockers_say_what_to_do() {
        assert!(blocker(&ok_status(), Locale::default()).is_none());
        let out = Status {
            installed: true,
            logged_in: false,
            ..Status::default()
        };
        assert!(
            blocker(&out, Locale::default())
                .unwrap()
                .contains("claude login")
        );
        let env = Status {
            installed: true,
            error: Some("env refuses".into()),
            ..Status::default()
        };
        assert_eq!(blocker(&env, Locale::default()).unwrap(), "env refuses");
    }

    type Answer = Result<ToolReply, String>;

    fn waiting_call() -> (Receiver<Answer>, Sender<Answer>) {
        let (tx, rx) = mpsc::channel();
        (rx, tx)
    }

    #[test]
    fn a_change_waits_for_an_answer_and_is_declined_when_none_comes() {
        let (mut dj, _) = dj_with(Vec::new(), ok_status());
        let (rx, tx) = waiting_call();
        dj.ask_approval(
            "Add 2 songs to \"Run\"".into(),
            Call::LikeTracks { uris: Vec::new() },
            tx,
        );
        assert_eq!(dj.approvals.len(), 1);
        let id = dj.approvals[0].id;
        // not yet late
        assert!(dj.expire_approvals().is_empty());
        assert!(rx.try_recv().is_err());
        dj.approvals[0].asked = Instant::now() - APPROVAL_WAIT - Duration::from_secs(1);
        let expired = dj.expire_approvals();
        assert_eq!(expired.len(), 1);
        assert!(
            rx.recv()
                .unwrap()
                .unwrap_err()
                .contains("nothing was changed")
        );
        assert!(dj.take_approval(id).is_none());
    }

    #[test]
    fn a_change_still_waiting_when_the_turn_ends_is_declined() {
        let (mut dj, _) = dj_with(
            vec![ChatEvent::Error {
                message: "stopped".into(),
                hint: None,
            }],
            ok_status(),
        );
        let (rx, tx) = waiting_call();
        dj.ask_approval("one".into(), Call::Skip, tx);
        dj.ask("x", "sonnet", "low");
        dj.poll();
        assert!(dj.approvals.is_empty());
        assert!(
            rx.recv()
                .unwrap()
                .unwrap_err()
                .contains("nothing was changed")
        );
    }

    #[test]
    fn stopping_the_dj_declines_every_waiting_change() {
        let (mut dj, _) = dj_with(Vec::new(), ok_status());
        let (rx1, tx1) = waiting_call();
        let (rx2, tx2) = waiting_call();
        dj.ask_approval("one".into(), Call::Skip, tx1);
        dj.ask_approval("two".into(), Call::Skip, tx2);
        let first = dj.approvals[0].id;
        let second = dj.approvals[1].id;
        assert_ne!(first, second);
        let taken = dj.take_approval(first).unwrap();
        assert_eq!(taken.summary, "one");
        dj.cancel();
        assert!(rx2.recv().unwrap().is_err());
        assert!(
            rx1.try_recv().is_err(),
            "a taken approval is the app's to answer"
        );
        assert!(dj.approvals.is_empty());
    }

    #[test]
    fn an_unprompted_turn_shows_no_message_and_a_quiet_answer_leaves_no_line() {
        let (mut dj, slot) = dj_with(
            vec![ChatEvent::Done {
                answer: "ok".into(),
            }],
            ok_status(),
        );
        dj.ask_unprompted("[events] queue low", "haiku", "low");
        dj.poll();
        assert!(dj.lines.is_empty());
        assert!(!dj.busy);
        let f = slot.lock().unwrap().clone().unwrap();
        assert_eq!(f.asked.lock().unwrap()[0].prompt, "[events] queue low");
    }

    #[test]
    fn the_djs_own_turn_after_a_chat_is_marked_so_it_never_joins_the_listeners_turn() {
        let (mut dj, slot) = dj_with(
            vec![
                ChatEvent::Tool {
                    text: "Searched for \"jazz\"".into(),
                    card: None,
                },
                ChatEvent::Done {
                    answer: "Here.".into(),
                },
            ],
            ok_status(),
        );
        dj.ask("something jazzy", "sonnet", "low");
        dj.poll();
        assert!(dj.lines.iter().all(|l| !l.auto));
        let f = slot.lock().unwrap().clone().unwrap();
        *f.script.lock().unwrap() = vec![
            ChatEvent::Tool {
                text: "Queued 2 songs".into(),
                card: None,
            },
            ChatEvent::Done {
                answer: "Topped up the queue.".into(),
            },
        ];
        dj.ask_unprompted("[events] queue low", "haiku", "low");
        dj.poll();
        let auto: Vec<&str> = dj
            .lines
            .iter()
            .filter(|l| l.auto)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(auto, ["Queued 2 songs", "Topped up the queue."]);
        let own: Vec<&str> = dj
            .lines
            .iter()
            .filter(|l| !l.auto && l.kind == LineKind::Tool)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(own, ["Searched for \"jazz\""]);
    }

    #[test]
    fn notes_go_along_once_and_again_when_they_change() {
        let (mut dj, slot) = dj_with(Vec::new(), ok_status());
        dj.memory.remember("no country").unwrap();
        dj.ask("one", "sonnet", "low");
        dj.busy = false;
        dj.ask("two", "sonnet", "low");
        dj.busy = false;
        dj.memory.remember("likes jazz").unwrap();
        dj.ask("three", "sonnet", "low");
        let f = slot.lock().unwrap().clone().unwrap();
        let asked = f.asked.lock().unwrap();
        assert!(asked[0].prompt.contains("no country") && asked[0].prompt.ends_with("one"));
        assert_eq!(asked[1].prompt, "two");
        assert!(asked[2].prompt.contains("likes jazz"));
    }

    #[test]
    fn after_shutdown_the_next_turn_is_a_new_chat_and_gets_the_notes() {
        let done = || ChatEvent::Done {
            answer: "Queued.".into(),
        };
        let (mut dj, slot) = dj_with(vec![done()], ok_status());
        dj.memory.remember("no country").unwrap();
        dj.ask("first", "sonnet", "low");
        dj.poll();
        dj.shutdown();
        let f = slot.lock().unwrap().clone().unwrap();
        f.script.lock().unwrap().push(done());
        dj.ask("second", "sonnet", "low");
        dj.poll();
        let titles: Vec<&str> = dj.chats().iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["second", "first"]);
        let asked = f.asked.lock().unwrap();
        assert!(asked[1].prompt.contains("no country") && asked[1].prompt.ends_with("second"));
    }

    #[test]
    fn a_finished_chat_is_saved_and_can_be_reopened() {
        let (mut dj, _) = dj_with(
            vec![ChatEvent::Done {
                answer: "Queued.".into(),
            }],
            ok_status(),
        );
        dj.ask("something for a run", "sonnet", "low");
        dj.poll();
        assert_eq!(dj.chats().len(), 1);
        assert_eq!(dj.chats()[0].title, "something for a run");
        let id = dj.chats()[0].id;
        dj.new_chat();
        assert!(dj.lines.is_empty());
        dj.open_chat(id);
        assert_eq!(dj.lines.len(), 2);
        assert_eq!(dj.lines[1].text, "Queued.");
        dj.delete_chat(id);
        assert!(dj.chats().is_empty());
        assert!(dj.lines.is_empty());
    }

    #[test]
    fn the_chat_is_bounded() {
        let (mut dj, _) = dj_with(Vec::new(), ok_status());
        for i in 0..(KEPT_LINES + 20) {
            dj.push(LineKind::Tool, format!("{i}"));
        }
        assert_eq!(dj.lines.len(), KEPT_LINES);
        assert_eq!(dj.lines[0].text, "20");
    }
}
