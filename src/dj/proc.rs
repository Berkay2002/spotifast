//! One `claude` child process speaking stream-json on its stdio.
//!
//! Every line the child writes that parses as a JSON object arrives on the receiver; when the
//! process goes away a last `{"type":"__closed__"}` arrives and the channel ends. One writer thread
//! owns stdin (a slow pipe never blocks the caller), one thread drains stderr (a full pipe stalls the
//! child) and keeps its tail for error messages. The child lives in its own Job object with
//! kill-on-close, so it and anything it started die with Spotifast however Spotifast dies.

use serde_json::Value;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const CLOSED: &str = "__closed__";

/// The longest line the child may write (a page picture inside a tool result is a few MB of base64
/// at most). A longer one ends the child instead of growing a buffer without bound.
const MAX_LINE: usize = 16 * 1024 * 1024;

pub struct Proc {
    outbox: Sender<Vec<u8>>,
    child: Mutex<Child>,
    job: Option<job::Job>,
    alive: Arc<AtomicBool>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

impl Proc {
    /// Starts `program args` in `cwd` with exactly `env` (nothing is inherited).
    pub fn spawn(
        program: &Path,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Result<(Arc<Proc>, Receiver<Value>), String> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("could not start {}: {e}", program.display()))?;
        // fail closed: a child that cannot be tied to Spotifast's lifetime does not run
        let job = job::Job::new();
        if !job.as_ref().is_some_and(|j| j.adopt(&child)) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "could not confine {} to Spotifast's process group",
                program.display()
            ));
        }
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let stdin = child.stdin.take().expect("piped");
        let (outbox, lines) = mpsc::channel::<Vec<u8>>();
        let alive = Arc::new(AtomicBool::new(true));
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let p = Arc::new(Proc {
            outbox,
            child: Mutex::new(child),
            job,
            alive: alive.clone(),
            stderr_tail: tail.clone(),
        });
        let (tx, rx) = mpsc::channel::<Value>();
        let weak = Arc::downgrade(&p);
        std::thread::Builder::new()
            .name("claude-out".into())
            .spawn(move || {
                let mut r = BufReader::new(stdout);
                let mut buf = Vec::new();
                loop {
                    buf.clear();
                    match (&mut r)
                        .take(MAX_LINE as u64 + 1)
                        .read_until(b'\n', &mut buf)
                    {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    if buf.len() > MAX_LINE {
                        if let Some(p) = weak.upgrade() {
                            p.note_stderr("Claude Code wrote a line over 16 MB; it was stopped.");
                            p.kill();
                        }
                        break;
                    }
                    let line = String::from_utf8_lossy(&buf);
                    let line = line.trim();
                    if !line.starts_with('{') {
                        continue;
                    }
                    if let Ok(v) = serde_json::from_str::<Value>(line)
                        && tx.send(v).is_err()
                    {
                        break;
                    }
                }
                alive.store(false, Ordering::SeqCst);
                let _ = tx.send(serde_json::json!({ "type": CLOSED }));
                // reap, without holding the child lock across a blocking wait (`kill` needs it)
                if let Some(p) = weak.upgrade() {
                    for _ in 0..50 {
                        let running = matches!(p.child.lock().unwrap().try_wait(), Ok(None));
                        if !running {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    p.kill();
                    let _ = p.child.lock().unwrap().wait();
                }
            })
            .map_err(|e| e.to_string())?;
        std::thread::Builder::new()
            .name("claude-err".into())
            .spawn(move || {
                for line in BufReader::new(stderr).split(b'\n').map_while(Result::ok) {
                    let mut t = tail.lock().unwrap();
                    t.push_back(String::from_utf8_lossy(&line).trim_end().to_string());
                    if t.len() > 40 {
                        t.pop_front();
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let weak = Arc::downgrade(&p);
        std::thread::Builder::new()
            .name("claude-in".into())
            .spawn(move || write_loop(stdin, lines, weak))
            .map_err(|e| e.to_string())?;
        Ok((p, rx))
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Queues one JSON line for the child. False when the child is gone.
    pub fn send(&self, msg: &Value) -> bool {
        if !self.is_alive() {
            return false;
        }
        let mut line = serde_json::to_vec(msg).expect("serializable");
        line.push(b'\n');
        self.outbox.send(line).is_ok()
    }

    /// Ends the child and everything it started.
    pub fn kill(&self) {
        if let Some(j) = &self.job {
            j.terminate();
        }
        let _ = self.child.lock().unwrap().kill();
    }

    fn note_stderr(&self, line: &str) {
        let mut t = self.stderr_tail.lock().unwrap();
        t.push_back(line.to_string());
        if t.len() > 40 {
            t.pop_front();
        }
    }

    /// Last lines the child wrote to stderr.
    pub fn stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn write_loop(mut stdin: ChildStdin, lines: Receiver<Vec<u8>>, p: std::sync::Weak<Proc>) {
    for line in lines {
        if stdin.write_all(&line).and_then(|_| stdin.flush()).is_err() {
            if let Some(p) = p.upgrade() {
                p.kill();
            }
            return;
        }
    }
}

/// A Windows Job object with kill-on-close: the child and anything it started die with Spotifast
/// however Spotifast dies.
#[cfg(windows)]
mod job {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };

    /// The handle as an integer, so the type is Send + Sync.
    pub struct Job(usize);

    impl Job {
        pub fn new() -> Option<Job> {
            // SAFETY: plain Win32 calls on a handle this function creates and owns.
            unsafe {
                let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if h.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    h,
                    JobObjectExtendedLimitInformation,
                    (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    CloseHandle(h);
                    return None;
                }
                Some(Job(h as usize))
            }
        }

        pub fn adopt(&self, child: &std::process::Child) -> bool {
            // SAFETY: both handles are live for the call.
            unsafe {
                AssignProcessToJobObject(self.0 as HANDLE, child.as_raw_handle() as HANDLE) != 0
            }
        }

        pub fn terminate(&self) {
            // SAFETY: the handle is owned by this value.
            unsafe {
                TerminateJobObject(self.0 as HANDLE, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the handle is owned by this value and closed once.
            unsafe {
                CloseHandle(self.0 as HANDLE);
            }
        }
    }
}

/// Elsewhere the child is ended by `kill`; there is no job to confine it to.
#[cfg(not(windows))]
mod job {
    pub struct Job;
    impl Job {
        pub fn new() -> Option<Job> {
            Some(Job)
        }
        pub fn adopt(&self, _: &std::process::Child) -> bool {
            true
        }
        pub fn terminate(&self) {}
    }
}
