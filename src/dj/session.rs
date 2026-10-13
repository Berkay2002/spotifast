//! What the app holds for the DJ: the chat as lines, the provider, and the two channels that cross
//! threads.
//!
//! * Events (text, tool log lines, the end of a turn) come from the turn's thread. [`Dj::poll`]
//!   applies them on the UI thread, so the page only ever reads plain data.
//! * Tool calls come from the MCP server's request threads. Each waits for an answer from the UI
//!   thread (`crate::app::dj` runs it against the app and replies); [`Dj::take_calls`] hands them
//!   over. Both directions wake the window through the shell's [`Waker`], which also works when the
//!   window is hidden in the tray.

use super::agent::{self, Chats, Feed, Memory, Schedules};
use super::claude::{ClaudeProvider, ToolHost};
use super::tools::{Call, ToolReply};
use super::{AskRequest, ChatEvent, Provider, Sink, Status};
use fastframe_shell::Waker;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, atomic::Ordering};
use std::time::{Duration, Instant};

/// How long a tool call waits for the app before it is told the app did not answer. The CLI's own
/// limit for a call is longer (`claude.rs`).
const TOOL_WAIT: Duration = Duration::from_secs(30);
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
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub text: String,
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
    /// Changes waiting for the listener's yes or no, oldest first.
    pub approvals: Vec<Approval>,
    next_approval: u64,
    /// What the DJ remembers, its scheduled tasks and the events that wake it.
    pub memory: Memory,
    pub schedules: Schedules,
    pub feed: Feed,
    chats: Chats,
    chat_id: u64,
    /// The turn in flight is the DJ's own (an event or a schedule), not an answer to a message.
    background: bool,
    /// The notes block the conversation was last given.
    notes_sent: Option<String>,
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
            approvals: Vec::new(),
            next_approval: 0,
            memory: Memory::default(),
            schedules: Schedules::default(),
            feed: Feed::default(),
            chats: Chats::default(),
            chat_id: 1,
            background: false,
            notes_sent: None,
        }
    }

    /// Loads what the DJ kept from `dir`: its notes, its schedules and the saved chats. The
    /// newest chat is not reopened: every start is a new conversation.
    pub fn attach(&mut self, dir: &Path) {
        self.memory = Memory::open(dir.join("dj-memory.json"));
        self.schedules = Schedules::open(dir.join("dj-schedules.json"));
        self.chats = Chats::open(dir.join("dj-chats.json"));
        self.chat_id = self.chats.next_id();
    }

    /// The saved chats, newest first.
    pub fn chats(&self) -> &[agent::Chat] {
        self.chats.all()
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
        self.provider.set_session(chat.session);
        self.notes_sent = None;
        self.revision += 1;
    }

    pub fn delete_chat(&mut self, id: u64) {
        self.chats.delete(id);
        if id == self.chat_id && !self.busy {
            self.new_chat();
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
        }
        self.background = !from_listener;
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
        self.decline_all("The listener stopped the DJ.");
        self.provider.cancel();
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
        self.calls.try_iter().collect()
    }

    fn push(&mut self, kind: LineKind, text: String) {
        self.lines.push(Line { kind, text });
        if self.lines.len() > KEPT_LINES {
            let extra = self.lines.len() - KEPT_LINES;
            self.lines.drain(..extra);
        }
        self.revision += 1;
    }

    fn apply(&mut self, ev: ChatEvent) {
        match ev {
            ChatEvent::Status { phase } => self.phase = Some(phase),
            ChatEvent::Delta { text } => {
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
            ChatEvent::Tool { text } => {
                self.live = false;
                self.push(LineKind::Tool, text);
            }
            ChatEvent::Done { answer } => {
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
                self.live = false;
                self.busy = false;
                self.phase = None;
                self.push(LineKind::Tool, "Stopped.".to_string());
                self.save_chat();
            }
            ChatEvent::Error { message, hint } => {
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
                self.push(LineKind::Error, message);
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

/// The page's explanation when the provider cannot be used, `None` when it can.
pub fn blocker(st: &Status) -> Option<String> {
    if !st.installed {
        return Some(
            "Claude Code isn't installed. The DJ uses your Claude subscription through the Claude Code \
             CLI. Install it from claude.com/claude-code, then run `claude login` in a terminal."
                .to_string(),
        );
    }
    if let Some(e) = &st.error {
        return Some(e.clone());
    }
    if !st.logged_in {
        return Some(
            "Sign in to Claude Code. Run `claude login` in a terminal and sign in with your Claude \
             account. Spotifast never sees your login."
                .to_string(),
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
                },
                ChatEvent::Tool {
                    text: "Queued 8 songs".into(),
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
                log: None,
            }))
            .unwrap();
        assert_eq!(t.join().unwrap().unwrap().text, "skipped");
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
            blocker(dj.status.as_ref().unwrap())
                .unwrap()
                .contains("isn't installed")
        );
    }

    #[test]
    fn blockers_say_what_to_do() {
        assert!(blocker(&ok_status()).is_none());
        let out = Status {
            installed: true,
            logged_in: false,
            ..Status::default()
        };
        assert!(blocker(&out).unwrap().contains("claude login"));
        let env = Status {
            installed: true,
            error: Some("env refuses".into()),
            ..Status::default()
        };
        assert_eq!(blocker(&env).unwrap(), "env refuses");
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
