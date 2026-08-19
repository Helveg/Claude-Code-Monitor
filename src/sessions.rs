//! Session model for the dashboard. A `Session` is one terminal-backed unit
//! the dashboard can render in any of its tiles. The list of sessions lives
//! once in `Sessions`; tiles look up by `SessionId` so the same underlying
//! Terminal can be referenced from the sidebar, the main terminal slot, and
//! the terminal grid simultaneously.

use std::path::PathBuf;
use std::time::Instant;

use crate::grid_tile::GridPlacement;
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
#[derive(Clone, Debug, Default)]
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
    /// What claude itself published for this session, when a record exists.
    /// Authoritative where it speaks — most of all for `waiting`, which no
    /// amount of watching the terminal can distinguish from idle.
    pub agent: Option<crate::agent_state::AgentState>,
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

/// Decide a session's status.
///
/// When claude has published a record for the session ([`crate::agent_state`])
/// that record decides it:
///
/// * `waiting` — blocked on the user (permission prompt, dialog, elicitation).
///   **NeedsAttention**, labelled with claude's own reason, until the user
///   acknowledges it. A later prompt bumps `status_updated_ms` and re-raises.
/// * `busy` / `shell` — **Thinking**.
/// * `idle` — nothing in flight, which is also what "your turn" looks like,
///   so the transcript decides: assistant spoke last and the user hasn't
///   looked since → **NeedsAttention**.
///
/// Without a record (older CLI, session not started yet) it falls back to the
/// original heuristic over terminal-output cadence, jsonl mtime and the last
/// jsonl speaker.
pub fn predict_status(inputs: StatusInputs) -> StatusPrediction {
    let acked = inputs.last_input_ms.max(inputs.last_acknowledged_ms);
    let since_output = inputs.now_ms.saturating_sub(inputs.last_output_ms);
    let since_jsonl = inputs
        .jsonl_mtime_ms
        .map(|t| inputs.now_ms.saturating_sub(t));
    let since_ack = inputs.now_ms.saturating_sub(acked);

    let term_active = inputs.last_output_ms > 0 && since_output < WORKING_WITHIN_MS;
    let jsonl_active = since_jsonl.map_or(false, |j| j < WORKING_WITHIN_MS);

    // "claude finished its turn and the user hasn't looked since." Shared by
    // both paths; only the timestamp standing for "finished" differs.
    let assistant_waiting = |finished_ms: u64| {
        matches!(
            inputs.jsonl_speaker,
            Some(crate::claude_store::LastSpeaker::Assistant)
        ) && finished_ms > acked
    };

    // Pick the human-facing label first; the bucket status follows from it.
    let label: String = match inputs.agent.as_ref() {
        Some(agent) => {
            use crate::agent_state::AgentStatus;
            match agent.status {
                AgentStatus::Waiting => {
                    let reason = agent.waiting_for.clone().unwrap_or_else(|| "waiting".into());
                    if agent.status_updated_ms > acked {
                        reason
                    } else {
                        // Already looked at — keep claude's wording, drop the
                        // flag, so a prompt the user is answering stops
                        // shouting at them.
                        format!("{reason} (seen)")
                    }
                }
                AgentStatus::Busy => "thinking".into(),
                AgentStatus::Shell => "shell".into(),
                AgentStatus::Idle if assistant_waiting(agent.status_updated_ms) => {
                    "assistant".into()
                }
                AgentStatus::Idle => "idle".into(),
            }
        }
        None if term_active || jsonl_active => "thinking".into(),
        None => match inputs.jsonl_speaker {
            Some(crate::claude_store::LastSpeaker::Assistant)
                if since_ack > NEEDS_ATTENTION_AFTER_MS =>
            {
                "assistant".into()
            }
            Some(crate::claude_store::LastSpeaker::ToolResult) => "tool".into(),
            Some(crate::claude_store::LastSpeaker::Human) => "human".into(),
            _ if inputs.last_output_ms > acked && since_output > NEEDS_ATTENTION_AFTER_MS => {
                "assistant".into()
            }
            _ => "idle".into(),
        },
    };

    // Everything claude reports as `waiting` and hasn't been acknowledged is
    // an attention case, whatever wording it used for the reason.
    let waiting_unacked = inputs.agent.as_ref().map_or(false, |a| {
        a.status == crate::agent_state::AgentStatus::Waiting && a.status_updated_ms > acked
    });
    let status = match label.as_str() {
        _ if waiting_unacked => SessionStatus::NeedsAttention,
        "thinking" | "shell" => SessionStatus::Thinking,
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
    match inputs.agent.as_ref() {
        Some(a) => parts.push(format!(
            "a={:?}{}",
            a.status,
            a.waiting_for
                .as_deref()
                .map(|w| format!("/{w}"))
                .unwrap_or_default()
        )),
        None => parts.push("a=-".into()),
    }
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

/// One entry of the manager's remembered workspace: everything needed to
/// put a session back on screen after a restart without running anything.
/// The conversation itself lives in claude's own transcript, so a name, a
/// directory and the UUID are the whole record.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SavedSession {
    pub session_id: String,
    pub cwd: PathBuf,
    /// What the row was labelled with when the manager last had it open —
    /// the conversation's opening prompt where the scanner had resolved
    /// one, else the project name.
    pub name: String,
    /// The cell the user dragged this conversation to in the grid view, and
    /// how many cells they stretched it across. Absent for one that has
    /// never been arranged: it flows into whatever cell is free.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<GridPlacement>,
}

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
    /// `--session-id <uuid>` — always populated. Used to look up the
    /// matching jsonl record (`claude_store::lookup_by_session_id`).
    pub session_id: String,
    /// Short human-facing label from [`predict_status`] (`thinking`,
    /// `assistant`, `idle`, …). Rendered as the card subtitle.
    pub status_label: String,
    /// Latest record claude published for this conversation, refreshed on
    /// every status pass. Carries the conversation's own name as well as its
    /// status — see [`Session::label`].
    pub agent: Option<crate::agent_state::AgentState>,
    /// Cell this session occupies in the grid view, once the user has moved
    /// or resized it. `None` means it flows into the first free cell, which
    /// is where every session starts out.
    pub placement: Option<GridPlacement>,
}

impl Session {
    /// True while this session is only a placeholder — restored from the
    /// last time the manager was open, with no process behind it. Its slot
    /// shows the resume card instead of a terminal.
    pub fn is_dormant(&self) -> bool {
        self.session_view.is_dormant()
    }

    /// What to call this conversation, best source first.
    ///
    /// A name the user set in claude wins outright — they said what this is.
    /// Otherwise the transcript's opening prompt (`transcript_title`), which
    /// says more about the conversation than any generated name, but which
    /// only exists once the user has sent a first message. Until then,
    /// claude's own derived name (`athena-ec`), which is published from the
    /// moment the session starts. `self.name` — the directory-plus-number
    /// label the session was spawned with — is only what's left when claude
    /// has published nothing at all.
    pub fn label(&self, transcript_title: Option<&str>) -> String {
        let published = self.agent.as_ref().and_then(|a| a.name.as_deref());
        if self.agent.as_ref().is_some_and(|a| a.name_from_user) {
            if let Some(name) = published {
                return name.to_string();
            }
        }
        transcript_title
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .or(published)
            .unwrap_or(&self.name)
            .to_string()
    }
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
    /// used to match against `~/.claude/projects/` records in the nav tree.
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
        let dormant = session_view.is_dormant();
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
            status_label: if dormant { "paused".into() } else { String::new() },
            agent: None,
            placement: None,
        });
        id
    }

    /// Drop a session. A live one's PTY closes with it — `Terminal`'s drop
    /// tears the pseudoconsole down so claude can flush and exit.
    pub fn remove(&mut self, id: SessionId) -> bool {
        let before = self.items.len();
        self.items.retain(|s| s.id != id);
        self.items.len() != before
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


#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_state::{AgentState, AgentStatus};
    use crate::claude_store::LastSpeaker;
    use windows::Win32::Foundation::{HWND, RECT};

    /// A restored session is a placeholder: laid out, labelled, and holding
    /// its slot, with no process behind it until the user resumes it.
    #[test]
    fn a_restored_session_stays_dormant_until_it_is_woken() {
        let mut sessions = Sessions::new();
        let view = SessionView::new_dormant(
            HWND::default(),
            0,
            "add a nav tile",
            "claude --resume id-1",
            None,
        );
        let id = sessions.add("add a nav tile", view, None, "id-1".to_string());
        assert!(sessions.get(id).unwrap().is_dormant());
        assert_eq!(sessions.get(id).unwrap().status_label, "paused");

        // Laying it out gives it geometry, and must not start anything.
        sessions.get_mut(id).unwrap().session_view.set_bounds(
            RECT {
                left: 0,
                top: 0,
                right: 400,
                bottom: 300,
            },
            96,
            9,
        );
        assert!(
            sessions
                .get(id)
                .unwrap()
                .session_view
                .terminal()
                .grid_arc()
                .is_none(),
            "a dormant session must not spawn a PTY"
        );

        sessions.get_mut(id).unwrap().session_view.wake();
        assert!(!sessions.get(id).unwrap().is_dormant());
    }

    #[test]
    fn removing_a_session_takes_it_out_once() {
        let mut sessions = Sessions::new();
        let view = SessionView::new_dormant(HWND::default(), 0, "x", "cmd", None);
        let id = sessions.add("x", view, None, "id-1".to_string());
        assert!(sessions.remove(id));
        assert!(!sessions.remove(id));
        assert!(sessions.get(id).is_none());
    }

    fn agent(status: AgentStatus, waiting_for: Option<&str>, status_updated_ms: u64) -> AgentState {
        AgentState {
            status,
            waiting_for: waiting_for.map(str::to_string),
            name: None,
            name_from_user: false,
            status_updated_ms,
            updated_ms: status_updated_ms,
        }
    }

    /// A conversation is named by claude from the moment it starts, so a
    /// fresh session shows that rather than the directory-and-number label
    /// it was spawned with.
    #[test]
    fn a_fresh_session_takes_claudes_own_name() {
        let mut session = test_session("athena (2)");
        session.agent = Some(named("athena-ec", false));
        assert_eq!(session.label(None), "athena-ec");
    }

    /// Once there is a transcript, its opening prompt says more about the
    /// conversation than a generated name does.
    #[test]
    fn the_opening_prompt_outranks_a_derived_name() {
        let mut session = test_session("athena (2)");
        session.agent = Some(named("athena-ec", false));
        assert_eq!(session.label(Some("fix the flaky test")), "fix the flaky test");
    }

    /// Except when the user named the conversation themselves.
    #[test]
    fn a_user_set_name_outranks_everything() {
        let mut session = test_session("athena (2)");
        session.agent = Some(named("the refactor", true));
        assert_eq!(session.label(Some("fix the flaky test")), "the refactor");
    }

    /// No record and no transcript — an older CLI, or the first second of a
    /// session — leaves the spawn label standing.
    #[test]
    fn the_spawn_label_is_the_last_resort() {
        let session = test_session("athena (2)");
        assert_eq!(session.label(None), "athena (2)");
        assert_eq!(session.label(Some("   ")), "athena (2)");
    }

    fn test_session(name: &str) -> Session {
        let mut sessions = Sessions::new();
        let view = SessionView::new_dormant(HWND::default(), 0, name, "cmd", None);
        let id = sessions.add(name, view, None, "id-1".to_string());
        sessions.items.into_iter().find(|s| s.id == id).unwrap()
    }

    fn named(name: &str, from_user: bool) -> AgentState {
        AgentState {
            name: Some(name.to_string()),
            name_from_user: from_user,
            ..agent(AgentStatus::Idle, None, 0)
        }
    }

    /// The whole point of reading claude's own record: a session sitting on a
    /// permission prompt is indistinguishable from an idle one out here.
    #[test]
    fn waiting_needs_attention_and_says_why() {
        let p = predict_status(StatusInputs {
            now_ms: 10_000,
            last_acknowledged_ms: 1_000,
            agent: Some(agent(AgentStatus::Waiting, Some("input needed"), 5_000)),
            ..Default::default()
        });
        assert_eq!(p.status, SessionStatus::NeedsAttention);
        assert_eq!(p.label, "input needed");
    }

    /// Tabbing to (or clicking) a flagged session acknowledges it. The prompt
    /// is still open, so claude still says `waiting` — but it must stop
    /// shouting until something newer happens.
    #[test]
    fn acknowledging_clears_the_flag_until_the_next_prompt() {
        let seen = predict_status(StatusInputs {
            now_ms: 10_000,
            last_acknowledged_ms: 6_000,
            agent: Some(agent(AgentStatus::Waiting, Some("input needed"), 5_000)),
            ..Default::default()
        });
        assert_eq!(seen.status, SessionStatus::Idle);

        // A second prompt arrives after the acknowledgement: flagged again.
        let again = predict_status(StatusInputs {
            now_ms: 10_000,
            last_acknowledged_ms: 6_000,
            agent: Some(agent(AgentStatus::Waiting, Some("dialog open"), 7_000)),
            ..Default::default()
        });
        assert_eq!(again.status, SessionStatus::NeedsAttention);
        assert_eq!(again.label, "dialog open");
    }

    #[test]
    fn busy_and_shell_are_thinking() {
        for status in [AgentStatus::Busy, AgentStatus::Shell] {
            let p = predict_status(StatusInputs {
                now_ms: 10_000,
                agent: Some(agent(status, None, 9_000)),
                ..Default::default()
            });
            assert_eq!(p.status, SessionStatus::Thinking, "{status:?}");
        }
    }

    /// `idle` is also what "claude answered, your turn" looks like, so the
    /// transcript decides — and a session that only just started (nothing in
    /// the transcript yet) must not flash as needing attention.
    #[test]
    fn idle_defers_to_the_transcript() {
        let answered = predict_status(StatusInputs {
            now_ms: 10_000,
            last_acknowledged_ms: 1_000,
            jsonl_speaker: Some(LastSpeaker::Assistant),
            agent: Some(agent(AgentStatus::Idle, None, 5_000)),
            ..Default::default()
        });
        assert_eq!(answered.status, SessionStatus::NeedsAttention);

        let fresh = predict_status(StatusInputs {
            now_ms: 10_000,
            last_acknowledged_ms: 1_000,
            jsonl_speaker: None,
            agent: Some(agent(AgentStatus::Idle, None, 5_000)),
            ..Default::default()
        });
        assert_eq!(fresh.status, SessionStatus::Idle);
    }

    /// No record (older CLI, or claude hasn't written one yet) leaves the
    /// original cadence heuristic in charge.
    #[test]
    fn without_a_record_the_heuristic_still_runs() {
        let thinking = predict_status(StatusInputs {
            now_ms: 10_000,
            last_output_ms: 9_800,
            ..Default::default()
        });
        assert_eq!(thinking.status, SessionStatus::Thinking);

        let waiting_on_user = predict_status(StatusInputs {
            now_ms: 10_000,
            last_output_ms: 1_000,
            last_acknowledged_ms: 500,
            jsonl_speaker: Some(LastSpeaker::Assistant),
            ..Default::default()
        });
        assert_eq!(waiting_on_user.status, SessionStatus::NeedsAttention);
    }
}
