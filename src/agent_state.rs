//! Live session status, straight from claude.
//!
//! Claude Code publishes one small JSON file per running interactive session
//! at `~/.claude/sessions/<pid>.json` and rewrites it on every status change:
//!
//! ```json
//! {"pid":13940,"sessionId":"de26c14f-…","cwd":"…","name":"athena-ec",
//!  "nameSource":"derived","status":"waiting","waitingFor":"input needed",
//!  "updatedAt":…,"statusUpdatedAt":…}
//! ```
//!
//! `status` is one of `busy | shell | idle | waiting`, and `waiting` carries a
//! `waitingFor` reason ("input needed", "sandbox request", "dialog open", or
//! the open dialog's own label). That is the signal the manager cannot derive
//! for itself: a session sitting on a permission prompt looks exactly like an
//! idle one from the outside.
//!
//! `name` is the conversation's own name, and `nameSource` says where it came
//! from: `user` for one the user set, anything else for one claude derived.
//! It is there from the first write, which is what makes it worth reading —
//! a conversation has a name well before it has a transcript to take a title
//! from.
//!
//! `claude agents --json` reports the same data, but spawning the CLI costs
//! ~650 ms and a 260 MB process; these files are the source it reads.
//!
//! The format is internal and carries no stability guarantee, so every field
//! is parsed defensively and a session with no readable record falls back to
//! the cadence heuristic in [`crate::sessions::predict_status`].

use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

const SCAN_INTERVAL_MS: u64 = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    /// Working — model call or tool loop in flight.
    Busy,
    /// Dropped to a shell.
    Shell,
    /// Nothing in flight. Note this is also what "assistant finished, your
    /// turn" looks like; the turn boundary is `status_updated_ms`.
    Idle,
    /// Blocked on the user: permission prompt, dialog, elicitation.
    Waiting,
}

impl AgentStatus {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "busy" => Some(Self::Busy),
            "shell" => Some(Self::Shell),
            "idle" => Some(Self::Idle),
            "waiting" => Some(Self::Waiting),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AgentState {
    pub status: AgentStatus,
    /// What a `Waiting` session wants from the user. Absent for every other
    /// status.
    pub waiting_for: Option<String>,
    /// The conversation's name as claude publishes it, e.g. `athena-ec`.
    pub name: Option<String>,
    /// True when `name` is one the user set rather than one claude derived.
    /// A name the user chose outranks anything the manager can work out for
    /// itself.
    pub name_from_user: bool,
    /// Wall-clock millis of the last *status* change — the turn boundary.
    pub status_updated_ms: u64,
    /// Wall-clock millis of the last write of any kind.
    pub updated_ms: u64,
}

#[derive(Clone)]
pub struct AgentStateStore {
    inner: Arc<Inner>,
}

struct Inner {
    /// Keyed by claude session UUID, which is what the manager knows about
    /// its own sessions (it pins one per spawn via `--session-id`).
    states: Mutex<HashMap<String, AgentState>>,
}

static GLOBAL: OnceLock<AgentStateStore> = OnceLock::new();

/// Process-wide instance. Lazily starts the scanner thread on first call.
pub fn global() -> &'static AgentStateStore {
    GLOBAL.get_or_init(AgentStateStore::start)
}

impl AgentStateStore {
    fn start() -> Self {
        let inner = Arc::new(Inner {
            states: Mutex::new(HashMap::new()),
        });
        let scan_inner = Arc::clone(&inner);
        thread::Builder::new()
            .name("agent-state-scan".into())
            .spawn(move || loop {
                scan_once(&scan_inner);
                thread::sleep(Duration::from_millis(SCAN_INTERVAL_MS));
            })
            .ok();
        Self { inner }
    }

    /// Latest published state for a session UUID, or `None` when claude has
    /// written no record for it (older CLI, or the session died).
    pub fn lookup(&self, session_id: &str) -> Option<AgentState> {
        if session_id.is_empty() {
            return None;
        }
        let m = self.inner.states.lock().unwrap_or_else(|e| e.into_inner());
        m.get(session_id).cloned()
    }
}

fn scan_once(inner: &Inner) {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let dir = home.join(".claude").join("sessions");
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };

    let mut found: HashMap<String, AgentState> = HashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.extension().map_or(false, |x| x == "json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Some((session_id, state)) = parse_record(&text) else {
            continue;
        };
        // A resumed conversation keeps its UUID, so a previous run's file can
        // still be on disk under a dead pid. The live one is the one that
        // wrote most recently.
        match found.get(&session_id) {
            Some(prev) if prev.updated_ms >= state.updated_ms => {}
            _ => {
                found.insert(session_id, state);
            }
        }
    }

    let mut m = inner.states.lock().unwrap_or_else(|e| e.into_inner());
    *m = found;
}

/// Pull the fields we use out of one session record. Returns `None` for
/// anything that isn't a live interactive session we can key by UUID.
fn parse_record(text: &str) -> Option<(String, AgentState)> {
    let val: serde_json::Value = serde_json::from_str(text).ok()?;
    let session_id = val.get("sessionId")?.as_str()?.to_string();
    if session_id.is_empty() {
        return None;
    }
    let status = AgentStatus::parse(val.get("status")?.as_str()?)?;
    let waiting_for = val
        .get("waitingFor")
        .and_then(|w| w.as_str())
        .map(str::to_string)
        .filter(|w| !w.is_empty());
    let name = val
        .get("name")
        .and_then(|n| n.as_str())
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());
    let name_from_user = val.get("nameSource").and_then(|s| s.as_str()) == Some("user");
    let updated_ms = val.get("updatedAt").and_then(|u| u.as_u64()).unwrap_or(0);
    let status_updated_ms = val
        .get("statusUpdatedAt")
        .and_then(|u| u.as_u64())
        .unwrap_or(updated_ms);
    Some((
        session_id,
        AgentState {
            status,
            waiting_for,
            name,
            name_from_user,
            status_updated_ms,
            updated_ms,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = r#"{"pid":13940,"sessionId":"de26c14f-3437-45ef-adb4-7fab128bd8ce",
        "cwd":"C:\\git\\x","startedAt":1785761004690,"version":"2.1.220","kind":"interactive",
        "entrypoint":"cli","name":"x-1f","status":"busy","updatedAt":1785763873693,
        "statusUpdatedAt":1785763873693}"#;

    #[test]
    fn parses_a_live_record() {
        let (id, state) = parse_record(LIVE).expect("should parse");
        assert_eq!(id, "de26c14f-3437-45ef-adb4-7fab128bd8ce");
        assert_eq!(state.status, AgentStatus::Busy);
        assert_eq!(state.waiting_for, None);
        assert_eq!(state.status_updated_ms, 1785763873693);
        assert_eq!(state.name.as_deref(), Some("x-1f"));
        assert!(!state.name_from_user);
    }

    #[test]
    fn a_user_set_name_is_marked_as_theirs() {
        let json = r#"{"sessionId":"abc","status":"idle","name":"the refactor",
            "nameSource":"user","updatedAt":10}"#;
        let (_, state) = parse_record(json).expect("should parse");
        assert_eq!(state.name.as_deref(), Some("the refactor"));
        assert!(state.name_from_user);
    }

    /// A record with no name at all, or a blank one, must leave the label
    /// decision to whatever the manager knows.
    #[test]
    fn a_blank_or_missing_name_is_no_name() {
        let blank = r#"{"sessionId":"abc","status":"idle","name":"  ","updatedAt":10}"#;
        assert_eq!(parse_record(blank).expect("should parse").1.name, None);
        let missing = r#"{"sessionId":"abc","status":"idle","updatedAt":10}"#;
        assert_eq!(parse_record(missing).expect("should parse").1.name, None);
    }

    #[test]
    fn waiting_carries_its_reason() {
        let json = r#"{"sessionId":"abc","status":"waiting","waitingFor":"input needed",
            "updatedAt":10,"statusUpdatedAt":10}"#;
        let (_, state) = parse_record(json).expect("should parse");
        assert_eq!(state.status, AgentStatus::Waiting);
        assert_eq!(state.waiting_for.as_deref(), Some("input needed"));
    }

    /// The format is internal: an unknown status, a missing field or a
    /// renamed one must leave us on the fallback heuristic rather than
    /// producing a wrong answer.
    #[test]
    fn unusable_records_are_skipped() {
        assert!(parse_record(r#"{"sessionId":"abc","status":"teleporting"}"#).is_none());
        assert!(parse_record(r#"{"sessionId":"abc"}"#).is_none());
        assert!(parse_record(r#"{"status":"idle"}"#).is_none());
        assert!(parse_record("not json").is_none());
    }

    /// statusUpdatedAt is the turn boundary; fall back to updatedAt when a
    /// record predates the field.
    #[test]
    fn status_timestamp_falls_back_to_the_write_timestamp() {
        let json = r#"{"sessionId":"abc","status":"idle","updatedAt":42}"#;
        let (_, state) = parse_record(json).expect("should parse");
        assert_eq!(state.status_updated_ms, 42);
    }
}
