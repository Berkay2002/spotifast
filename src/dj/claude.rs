//! Claude as the DJ: the user's own installed `claude` (Claude Code), signed in with their own
//! Claude plan. Nothing of Anthropic's is read, copied, refreshed, stored or printed by Spotifast;
//! the login stays where `claude login` put it and only the CLI ever touches it.
//!
//! One long-lived `claude -p --input-format stream-json` child. Its own tools are off
//! (`--tools ""`) and it sees no user settings, plugins, skills or hooks; it gets Spotifast's tools
//! as an MCP server on 127.0.0.1 (`mcp.rs`, bearer token in a private config file). What the child
//! writes is mapped to [`ChatEvent`]s by [`Mapper`], a pure state machine the tests drive with
//! fixtures.
//!
//! Flags verified against Claude Code 2.1.296 (`claude --help`).

use super::mcp::{Handler, McpServer};
use super::proc::{CLOSED, Proc};
use super::tools::{self, Call, Tool, ToolReply};
use super::{
    ALLOWED_TOOLS, AskRequest, ChatEvent, EFFORTS, Provider, ProviderId, SERVER_NAME,
    SYSTEM_PROMPT, Sink, Status, exe, private,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// No event from the child for this long (while no tool call is in flight) and it is considered hung.
const IDLE_TIMEOUT: Duration = Duration::from_secs(180);
/// An interrupt the child has not answered by now is a kill.
const INTERRUPT_GRACE: Duration = Duration::from_secs(5);
/// The CLI's own limit for one MCP call. The app answers a call well within this (`session.rs`);
/// the longest wait is a call held for the listener's approval.
const MCP_TOOL_TIMEOUT_MS: &str = "300000";
/// Claude Code's own web search, the only built-in the DJ may be given. WebFetch stays off: a URL
/// the model builds can carry what it has read out of the library.
const WEB_TOOLS: &str = "WebSearch";

/// Runs a validated call against the app and waits for its answer. The flag is the turn's cancel:
/// a wait ends early when it is set.
pub type ToolHost = Arc<dyn Fn(Call, &AtomicBool) -> Result<ToolReply, String> + Send + Sync>;

/// A model id that may reach the command line: the empty string and "default" mean the CLI's own
/// choice (`Ok(None)`); anything else must be plain letters, digits and `. _ - [ ]` that does not
/// start with a dash. The id comes from a settings file, so it is never trusted to be a value
/// rather than an option (`--model --bare` would be both).
pub fn accepted_model(m: &str) -> Result<Option<String>, String> {
    let m = m.trim();
    if m.is_empty() || m == "default" {
        return Ok(None);
    }
    let plain = m.len() <= 100
        && m.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && m.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '[' | ']'));
    if plain {
        Ok(Some(m.to_string()))
    } else {
        Err("That model name is not one Claude Code offers. Pick a model in Settings.".into())
    }
}

/// Claude Code's conversation ids are UUIDs.
fn is_session_id(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(n, p)| p.len() == *n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The arguments of the long-lived child, after the program. `cfg` is the MCP config file.
pub fn build_args(
    cfg: &Path,
    model: Option<&str>,
    effort: Option<&str>,
    resume: Option<&str>,
    web: bool,
) -> Vec<String> {
    let mut a: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        // none of Claude Code's own tools but (when allowed) its web search: it can only do what
        // Spotifast's server offers, and look things up
        "--tools",
        if web { WEB_TOOLS } else { "" },
        "--strict-mcp-config",
        "--mcp-config",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    a.push(cfg.to_string_lossy().into_owned());
    for s in [
        "--allowedTools",
        if web {
            concat!("mcp__spotifast__*", ",", "WebSearch")
        } else {
            ALLOWED_TOOLS
        },
        // no user, project or local settings: no hooks, no plugins, no CLAUDE.md of the user's
        "--setting-sources",
        "",
        "--disable-slash-commands",
        // anything that would still prompt is a denial, never a wait nobody can answer
        "--permission-prompts",
        "none",
        "--system-prompt",
        SYSTEM_PROMPT,
    ] {
        a.push(s.to_string());
    }
    // `--flag=value`: a value can never be read as another option
    if let Some(m) = model.filter(|m| !m.is_empty() && *m != "default") {
        a.push(format!("--model={m}"));
    }
    if let Some(e) = effort.filter(|e| EFFORTS.contains(e)) {
        a.push(format!("--effort={e}"));
    }
    if let Some(r) = resume.filter(|r| is_session_id(r)) {
        a.push(format!("--resume={r}"));
    }
    a
}

/// The MCP config file's content: the loopback server and its bearer token.
fn mcp_config(url: &str, token: &str) -> Value {
    json!({ "mcpServers": { SERVER_NAME: {
        "type": "http",
        "url": url,
        "headers": { "Authorization": format!("Bearer {token}") },
    } } })
}

// ---- what the child writes -------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub message: String,
    pub hint: Option<&'static str>,
}

impl Failure {
    fn new(m: impl Into<String>) -> Self {
        Self {
            message: m.into(),
            hint: None,
        }
    }
    fn hinted(m: impl Into<String>, hint: &'static str) -> Self {
        Self {
            message: m.into(),
            hint: Some(hint),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Answer(String),
    Interrupted,
    Failed(Failure),
}

/// What one line of the child's output means.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Event(ChatEvent),
    /// The conversation id, for `--resume` after a restart.
    Session(String),
    /// The turn ended.
    Finished(Outcome),
    /// The child went away.
    Closed,
}

/// Turns the child's stream-json lines into steps. Holds only the text of the message being written.
#[derive(Default)]
pub struct Mapper {
    /// Text of the assistant message being written; a message that ends in a tool call is not the answer.
    raw: String,
    shown: usize,
    assistant_error: Option<String>,
    limited: bool,
}

impl Mapper {
    pub fn feed(&mut self, msg: &Value, cancelled: bool) -> Vec<Step> {
        let mut out = Vec::new();
        // output of a sub-agent is not this conversation's
        if !msg.get("parent_tool_use_id").is_none_or(Value::is_null) {
            return out;
        }
        match msg.get("type").and_then(Value::as_str).unwrap_or_default() {
            CLOSED => out.push(Step::Closed),
            "system" => {
                if msg.get("subtype").and_then(Value::as_str) == Some("init") {
                    if let Some(id) = msg.get("session_id").and_then(Value::as_str) {
                        out.push(Step::Session(id.to_string()));
                    }
                    // OAuth reports "none"; any other source means something else would be billed
                    if let Some(src) = msg.get("apiKeySource").and_then(Value::as_str)
                        && src != "none"
                        && src != "oauth"
                    {
                        out.push(Step::Finished(Outcome::Failed(Failure::hinted(
                                "Claude Code is set to use an API key, not your Claude plan. Spotifast only uses \
                                 your plan (`claude login`), so it stopped.",
                                "env",
                            ))));
                    }
                }
            }
            "stream_event" => {
                let ev = &msg["event"];
                match ev.get("type").and_then(Value::as_str) {
                    Some("message_start") => {
                        self.raw.clear();
                        self.shown = 0;
                    }
                    // a message that ends in a tool call was a remark ("let me look"), not the answer
                    Some("message_delta")
                        if ev.pointer("/delta/stop_reason").and_then(Value::as_str)
                            == Some("tool_use") =>
                    {
                        if self.shown > 0 {
                            out.push(Step::Event(ChatEvent::Reset));
                        }
                        self.raw.clear();
                        self.shown = 0;
                    }
                    Some("content_block_delta")
                        if ev.pointer("/delta/type").and_then(Value::as_str)
                            == Some("thinking_delta") =>
                    {
                        let t = ev
                            .pointer("/delta/thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if !t.is_empty() {
                            out.push(Step::Event(ChatEvent::Thinking {
                                text: t.to_string(),
                            }));
                        }
                    }
                    Some("content_block_delta")
                        if ev.pointer("/delta/type").and_then(Value::as_str)
                            == Some("text_delta") =>
                    {
                        let t = ev
                            .pointer("/delta/text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if !t.is_empty() {
                            self.raw.push_str(t);
                            self.shown = self.raw.len();
                            out.push(Step::Event(ChatEvent::Delta {
                                text: t.to_string(),
                            }));
                        }
                    }
                    _ => {}
                }
            }
            "assistant" => {
                if let Some(e) = msg.get("error").and_then(Value::as_str) {
                    self.assistant_error = Some(e.to_string());
                }
            }
            "rate_limit_event" => {
                self.limited = msg
                    .pointer("/rate_limit_info/status")
                    .and_then(Value::as_str)
                    == Some("rejected");
            }
            "result" => out.push(Step::Finished(finish(
                msg,
                self.assistant_error.as_deref(),
                self.limited,
                cancelled,
            ))),
            _ => {}
        }
        out
    }
}

/// The `result` event of a turn: its answer, or why there is none.
fn finish(r: &Value, assistant_error: Option<&str>, limited: bool, cancelled: bool) -> Outcome {
    let is_error = r.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    let subtype = r.get("subtype").and_then(Value::as_str).unwrap_or_default();
    let result = r.get("result").and_then(Value::as_str).unwrap_or_default();
    if cancelled && (is_error || subtype.starts_with("error")) {
        return Outcome::Interrupted;
    }
    if is_error || subtype.starts_with("error") {
        let lower = result.to_ascii_lowercase();
        if assistant_error == Some("authentication_failed")
            || lower.contains("not logged in")
            || lower.contains("/login")
        {
            return Outcome::Failed(Failure::hinted(
                "You are not signed in to Claude. Run `claude login` in a terminal, then try again.",
                "login",
            ));
        }
        if limited
            || matches!(assistant_error, Some("rate_limit"))
            || r.get("api_error_status").and_then(Value::as_i64) == Some(429)
            || lower.contains("usage limit")
            || lower.contains("rate limit")
        {
            let tail = if result.trim().is_empty() {
                String::new()
            } else {
                format!(" {}", result.trim())
            };
            return Outcome::Failed(Failure::hinted(
                format!("Your Claude usage limit is reached.{tail}"),
                "limit",
            ));
        }
        let message = if result.trim().is_empty() {
            "The request failed.".to_string()
        } else {
            result.trim().to_string()
        };
        return Outcome::Failed(Failure::new(message));
    }
    // a turn that only used tools may end with an empty text: the log already says what happened
    Outcome::Answer(result.trim().to_string())
}

// ---- the provider ----------------------------------------------------------------------------------

struct Running {
    proc: Arc<Proc>,
    mcp: McpServer,
    /// The private folder of this child: its working directory and its MCP config.
    dir: PathBuf,
    model: Option<String>,
    effort: Option<String>,
    web: bool,
}

impl Running {
    fn close(self) {
        self.mcp.stop();
        self.proc.kill();
        // the token file first; the folder is the child's working directory, which Windows keeps
        // until the process is really gone, so removing it is retried off this thread
        let _ = std::fs::remove_file(self.dir.join("mcp.json"));
        let dir = self.dir.clone();
        std::thread::spawn(move || {
            for _ in 0..60 {
                if std::fs::remove_dir_all(&dir).is_ok() || !dir.exists() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
    }
}

#[derive(Default)]
struct State {
    running: Option<Running>,
    /// Claude Code's id of the conversation, for `--resume` after the child went away.
    session_id: Option<String>,
}

/// How to start the CLI: the program, then these arguments, then Spotifast's.
#[derive(Debug, Clone)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
}

type FindCli = Box<dyn Fn(Option<&Path>) -> Option<Launch> + Send + Sync>;

pub struct ClaudeProvider {
    find: FindCli,
    environment: Box<dyn Fn() -> Vec<(String, String)> + Send + Sync>,
    tools: Arc<Vec<Tool>>,
    host: ToolHost,
    busy: AtomicBool,
    cancel: AtomicBool,
    closed: AtomicBool,
    /// Tool calls running: the idle timer pauses while there are any.
    pending: AtomicUsize,
    /// The sink of the turn in flight, for the activity log.
    sink: Mutex<Option<Sink>>,
    /// Serialises starting a child.
    start: Mutex<()>,
    state: Mutex<State>,
    /// The running child's output, taken by the turn that is streaming it.
    rx: Mutex<Option<Receiver<Value>>>,
    /// When an interrupt was sent for the turn in flight.
    interrupted: Mutex<Option<Instant>>,
    /// The user's own choice of executable; `None` = look for it.
    chosen: Mutex<Option<PathBuf>>,
}

impl ClaudeProvider {
    pub fn new(host: ToolHost) -> Arc<Self> {
        let find: FindCli = Box::new(|chosen| {
            exe::resolve(chosen).map(|program| Launch {
                program,
                args: Vec::new(),
            })
        });
        Arc::new(Self::build(host, find, Box::new(real_environment)))
    }

    fn build(
        host: ToolHost,
        find: FindCli,
        environment: Box<dyn Fn() -> Vec<(String, String)> + Send + Sync>,
    ) -> Self {
        Self {
            find,
            environment,
            tools: Arc::new(tools::registry()),
            host,
            busy: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            pending: AtomicUsize::new(0),
            sink: Mutex::new(None),
            start: Mutex::new(()),
            state: Mutex::new(State::default()),
            rx: Mutex::new(None),
            interrupted: Mutex::new(None),
            chosen: Mutex::new(None),
        }
    }

    fn launch(&self) -> Option<Launch> {
        let chosen = self.chosen.lock().unwrap().clone();
        (self.find)(chosen.as_deref())
    }

    /// The names of the variables in the user's environment that would make Claude Code use
    /// something other than their plan.
    fn forced(&self) -> Option<String> {
        let f = exe::forcing(&(self.environment)());
        (!f.is_empty()).then(|| exe::refusal(&f))
    }

    fn env_for_child(&self) -> Vec<(String, String)> {
        let mut env = exe::child_env(&(self.environment)());
        env.retain(|(k, _)| !k.eq_ignore_ascii_case("MCP_TOOL_TIMEOUT"));
        env.push(("MCP_TOOL_TIMEOUT".into(), MCP_TOOL_TIMEOUT_MS.into()));
        env
    }

    fn status_impl(&self) -> Status {
        let mut st = Status::default();
        let Some(l) = self.launch() else {
            st.error = Some("Claude Code is not installed.".into());
            return st;
        };
        st.installed = true;
        if let Some(why) = self.forced() {
            st.error = Some(why);
            return st;
        }
        let env = self.env_for_child();
        st.version = run_capture(&l, &["--version"], &env, Duration::from_secs(10))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if !st.version.as_deref().is_some_and(exe::is_claude_version) {
            st.installed = false;
            st.error = Some(
                "The program found as `claude` did not identify itself as Claude Code.".into(),
            );
            return st;
        }
        // JSON is its default output; it exits 1 when signed out and still prints `{"loggedIn": false}`
        match run_capture(&l, &["auth", "status"], &env, Duration::from_secs(15)) {
            Ok(out) => match serde_json::from_str::<Value>(out.trim()) {
                Ok(v) => {
                    st.logged_in = v.get("loggedIn").and_then(Value::as_bool).unwrap_or(false);
                    if st.logged_in {
                        st.account = v.get("email").and_then(Value::as_str).map(str::to_string);
                        st.plan = v
                            .get("subscriptionType")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                }
                Err(_) => st.error = Some("Claude Code's sign-in status could not be read.".into()),
            },
            Err(e) => st.error = Some(e),
        }
        st
    }

    /// The MCP handler: validates the call, runs it through the app, logs what it did.
    fn handler(self: &Arc<Self>) -> Handler {
        let weak = Arc::downgrade(self);
        Arc::new(move |name: &str, args: Value| {
            let Some(me) = weak.upgrade() else {
                return Err("Spotifast is closing.".to_string());
            };
            if !me.busy.load(Ordering::SeqCst) {
                return Err(
                    "No request is being answered right now, so nothing was run.".to_string(),
                );
            }
            let call = tools::parse(name, &args)?;
            me.pending.fetch_add(1, Ordering::SeqCst);
            let r = (me.host)(call, &me.cancel);
            me.pending.fetch_sub(1, Ordering::SeqCst);
            let reply = r?;
            if let (Some(log), Some(sink)) = (reply.log, me.sink.lock().unwrap().clone()) {
                sink(ChatEvent::Tool {
                    text: log,
                    card: reply.card,
                });
            }
            Ok(reply.text)
        })
    }

    /// The child, started (or restarted) when there is none, it died, or the model or effort
    /// changed. A restart continues the conversation with `--resume`.
    fn ensure_child(
        self: &Arc<Self>,
        model: Option<&str>,
        effort: Option<&str>,
        web: bool,
    ) -> Result<Arc<Proc>, Failure> {
        let _start = self.start.lock().unwrap();
        if self.closed.load(Ordering::SeqCst) {
            return Err(Failure::new("Spotifast is closing."));
        }
        // a shutdown or a stop that came while this waited for the lock: nothing is started
        if self.cancel.load(Ordering::SeqCst) {
            return Err(Failure::new("The listener stopped the DJ."));
        }
        let mut st = self.state.lock().unwrap();
        if let Some(r) = &st.running
            && r.proc.is_alive()
            && r.model.as_deref() == model
            && r.effort.as_deref() == effort
            && r.web == web
        {
            return Ok(r.proc.clone());
        }
        if let Some(old) = st.running.take() {
            old.close();
        }
        *self.rx.lock().unwrap() = None;
        let Some(l) = self.launch() else {
            return Err(Failure::hinted("Claude Code is not installed.", "install"));
        };
        if let Some(why) = self.forced() {
            return Err(Failure::hinted(why, "env"));
        }
        // folders an earlier run left when it was killed
        static TIDIED: std::sync::Once = std::sync::Once::new();
        TIDIED.call_once(|| {
            private::remove_stale(
                &std::env::temp_dir(),
                Duration::from_secs(24 * 3600),
                std::time::SystemTime::now(),
            );
        });
        // a new folder with a random name that only the owner can enter (see `private.rs`)
        let dir = private::private_dir_in(&std::env::temp_dir())
            .map_err(|e| Failure::new(format!("Could not prepare Claude: {e}")))?;
        let mcp = McpServer::start(self.tools.clone(), self.handler()).map_err(|e| {
            let _ = std::fs::remove_dir_all(&dir);
            Failure::new(format!("Could not start Spotifast's tool server: {e}"))
        })?;
        // the token goes to the child through this file, never through a command line
        let cfg = dir.join("mcp.json");
        let body = mcp_config(&mcp.url(), &mcp.token);
        if let Err(e) =
            private::write_new_private(&cfg, &serde_json::to_vec(&body).expect("serializable"))
        {
            mcp.stop();
            let _ = std::fs::remove_dir_all(&dir);
            return Err(Failure::new(format!("Could not prepare Claude: {e}")));
        }
        let mut args = l.args.clone();
        args.extend(build_args(
            &cfg,
            model,
            effort,
            st.session_id.as_deref(),
            web,
        ));
        match Proc::spawn(&l.program, &args, &self.env_for_child(), &dir) {
            Ok((proc, rx)) => {
                *self.rx.lock().unwrap() = Some(rx);
                st.running = Some(Running {
                    proc: proc.clone(),
                    mcp,
                    dir,
                    model: model.map(str::to_string),
                    effort: effort.map(str::to_string),
                    web,
                });
                Ok(proc)
            }
            Err(e) => {
                mcp.stop();
                let _ = std::fs::remove_dir_all(&dir);
                Err(Failure::new(e))
            }
        }
    }

    /// Forgets a child that is gone or no good; its conversation id is kept for `--resume`.
    fn drop_child(&self) {
        let old = self.state.lock().unwrap().running.take();
        if let Some(r) = old {
            r.close();
        }
        *self.rx.lock().unwrap() = None;
    }

    fn run_turn(self: &Arc<Self>, req: AskRequest, sink: &Sink) -> Result<ChatEvent, Failure> {
        if let Some(why) = self.forced() {
            return Err(Failure::hinted(why, "env"));
        }
        let model = accepted_model(req.model.as_deref().unwrap_or("")).map_err(Failure::new)?;
        let effort = req.effort.clone().filter(|e| EFFORTS.contains(&e.as_str()));
        sink(ChatEvent::Status {
            phase: "connecting",
        });
        let proc = match self.ensure_child(model.as_deref(), effort.as_deref(), req.web) {
            Ok(proc) => proc,
            Err(_) if self.cancel.load(Ordering::SeqCst) => return Ok(ChatEvent::Cancelled),
            Err(failure) => return Err(failure),
        };
        if self.cancel.load(Ordering::SeqCst) {
            return Ok(ChatEvent::Cancelled);
        }
        *self.interrupted.lock().unwrap() = None;
        let rx = self
            .rx
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| Failure::new("Claude Code is not running. Try again."))?;
        // anything left from an earlier turn is not part of this one
        while let Ok(m) = rx.try_recv() {
            if m.get("type").and_then(Value::as_str) == Some(CLOSED) {
                *self.rx.lock().unwrap() = Some(rx);
                self.drop_child();
                return Err(Failure::new(
                    "Claude Code stopped unexpectedly. Try again to restart it.",
                ));
            }
        }
        let sent = proc.send(&json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": req.prompt }] },
        }));
        sink(ChatEvent::Status { phase: "thinking" });
        let outcome = if sent {
            self.stream(&rx, &proc, sink)
        } else {
            Outcome::Failed(Failure::hinted(
                "Claude Code stopped unexpectedly. Try again to restart it.",
                "dead",
            ))
        };
        *self.rx.lock().unwrap() = Some(rx);
        match outcome {
            Outcome::Interrupted => Ok(ChatEvent::Cancelled),
            Outcome::Answer(answer) => Ok(ChatEvent::Done { answer }),
            Outcome::Failed(f) => {
                // after these the child may still be writing the turn we gave up on
                if matches!(f.hint, Some("dead" | "login" | "env")) {
                    self.drop_child();
                }
                Err(f)
            }
        }
    }

    fn stream(&self, rx: &Receiver<Value>, proc: &Arc<Proc>, sink: &Sink) -> Outcome {
        let cancelled = || self.cancel.load(Ordering::SeqCst);
        let mut mapper = Mapper::default();
        let mut last_event = Instant::now();
        loop {
            if cancelled() {
                let mut at = self.interrupted.lock().unwrap();
                let sent_at = *at;
                if sent_at.is_none() {
                    proc.send(&interrupt_request());
                    *at = Some(Instant::now());
                }
                drop(at);
                // an interrupt the child does not answer is a kill; the next request starts it again
                if sent_at.is_some_and(|t| t.elapsed() > INTERRUPT_GRACE) {
                    self.drop_child();
                    return Outcome::Interrupted;
                }
            }
            let msg = match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(m) => m,
                Err(RecvTimeoutError::Timeout) => {
                    // a call waiting for the app is not silence
                    if self.pending.load(Ordering::SeqCst) > 0 {
                        last_event = Instant::now();
                    } else if last_event.elapsed() > IDLE_TIMEOUT {
                        return Outcome::Failed(Failure::hinted(
                            "Claude stopped responding.",
                            "dead",
                        ));
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return if cancelled() {
                        Outcome::Interrupted
                    } else {
                        Outcome::Failed(Failure::hinted(
                            "Claude Code stopped unexpectedly. Try again to restart it.",
                            "dead",
                        ))
                    };
                }
            };
            last_event = Instant::now();
            for step in mapper.feed(&msg, cancelled()) {
                match step {
                    Step::Event(e) => sink(e),
                    Step::Session(id) => self.state.lock().unwrap().session_id = Some(id),
                    Step::Finished(o) => return o,
                    Step::Closed => {
                        if cancelled() {
                            return Outcome::Interrupted;
                        }
                        let tail = proc.stderr_tail();
                        let tail = tail
                            .lines()
                            .rev()
                            .find(|l| !l.trim().is_empty())
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        let message = if tail.is_empty() {
                            "Claude Code stopped unexpectedly. Try again to restart it.".to_string()
                        } else {
                            format!("Claude Code stopped: {tail}")
                        };
                        return Outcome::Failed(Failure::hinted(message, "dead"));
                    }
                }
            }
        }
    }
}

fn interrupt_request() -> Value {
    json!({ "type": "control_request", "request_id": "spotifast-interrupt", "request": { "subtype": "interrupt" } })
}

/// The user's environment as the process has it (a variable that is not valid Unicode is read lossily).
fn real_environment() -> Vec<(String, String)> {
    std::env::vars_os()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

/// Runs `claude <extra>` to the end (at most `limit`) and returns its stdout. The exit code is not an
/// error: `claude auth status` exits 1 when signed out and prints its answer all the same.
fn run_capture(
    l: &Launch,
    extra: &[&str],
    env: &[(String, String)],
    limit: Duration,
) -> Result<String, String> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(&l.program);
    cmd.args(&l.args)
        .args(extra)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", l.program.display()))?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut out = Vec::new();
        let _ = (&mut stdout).take(1 << 20).read_to_end(&mut out);
        let _ = tx.send(out);
    });
    match rx.recv_timeout(limit) {
        Ok(out) => {
            let _ = child.wait();
            Ok(String::from_utf8_lossy(&out).into_owned())
        }
        Err(_) => {
            // a program that does not answer in time is stopped, not left running
            let _ = child.kill();
            let _ = child.wait();
            Err("Claude Code did not answer in time.".into())
        }
    }
}

impl Provider for ClaudeProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Claude
    }

    fn status(&self) -> Status {
        self.status_impl()
    }

    fn ask(self: Arc<Self>, req: AskRequest, sink: Sink) -> Result<(), String> {
        if self.closed.load(Ordering::SeqCst) {
            return Err("Spotifast is closing.".into());
        }
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err("The DJ is still answering the previous message.".into());
        }
        self.cancel.store(false, Ordering::SeqCst);
        *self.sink.lock().unwrap() = Some(sink.clone());
        let me = self.clone();
        std::thread::Builder::new()
            .name("dj-claude-turn".into())
            .spawn(move || {
                let ev = match me.run_turn(req, &sink) {
                    Ok(ev) => ev,
                    Err(f) => ChatEvent::Error {
                        message: f.message,
                        hint: f.hint.map(str::to_string),
                    },
                };
                // the busy flag goes first: the next ask may come from the very event below
                *me.sink.lock().unwrap() = None;
                me.busy.store(false, Ordering::SeqCst);
                sink(ev);
            })
            .map_err(|e| {
                self.busy.store(false, Ordering::SeqCst);
                e.to_string()
            })?;
        Ok(())
    }

    fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        if self.busy.load(Ordering::SeqCst) {
            let proc = self
                .state
                .lock()
                .unwrap()
                .running
                .as_ref()
                .map(|r| r.proc.clone());
            if let Some(p) = proc {
                let mut at = self.interrupted.lock().unwrap();
                if at.is_none() {
                    p.send(&interrupt_request());
                    *at = Some(Instant::now());
                }
            }
        }
    }

    fn session(&self) -> Option<String> {
        self.state.lock().unwrap().session_id.clone()
    }

    fn set_session(&self, id: Option<String>) {
        let id = id.filter(|id| is_session_id(id));
        let changed = self.state.lock().unwrap().session_id != id;
        if changed && !self.busy.load(Ordering::SeqCst) {
            // the running child holds the old conversation: the next turn starts one that resumes
            self.drop_child();
            self.state.lock().unwrap().session_id = id;
        }
    }

    fn shutdown(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        self.drop_child();
        // a later ask starts a fresh child (sign-out then sign-in is the same app run)
        self.state.lock().unwrap().session_id = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<Value> {
        s.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn run(fixture: &str) -> Vec<Step> {
        let mut m = Mapper::default();
        lines(fixture)
            .iter()
            .flat_map(|v| m.feed(v, false))
            .collect()
    }

    const UUID: &str = "0b7f5d1e-3c4a-4e8f-9a21-5d6e7f8a9b0c";

    #[test]
    fn the_command_line_is_closed_to_the_cli_own_tools() {
        let a = build_args(
            Path::new("C:\\tmp\\mcp.json"),
            Some("sonnet"),
            Some("low"),
            Some(UUID),
            false,
        );
        let at = |flag: &str| {
            a.iter()
                .position(|x| x == flag)
                .unwrap_or_else(|| panic!("{flag} missing in {a:?}"))
        };
        assert_eq!(a[at("--tools") + 1], "");
        assert_eq!(a[at("--setting-sources") + 1], "");
        assert_eq!(a[at("--permission-prompts") + 1], "none");
        assert_eq!(a[at("--allowedTools") + 1], "mcp__spotifast__*");
        assert_eq!(a[at("--mcp-config") + 1], "C:\\tmp\\mcp.json");
        assert!(a.contains(&"--strict-mcp-config".to_string()));
        assert!(a.contains(&"--disable-slash-commands".to_string()));
        assert!(a.contains(&"--model=sonnet".to_string()));
        assert!(a.contains(&"--effort=low".to_string()));
        assert!(a.contains(&format!("--resume={UUID}")));
        // the token is in the config file, never on the command line
        assert!(!a.iter().any(|x| x.to_ascii_lowercase().contains("bearer")));
    }

    #[test]
    fn web_search_is_the_only_built_in_tool_and_only_when_allowed() {
        let at = |a: &[String], flag: &str| a.iter().position(|x| x == flag).unwrap();
        let off = build_args(Path::new("c.json"), None, None, None, false);
        assert_eq!(off[at(&off, "--tools") + 1], "");
        assert_eq!(off[at(&off, "--allowedTools") + 1], "mcp__spotifast__*");
        let on = build_args(Path::new("c.json"), None, None, None, true);
        assert_eq!(on[at(&on, "--tools") + 1], "WebSearch");
        assert_eq!(
            on[at(&on, "--allowedTools") + 1],
            "mcp__spotifast__*,WebSearch"
        );
        assert!(
            !on.iter()
                .any(|x| x.contains("Bash") || x.contains("Edit") || x.contains("WebFetch"))
        );
    }

    #[test]
    fn values_that_could_be_options_never_reach_the_command_line() {
        let a = build_args(
            Path::new("c.json"),
            Some("--bare"),
            Some("--bare"),
            Some("--dangerously-skip-permissions"),
            false,
        );
        // `--model=--bare` is one argument: a value, not an option
        assert!(
            !a.iter()
                .any(|x| x == "--bare" || x == "--dangerously-skip-permissions")
        );
        assert!(
            !a.iter()
                .any(|x| x.starts_with("--effort") || x.starts_with("--resume"))
        );
        let a = build_args(
            Path::new("c.json"),
            Some("default"),
            Some("ultra"),
            Some("not-a-uuid"),
            false,
        );
        assert!(!a.iter().any(|x| x.starts_with("--model")
            || x.starts_with("--effort")
            || x.starts_with("--resume")));
    }

    #[test]
    fn model_names_are_vetted() {
        assert_eq!(accepted_model(""), Ok(None));
        assert_eq!(accepted_model("default"), Ok(None));
        assert_eq!(accepted_model("sonnet"), Ok(Some("sonnet".into())));
        assert_eq!(
            accepted_model("claude-sonnet-5-5[1m]"),
            Ok(Some("claude-sonnet-5-5[1m]".into()))
        );
        assert!(accepted_model("--bare").is_err());
        assert!(accepted_model("a b").is_err());
        assert!(accepted_model("a;b").is_err());
        assert!(accepted_model(&"x".repeat(101)).is_err());
    }

    #[test]
    fn the_config_carries_the_url_and_the_token() {
        let v = mcp_config("http://127.0.0.1:5555/mcp", "tok");
        let s = &v["mcpServers"]["spotifast"];
        assert_eq!(s["type"], "http");
        assert_eq!(s["url"], "http://127.0.0.1:5555/mcp");
        assert_eq!(s["headers"]["Authorization"], "Bearer tok");
    }

    #[test]
    fn a_reply_streams_as_deltas_and_ends_with_the_answer() {
        let steps = run(r#"
{"type":"system","subtype":"init","session_id":"0b7f5d1e-3c4a-4e8f-9a21-5d6e7f8a9b0c","apiKeySource":"none"}
{"type":"stream_event","event":{"type":"message_start"}}
{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Queued "}}}
{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"eight."}}}
{"type":"assistant","message":{}}
{"type":"result","subtype":"success","is_error":false,"result":"Queued eight."}
"#);
        assert_eq!(
            steps,
            vec![
                Step::Session(UUID.into()),
                Step::Event(ChatEvent::Delta {
                    text: "Queued ".into()
                }),
                Step::Event(ChatEvent::Delta {
                    text: "eight.".into()
                }),
                Step::Finished(Outcome::Answer("Queued eight.".into())),
            ]
        );
    }

    #[test]
    fn a_remark_before_a_tool_call_is_reset() {
        let steps = run(r#"
{"type":"stream_event","event":{"type":"message_start"}}
{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Let me look."}}}
{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"tool_use"}}}
{"type":"stream_event","event":{"type":"message_start"}}
{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"tool_use"}}}
"#);
        // the second tool_use ended an empty message: nothing to take back
        assert_eq!(
            steps,
            vec![
                Step::Event(ChatEvent::Delta {
                    text: "Let me look.".into()
                }),
                Step::Event(ChatEvent::Reset)
            ]
        );
    }

    #[test]
    fn a_sub_agents_output_is_not_this_conversations() {
        let steps = run(r#"
{"type":"stream_event","parent_tool_use_id":"toolu_1","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"hidden"}}}
{"type":"result","parent_tool_use_id":"toolu_1","subtype":"success","is_error":false,"result":"hidden"}
"#);
        assert!(steps.is_empty());
    }

    #[test]
    fn an_api_key_source_other_than_the_plan_stops_the_turn() {
        for src in ["ANTHROPIC_API_KEY", "apiKeyHelper"] {
            let steps = run(&format!(
                r#"{{"type":"system","subtype":"init","session_id":"{UUID}","apiKeySource":"{src}"}}"#
            ));
            assert!(
                matches!(steps.last(), Some(Step::Finished(Outcome::Failed(f))) if f.hint == Some("env")),
                "{src}: {steps:?}"
            );
        }
        for src in ["none", "oauth"] {
            let steps = run(&format!(
                r#"{{"type":"system","subtype":"init","session_id":"{UUID}","apiKeySource":"{src}"}}"#
            ));
            assert_eq!(steps, vec![Step::Session(UUID.into())]);
        }
    }

    #[test]
    fn failures_are_told_apart() {
        let login = run(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"Not logged in · Please run /login"}"#,
        );
        assert!(matches!(&login[0], Step::Finished(Outcome::Failed(f)) if f.hint == Some("login")));
        let limit = run(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected"}}
{"type":"result","subtype":"error","is_error":true,"result":"5-hour limit reached"}"#,
        );
        assert!(matches!(&limit[0], Step::Finished(Outcome::Failed(f)) if f.hint == Some("limit")));
        let other = run(r#"{"type":"result","subtype":"error","is_error":true,"result":"boom"}"#);
        assert_eq!(
            other,
            vec![Step::Finished(Outcome::Failed(Failure::new("boom")))]
        );
        // an interrupted turn that ends in an error result is a cancel, not a failure
        let mut m = Mapper::default();
        let v: Value = serde_json::from_str(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"x"}"#,
        )
        .unwrap();
        assert_eq!(m.feed(&v, true), vec![Step::Finished(Outcome::Interrupted)]);
    }

    #[test]
    fn the_closing_marker_is_a_step() {
        let steps = run(r#"{"type":"__closed__"}"#);
        assert_eq!(steps, vec![Step::Closed]);
    }

    #[test]
    fn a_turn_of_tools_only_ends_with_an_empty_answer() {
        let steps = run(r#"{"type":"result","subtype":"success","is_error":false,"result":""}"#);
        assert_eq!(steps, vec![Step::Finished(Outcome::Answer(String::new()))]);
    }

    #[test]
    fn session_ids_are_uuids() {
        assert!(is_session_id(UUID));
        assert!(!is_session_id("--resume"));
        assert!(!is_session_id("0b7f5d1e-3c4a-4e8f-9a21"));
    }
}
