//! Session model for the dashboard. A `Session` is one terminal-backed unit
//! the dashboard can render in any of its tiles. The list of sessions lives
//! once in `Sessions`; tiles look up by `SessionId` so the same underlying
//! Terminal can be referenced from the sidebar, the main terminal slot, and
//! the cards grid simultaneously.

use std::path::PathBuf;
use std::time::Instant;

use crate::session_view::SessionView;

/// Latency before a session that has emitted output but received no input
/// since gets flagged as "needs attention". Set generously so claude's
/// periodic status-line ticks (typically every 1–2 s) don't bridge the
/// threshold and cause the session to oscillate in/out of the queue.
pub const NEEDS_ATTENTION_AFTER_MS: u64 = 4000;
/// While output is still flowing this fast we treat the session as actively
/// "working" instead of needing attention.
pub const WORKING_WITHIN_MS: u64 = 1500;

/// Combined inputs to the status heuristic. We pass them through one
/// struct so [`predict_status`] can produce a debug `reason` summarizing
/// every signal it considered, which the dashboard surfaces in the card
/// title for live diagnosis.
#[derive(Clone, Copy, Debug, Default)]
pub struct StatusInputs {
    pub last_output_ms: u64,
    pub last_input_ms: u64,
    pub last_acknowledged_ms: u64,
    pub now_ms: u64,
    /// PTY-level cursor visibility. claude keeps this `false` for its whole
    /// lifetime, so it isn't load-bearing on its own — included for the
    /// debug reason string.
    pub cursor_visible: bool,
    /// Wall-clock millis when the matching jsonl was last modified, if
    /// `claude_store` knows about one. The matching record is selected by
    /// "newest jsonl in this session's cwd".
    pub jsonl_mtime_ms: Option<u64>,
    /// Last speaker observed in the matching jsonl, if known.
    pub jsonl_speaker: Option<crate::claude_store::LastSpeaker>,
}

#[derive(Clone, Debug)]
pub struct StatusPrediction {
    pub status: SessionStatus,
    /// Short, user-facing label rendered as a card subtitle: one of
    /// `idle`, `thinking`, `assistant`, `human`, `tool`, `stale`.
    pub label: String,
    /// Full debug dump of every signal — only used for the diagnose log,
    /// never shown in the UI.
    pub reason: String,
}

/// One-shot heuristic combining terminal-output cadence, jsonl mtime, and
/// the most recent jsonl speaker. The order of decisions:
///
/// 1. **Thinking** — anything wrote in the last `WORKING_WITHIN_MS`
///    (terminal output cadence OR jsonl mtime). claude's agent loops keep
///    the `(Xs · ↓ N tokens)` cell ticking once per second, and claude
///    itself appends to the jsonl on each turn boundary; either is enough.
/// 2. **NeedsAttention** — the last jsonl entry was an `assistant`
///    message *and* the user hasn't acknowledged it (`last_output > acked`
///    is the legacy fallback when no jsonl is available).
/// 3. **Idle** — everything else.
pub fn predict_status(inputs: StatusInputs) -> StatusPrediction {
    let acked = inputs.last_input_ms.max(inputs.last_acknowledged_ms);
    let since_output = inputs.now_ms.saturating_sub(inputs.last_output_ms);
    let since_jsonl = inputs
        .jsonl_mtime_ms
        .map(|t| inputs.now_ms.saturating_sub(t));
    let since_ack = inputs.now_ms.saturating_sub(acked);

    let term_active = inputs.last_output_ms > 0 && since_output < WORKING_WITHIN_MS;
    let jsonl_active = since_jsonl.map_or(false, |j| j < WORKING_WITHIN_MS);

    // Pick the human-facing label first; the bucket status follows from it.
    let label: &'static str = if term_active || jsonl_active {
        "thinking"
    } else {
        match inputs.jsonl_speaker {
            Some(crate::claude_store::LastSpeaker::Assistant)
                if since_ack > NEEDS_ATTENTION_AFTER_MS =>
            {
                "assistant"
            }
            Some(crate::claude_store::LastSpeaker::ToolResult) => "tool",
            Some(crate::claude_store::LastSpeaker::Human) => "human",
            _ if inputs.last_output_ms > acked && since_output > NEEDS_ATTENTION_AFTER_MS => {
                "assistant"
            }
            _ => "idle",
        }
    };

    let status = match label {
        "thinking" => SessionStatus::Thinking,
        "assistant" => SessionStatus::NeedsAttention,
        _ => SessionStatus::Idle,
    };

    let mut parts: Vec<String> = Vec::new();
    parts.push(format!("o={}s", since_output.saturating_div(1000)));
    if let Some(j) = since_jsonl {
        parts.push(format!("j={}s", j.saturating_div(1000)));
    } else {
        parts.push("j=-".into());
    }
    let speaker_tag = match inputs.jsonl_speaker {
        Some(crate::claude_store::LastSpeaker::Assistant) => "A",
        Some(crate::claude_store::LastSpeaker::Human) => "H",
        Some(crate::claude_store::LastSpeaker::ToolResult) => "R",
        Some(crate::claude_store::LastSpeaker::Unknown) => "U",
        None => "-",
    };
    parts.push(format!("s={speaker_tag}"));
    if !inputs.cursor_visible {
        parts.push("cv=0".into());
    }
    parts.push(format!("ack={}s", since_ack.saturating_div(1000)));
    let reason = format!("{label} | {}", parts.join(" "));

    StatusPrediction {
        status,
        label: label.to_string(),
        reason,
    }
}

pub type SessionId = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Thinking / NeedsAttention land in Phase 3.
pub enum SessionStatus {
    Idle,
    Thinking,
    NeedsAttention,
}

#[allow(dead_code)] // some fields are reserved for future phases.
pub struct Session {
    pub id: SessionId,
    pub name: String,
    pub session_view: SessionView,
    pub cwd: Option<PathBuf>,
    pub status: SessionStatus,
    /// Reserved for future on-Session timing. Live timestamps live on the
    /// underlying `Terminal` (output) and on the input/ack flow below.
    pub last_output_at: Instant,
    pub last_input_at: Instant,
    /// Wall-clock millis when the user last "checked" this session — set
    /// when the session is focused or actively viewed in the dashboard. Used
    /// alongside `last_input_ms` so a session that finished output and the
    /// user has now looked at doesn't keep re-flagging as NeedsAttention.
    pub last_acknowledged_ms: u64,
    /// claude session UUID this PTY is running. Set at spawn time via
    /// `--session-id <uuid>` for fresh sessions, or copied from
    /// `--resume <id>` for resumed ones — always populated. Used both to
    /// look up the matching jsonl record (`claude_store::lookup_by_session_id`)
    /// and to dedupe the orphan card backing the same id off the grid.
    pub session_id: String,
    /// Short human-facing label from [`predict_status`] (`thinking`,
    /// `assistant`, `idle`, …). Rendered as the card subtitle.
    pub status_label: String,
}

pub struct Sessions {
    items: Vec<Session>,
    next_id: SessionId,
}

impl Sessions {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            next_id: 1,
        }
    }

    /// Spawn a new session. The caller has already constructed the SessionView
    /// (which spawned the PTY); we wrap it with the metadata fields and assign
    /// a fresh id. `cwd` is the working directory the PTY was spawned in —
    /// used to match against `~/.claude/projects/` records on the cards grid.
    /// `session_id` is the claude UUID for this conversation: pre-generated
    /// via `claude::new_session_id()` for fresh spawns, or the resumed id for
    /// `--resume` paths.
    pub fn add(
        &mut self,
        name: impl Into<String>,
        session_view: SessionView,
        cwd: Option<PathBuf>,
        session_id: String,
    ) -> SessionId {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let now = Instant::now();
        self.items.push(Session {
            id,
            name: name.into(),
            session_view,
            cwd,
            status: SessionStatus::Idle,
            last_output_at: now,
            last_input_at: now,
            // Treat a freshly-spawned session as already acknowledged — it
            // hasn't said anything that warrants attention yet.
            last_acknowledged_ms: crate::terminal::now_ms(),
            session_id,
            status_label: String::new(),
        });
        id
    }

    pub fn iter(&self) -> impl Iterator<Item = &Session> {
        self.items.iter()
    }

    #[allow(dead_code)]
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Session> {
        self.items.iter_mut()
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn get(&self, id: SessionId) -> Option<&Session> {
        self.items.iter().find(|s| s.id == id)
    }

    pub fn get_mut(&mut self, id: SessionId) -> Option<&mut Session> {
        self.items.iter_mut().find(|s| s.id == id)
    }

    pub fn first_id(&self) -> Option<SessionId> {
        self.items.first().map(|s| s.id)
    }

    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.items.clear();
    }
}

