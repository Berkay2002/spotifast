//! What the DJ keeps between conversations and does without being asked: notes about the
//! listener, scheduled tasks, the feed of app events that wakes it, and the saved chats.
//!
//! All of it is plain data. Files are written atomically and read back leniently: a damaged file
//! is an empty one, never an error the listener has to fix.

use super::session::{Line, LineKind};
use super::tools::{Cadence, Slot};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const NOTES_MAX: usize = 40;
/// The notes handed to the model in one turn stay under this many characters.
const BLOCK_MAX: usize = 2000;
pub const SCHEDULES_MAX: usize = 10;
/// A task that missed its minute (the app was closed) still runs if it is at most this late.
const CATCH_UP_MINUTES: u16 = 120;
/// At most one turn of the DJ's own per this long.
pub const EVENT_GAP: Duration = Duration::from_secs(60);
/// More than this many events in one turn are dropped, oldest first.
const EVENTS_MAX: usize = 8;
pub const CHATS_MAX: usize = 30;
pub const CHAT_LINES_MAX: usize = 200;

fn save<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(&temporary, path)
}

fn load<T: DeserializeOwned + Default>(path: &Path) -> T {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// What the DJ was asked to remember about the listener ("no country music on Sundays").
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Memory {
    notes: Vec<String>,
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl Memory {
    pub fn open(path: PathBuf) -> Self {
        let mut memory: Memory = load(&path);
        memory.path = Some(path);
        memory
    }

    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    fn persist(&self) {
        if let Some(path) = &self.path {
            let _ = save(path, self);
        }
    }

    pub fn remember(&mut self, note: &str) -> Result<(), String> {
        let note = note.split_whitespace().collect::<Vec<_>>().join(" ");
        if note.is_empty() {
            return Err("There is nothing to remember in that note.".into());
        }
        if self.notes.iter().any(|n| n.eq_ignore_ascii_case(&note)) {
            return Ok(());
        }
        if self.notes.len() >= NOTES_MAX {
            return Err(format!(
                "Memory is full ({NOTES_MAX} notes). Forget one before adding another."
            ));
        }
        self.notes.push(note);
        self.persist();
        Ok(())
    }

    /// Drops every note that contains `needle` (case does not matter); returns how many.
    pub fn forget(&mut self, needle: &str) -> usize {
        let needle = needle.trim().to_lowercase();
        if needle.is_empty() {
            return 0;
        }
        let before = self.notes.len();
        self.notes.retain(|n| !n.to_lowercase().contains(&needle));
        let gone = before - self.notes.len();
        if gone > 0 {
            self.persist();
        }
        gone
    }

    /// The notes as text to put in front of a turn, newest last, `None` when there are none.
    pub fn block(&self) -> Option<String> {
        if self.notes.is_empty() {
            return None;
        }
        let mut out = String::from("What you remember about the listener:\n");
        for note in &self.notes {
            let line = format!("- {note}\n");
            if out.len() + line.len() > BLOCK_MAX {
                break;
            }
            out.push_str(&line);
        }
        Some(out)
    }
}

/// One scheduled task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: u32,
    pub slot: Slot,
    pub prompt: String,
    /// The local date (`YYYY-MM-DD`) it last ran.
    #[serde(default)]
    pub last_run: Option<String>,
}

/// The listener's local time, as the schedule reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moment {
    /// `YYYY-MM-DD`
    pub date: String,
    /// Minutes after local midnight.
    pub minute: u16,
    /// 0 = Monday.
    pub weekday: u8,
}

impl Moment {
    pub fn now() -> Self {
        let now = jiff::Zoned::now();
        Self {
            date: now.strftime("%Y-%m-%d").to_string(),
            minute: now.hour() as u16 * 60 + now.minute() as u16,
            weekday: now.weekday().to_monday_zero_offset() as u8,
        }
    }
}

/// Tasks the DJ runs at set times while Spotifast is open.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Schedules {
    tasks: Vec<Task>,
    next_id: u32,
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl Schedules {
    pub fn open(path: PathBuf) -> Self {
        let mut schedules: Schedules = load(&path);
        schedules.path = Some(path);
        schedules
    }

    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    fn persist(&self) {
        if let Some(path) = &self.path {
            let _ = save(path, self);
        }
    }

    pub fn add(&mut self, slot: Slot, prompt: &str) -> Result<u32, String> {
        if self.tasks.len() >= SCHEDULES_MAX {
            return Err(format!(
                "There are already {SCHEDULES_MAX} scheduled tasks. Cancel one first."
            ));
        }
        self.next_id = self
            .next_id
            .max(self.tasks.iter().map(|t| t.id).max().unwrap_or(0))
            + 1;
        let id = self.next_id;
        self.tasks.push(Task {
            id,
            slot,
            prompt: prompt.trim().to_string(),
            last_run: None,
        });
        self.persist();
        Ok(id)
    }

    pub fn cancel(&mut self, id: u32) -> bool {
        let before = self.tasks.len();
        self.tasks.retain(|t| t.id != id);
        let gone = self.tasks.len() != before;
        if gone {
            self.persist();
        }
        gone
    }

    /// The tasks that are due at `now`, marked as run. A one-time task is dropped once it ran
    /// or when its day is past.
    pub fn due(&mut self, now: &Moment) -> Vec<Task> {
        let mut out = Vec::new();
        let mut changed = false;
        self.tasks.retain_mut(|task| {
            let day_matches = match task.slot.cadence {
                Cadence::Once => match task.slot.date.as_deref() {
                    Some(date) if date < now.date.as_str() => {
                        changed = true;
                        return false;
                    }
                    Some(date) => date == now.date,
                    None => false,
                },
                Cadence::Daily => true,
                Cadence::Weekdays => now.weekday < 5,
                Cadence::Weekly => task.slot.weekday == Some(now.weekday),
            };
            let late = now.minute.checked_sub(task.slot.minute);
            let in_time = late.is_some_and(|late| late <= CATCH_UP_MINUTES);
            let fresh = task.last_run.as_deref() != Some(now.date.as_str());
            if day_matches && in_time && fresh {
                task.last_run = Some(now.date.clone());
                out.push(task.clone());
                changed = true;
                return task.slot.cadence != Cadence::Once;
            }
            true
        });
        if changed {
            self.persist();
        }
        out
    }
}

/// How a task's time reads back to the model: "daily at 08:30".
pub fn slot_text(slot: &Slot) -> String {
    let time = format!("{:02}:{:02}", slot.minute / 60, slot.minute % 60);
    const DAYS: [&str; 7] = [
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
    ];
    match slot.cadence {
        Cadence::Once => format!(
            "once on {} at {time}",
            slot.date.as_deref().unwrap_or("an unknown day")
        ),
        Cadence::Daily => format!("daily at {time}"),
        Cadence::Weekdays => format!("on weekdays at {time}"),
        Cadence::Weekly => format!(
            "every {} at {time}",
            slot.weekday
                .and_then(|d| DAYS.get(d as usize))
                .copied()
                .unwrap_or("week")
        ),
    }
}

/// Things that happened in the app, gathered so one turn can look at them together.
#[derive(Debug, Default)]
pub struct Feed {
    events: Vec<String>,
    /// An event that is worth a turn on its own (a low queue); the rest only add context.
    wake: bool,
    last_turn: Option<Instant>,
}

impl Feed {
    pub fn note(&mut self, event: impl Into<String>, wake: bool) {
        self.events.push(event.into());
        if self.events.len() > EVENTS_MAX {
            self.events.remove(0);
        }
        self.wake |= wake;
    }

    /// The prompt for a turn of the DJ's own, when one is due: nothing is running, the gap since
    /// the last one has passed, and something worth waking for happened.
    pub fn take(&mut self, now: Instant, busy: bool) -> Option<String> {
        if busy || !self.wake {
            return None;
        }
        if self
            .last_turn
            .is_some_and(|at| now.duration_since(at) < EVENT_GAP)
        {
            return None;
        }
        self.last_turn = Some(now);
        self.wake = false;
        let events = std::mem::take(&mut self.events);
        Some(format!(
            "[events] These happened in Spotifast just now. They are not from the listener.\n{}\n\
             Act only if it helps the listener (for example top up a queue that is running low). \
             If nothing is needed, answer with just: ok",
            events
                .iter()
                .map(|e| format!("- {e}"))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }

    /// Drops what is waiting, for when the DJ is switched off.
    pub fn clear(&mut self) {
        self.events.clear();
        self.wake = false;
    }
}

/// A turn of the DJ's own ends with this when it had nothing to do; the page does not show it.
pub fn is_nothing(answer: &str) -> bool {
    let a = answer.trim().trim_end_matches('.').to_lowercase();
    a == "ok" || a.is_empty()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredLine {
    kind: String,
    text: String,
    #[serde(default)]
    secs: u32,
}

/// One saved conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chat {
    pub id: u64,
    pub title: String,
    /// Unix seconds of the last change.
    pub updated: i64,
    /// Claude Code's id for `--resume`.
    #[serde(default)]
    pub session: Option<String>,
    lines: Vec<StoredLine>,
}

impl Chat {
    pub fn lines(&self) -> Vec<Line> {
        self.lines
            .iter()
            .filter_map(|l| {
                let kind = match l.kind.as_str() {
                    "user" => LineKind::User,
                    "assistant" => LineKind::Assistant,
                    "tool" => LineKind::Tool,
                    "thought" => LineKind::Thought,
                    "error" => LineKind::Error,
                    _ => return None,
                };
                let mut line = Line::new(kind, l.text.clone());
                line.secs = l.secs;
                Some(line)
            })
            .collect()
    }
}

/// The saved chats, newest first, at most [`CHATS_MAX`].
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Chats {
    chats: Vec<Chat>,
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl Chats {
    pub fn open(path: PathBuf) -> Self {
        let mut chats: Chats = load(&path);
        chats.path = Some(path);
        chats
    }

    pub fn all(&self) -> &[Chat] {
        &self.chats
    }

    pub fn get(&self, id: u64) -> Option<&Chat> {
        self.chats.iter().find(|c| c.id == id)
    }

    /// A fresh id for a chat that has no line yet.
    pub fn next_id(&self) -> u64 {
        self.chats.iter().map(|c| c.id).max().unwrap_or(0) + 1
    }

    /// Stores the chat `id` as it is now. A chat with no listener message is not worth keeping.
    pub fn put(&mut self, id: u64, lines: &[Line], session: Option<String>, now: i64) {
        let Some(first) = lines.iter().find(|l| l.kind == LineKind::User) else {
            return;
        };
        let title: String = first.text.chars().take(60).collect();
        let skip = lines.len().saturating_sub(CHAT_LINES_MAX);
        let lines = lines[skip..]
            .iter()
            .map(|l| StoredLine {
                kind: match l.kind {
                    LineKind::User => "user",
                    LineKind::Assistant => "assistant",
                    LineKind::Tool => "tool",
                    LineKind::Thought => "thought",
                    LineKind::Error => "error",
                }
                .into(),
                text: l.text.clone(),
                secs: l.secs,
            })
            .collect();
        self.chats.retain(|c| c.id != id);
        self.chats.insert(
            0,
            Chat {
                id,
                title,
                updated: now,
                session,
                lines,
            },
        );
        self.chats.truncate(CHATS_MAX);
        if let Some(path) = &self.path {
            let _ = save(path, self);
        }
    }

    pub fn delete(&mut self, id: u64) {
        self.chats.retain(|c| c.id != id);
        if let Some(path) = &self.path {
            let _ = save(path, self);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moment(date: &str, hour: u16, minute: u16, weekday: u8) -> Moment {
        Moment {
            date: date.into(),
            minute: hour * 60 + minute,
            weekday,
        }
    }

    fn daily(hour: u16) -> Slot {
        Slot {
            cadence: Cadence::Daily,
            minute: hour * 60,
            weekday: None,
            date: None,
        }
    }

    #[test]
    fn notes_are_deduplicated_capped_and_forgotten_by_substring() {
        let mut m = Memory::default();
        m.remember("Likes  jazz in the morning").unwrap();
        m.remember("likes jazz in the morning").unwrap();
        assert_eq!(m.notes().len(), 1);
        assert!(m.remember("   ").is_err());
        for i in 0..NOTES_MAX {
            let _ = m.remember(&format!("note {i}"));
        }
        assert_eq!(m.notes().len(), NOTES_MAX);
        assert!(m.remember("one too many").is_err());
        assert_eq!(m.forget("JAZZ"), 1);
        assert_eq!(m.forget(""), 0);
        assert!(m.block().unwrap().contains("note 0"));
    }

    #[test]
    fn the_notes_block_stays_small() {
        let mut m = Memory::default();
        for i in 0..NOTES_MAX {
            m.remember(&format!("{i} {}", "x".repeat(190))).unwrap();
        }
        assert!(m.block().unwrap().len() <= BLOCK_MAX);
    }

    #[test]
    fn notes_survive_a_restart_and_a_damaged_file_is_empty() {
        let dir = std::env::temp_dir().join(format!("spotifast-dj-mem-{}", std::process::id()));
        let path = dir.join("dj-memory.json");
        let mut m = Memory::open(path.clone());
        m.remember("no country").unwrap();
        assert_eq!(Memory::open(path.clone()).notes(), ["no country"]);
        std::fs::write(&path, b"{ nope").unwrap();
        assert!(Memory::open(path).notes().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_daily_task_runs_once_a_day_and_not_much_after_its_time() {
        let mut s = Schedules::default();
        let id = s.add(daily(8), "play something gentle").unwrap();
        assert!(s.due(&moment("2026-10-14", 7, 59, 2)).is_empty());
        let due = s.due(&moment("2026-10-14", 8, 0, 2));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, id);
        assert!(s.due(&moment("2026-10-14", 8, 1, 2)).is_empty());
        assert_eq!(s.due(&moment("2026-10-15", 8, 30, 3)).len(), 1);
        // opened far too late: missed, not run
        assert!(s.due(&moment("2026-10-16", 14, 0, 4)).is_empty());
    }

    #[test]
    fn weekday_and_weekly_and_once_cadences() {
        let mut s = Schedules::default();
        s.add(
            Slot {
                cadence: Cadence::Weekdays,
                minute: 9 * 60,
                weekday: None,
                date: None,
            },
            "work playlist",
        )
        .unwrap();
        s.add(
            Slot {
                cadence: Cadence::Weekly,
                minute: 9 * 60,
                weekday: Some(5),
                date: None,
            },
            "saturday brunch",
        )
        .unwrap();
        s.add(
            Slot {
                cadence: Cadence::Once,
                minute: 9 * 60,
                weekday: None,
                date: Some("2026-10-17".into()),
            },
            "party",
        )
        .unwrap();
        // Saturday 2026-10-17
        let due = s.due(&moment("2026-10-17", 9, 5, 5));
        let names: Vec<&str> = due.iter().map(|t| t.prompt.as_str()).collect();
        assert_eq!(names, ["saturday brunch", "party"]);
        // the one-time task is gone; the weekday one still waits for a weekday
        assert_eq!(s.tasks().len(), 2);
        assert_eq!(s.due(&moment("2026-10-19", 9, 0, 0)).len(), 1);
    }

    #[test]
    fn a_past_one_time_task_is_dropped_and_the_list_is_capped() {
        let mut s = Schedules::default();
        s.add(
            Slot {
                cadence: Cadence::Once,
                minute: 60,
                weekday: None,
                date: Some("2026-01-01".into()),
            },
            "old",
        )
        .unwrap();
        assert!(s.due(&moment("2026-10-14", 12, 0, 2)).is_empty());
        assert!(s.tasks().is_empty());
        for _ in 0..SCHEDULES_MAX {
            s.add(daily(8), "x").unwrap();
        }
        assert!(s.add(daily(8), "x").is_err());
        let id = s.tasks()[0].id;
        assert!(s.cancel(id));
        assert!(!s.cancel(id));
    }

    #[test]
    fn slots_read_back_in_words() {
        assert_eq!(slot_text(&daily(8)), "daily at 08:00");
        let weekly = Slot {
            cadence: Cadence::Weekly,
            minute: 18 * 60 + 30,
            weekday: Some(4),
            date: None,
        };
        assert_eq!(slot_text(&weekly), "every Friday at 18:30");
    }

    #[test]
    fn events_wake_the_dj_at_most_once_a_minute_and_only_for_something_worth_it() {
        let mut feed = Feed::default();
        let t0 = Instant::now();
        feed.note("track changed to X", false);
        assert!(feed.take(t0, false).is_none());
        feed.note("queue low: 2 songs left", true);
        assert!(feed.take(t0, true).is_none(), "not while a turn runs");
        let prompt = feed.take(t0, false).unwrap();
        assert!(prompt.contains("track changed to X"));
        assert!(prompt.contains("queue low"));
        feed.note("queue low: 1 song left", true);
        assert!(feed.take(t0 + Duration::from_secs(30), false).is_none());
        assert!(feed.take(t0 + Duration::from_secs(61), false).is_some());
        assert!(feed.take(t0 + Duration::from_secs(200), false).is_none());
    }

    #[test]
    fn a_quiet_answer_is_recognised() {
        assert!(is_nothing("ok"));
        assert!(is_nothing(" OK. "));
        assert!(!is_nothing("Queued 8 songs."));
    }

    #[test]
    fn chats_are_kept_newest_first_and_bounded() {
        let mut chats = Chats::default();
        let lines = vec![
            Line::new(LineKind::User, "something for a run"),
            Line::new(LineKind::Assistant, "Queued."),
        ];
        chats.put(1, &[], None, 0);
        assert!(chats.all().is_empty());
        chats.put(1, &lines, Some("abc".into()), 10);
        chats.put(2, &lines, None, 20);
        chats.put(1, &lines, Some("abc".into()), 30);
        assert_eq!(chats.all()[0].id, 1);
        assert_eq!(chats.all()[0].title, "something for a run");
        assert_eq!(chats.get(1).unwrap().lines().len(), 2);
        assert_eq!(chats.next_id(), 3);
        for id in 10..60 {
            chats.put(id, &lines, None, id as i64);
        }
        assert_eq!(chats.all().len(), CHATS_MAX);
        chats.delete(59);
        assert!(chats.get(59).is_none());
    }
}
