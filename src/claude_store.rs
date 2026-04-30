//! Process-wide read-only snapshot of `~/.claude/projects/`.
//!
//! Each `<session-id>.jsonl` under `~/.claude/projects/<sanitized-path>/`
//! is one claude session (NDJSON of user/assistant/system events). We
//! periodically walk the tree, tail-read each jsonl, and expose the result
//! as a flat list of `ClaudeSession` records — one per file, *not*
//! aggregated by project. That way each individual conversation can show
//! up as its own card in the dashboard's grid, including ones that happen
//! to share a project directory with a live panel session.
//!
//! The scanner is mtime-driven: a fast 1 Hz tick walks the tree, skips any
//! file whose mtime matches the previously-cached record, and only
//! tail-reads files that have actually changed. That keeps the cost
//! bounded while still surfacing "claude is writing right now" within ~1 s
//! for the activity dot on orphan cards.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime};

use crate::diagnose;

const SCAN_INTERVAL_MS: u64 = 1000;
/// How many bytes to read from the tail of each jsonl. Most recent
/// messages live near the end; full files can be many MB.
const TAIL_BYTES: u64 = 65_536;
/// Max chars of last-message summary to surface on a card.
const SUMMARY_MAX_CHARS: usize = 140;
/// Files modified within this window count as "actively writing" — drives
/// the green status dot regardless of last-speaker.
const THINKING_WINDOW_MS: u64 = 2_500;
/// Files older than this are treated as orphaned conversations and are
/// rendered faded; within the window the card stays first-class.
const ONGOING_WINDOW_MS: u64 = 30 * 60 * 1000;

/// Who/what wrote the last meaningful entry in a jsonl. claude's tool
/// loops produce alternating `assistant` (with tool_use blocks) and
/// `user` (with tool_result content arrays) entries — the latter aren't
/// real human input so we tag them separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LastSpeaker {
    Human,
    Assistant,
    ToolResult,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct ClaudeSession {
    /// Absolute path to the jsonl on disk. Kept on the record so future
    /// "reveal in explorer" / debug actions can reference it directly.
    #[allow(dead_code)]
    pub jsonl_path: PathBuf,
    /// Session UUID parsed from any entry's `sessionId` field. Empty if
    /// the file's contents didn't yield one.
    pub session_id: String,
    /// Canonical cwd as written by claude into the jsonl.
    pub project_path: PathBuf,
    pub last_modified: SystemTime,
    /// One-line summary of the most recent user-or-assistant message.
    pub last_message_summary: String,
    /// Number of `type: "user"` (human only) / `type: "assistant"` entries
    /// observed in the tail slice — a lower bound on the conversation's
    /// real length when files are larger than `TAIL_BYTES`.
    pub message_count: u32,
    /// Role of the most recent meaningful entry. Drives whether an idle
    /// session reads as `NeedsAttention` (claude finished and is waiting)
    /// or just `Idle`.
    pub last_speaker: LastSpeaker,
}

/// Activity state for an orphan/jsonl card. Mirrors the live-session
/// status enum so the card chrome can render a matching dot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionActivity {
    Thinking,
    NeedsAttention,
    Idle,
    Stale,
}

impl ClaudeSession {
    /// Derive an activity state from `last_modified` plus `last_speaker`.
    /// Recent writes always read as `Thinking`. Once writes go quiet, the
    /// last-speaker tag separates "claude finished, your turn"
    /// (`NeedsAttention`) from "claude is in the middle of something"
    /// (`Idle`). Anything older than `ONGOING_WINDOW_MS` reads `Stale`.
    pub fn activity_at(&self, now: SystemTime) -> SessionActivity {
        let age_ms = now
            .duration_since(self.last_modified)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if age_ms >= ONGOING_WINDOW_MS {
            return SessionActivity::Stale;
        }
        if age_ms < THINKING_WINDOW_MS {
            return SessionActivity::Thinking;
        }
        match self.last_speaker {
            LastSpeaker::Assistant => SessionActivity::NeedsAttention,
            _ => SessionActivity::Idle,
        }
    }
}

#[derive(Clone)]
pub struct ClaudeStore {
    sessions: Arc<Mutex<HashMap<PathBuf, ClaudeSession>>>,
}

static GLOBAL: OnceLock<ClaudeStore> = OnceLock::new();

/// Process-wide instance. Lazily starts the scanner thread on first call.
pub fn global() -> &'static ClaudeStore {
    GLOBAL.get_or_init(ClaudeStore::start)
}

impl ClaudeStore {
    fn start() -> Self {
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let map = Arc::clone(&sessions);
        thread::Builder::new()
            .name("claude-store-scan".into())
            .spawn(move || scanner_loop(map))
            .ok();
        Self { sessions }
    }

    /// Most recent session in the given cwd, if any. Used to attach a
    /// "history" badge to live cards whose cwd matches a known project.
    pub fn latest_for_cwd(&self, cwd: &Path) -> Option<ClaudeSession> {
        let key = normalize_key(cwd);
        let m = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        m.values()
            .filter(|s| normalize_key(&s.project_path) == key)
            .max_by_key(|s| s.last_modified)
            .cloned()
    }

    /// Look up a specific jsonl record by its session UUID. Returns `None`
    /// if no scanned jsonl carries that id.
    pub fn lookup_by_session_id(&self, session_id: &str) -> Option<ClaudeSession> {
        if session_id.is_empty() {
            return None;
        }
        let m = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        m.values().find(|s| s.session_id == session_id).cloned()
    }

    /// All known sessions, sorted newest-first.
    pub fn snapshot(&self) -> Vec<ClaudeSession> {
        let m = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<ClaudeSession> = m.values().cloned().collect();
        v.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));
        v
    }
}

/// Lower-case + forward-slash form, suitable for case-insensitive path
/// comparison on Windows where claude writes `C:\` but rust paths may
/// alternate between `\` and `/`. Exposed so `cards_tile` can compare
/// session paths to live-session cwds under the same key.
pub fn normalize_key(p: &Path) -> PathBuf {
    let s: String = p
        .to_string_lossy()
        .chars()
        .map(|c| if c == '\\' { '/' } else { c.to_ascii_lowercase() })
        .collect();
    PathBuf::from(s)
}

fn scanner_loop(sessions: Arc<Mutex<HashMap<PathBuf, ClaudeSession>>>) {
    loop {
        scan_once(&sessions);
        thread::sleep(Duration::from_millis(SCAN_INTERVAL_MS));
    }
}

fn scan_once(sessions: &Mutex<HashMap<PathBuf, ClaudeSession>>) {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let root = home.join(".claude").join("projects");
    let project_dirs = match fs::read_dir(&root) {
        Ok(d) => d,
        Err(_) => return,
    };

    // Snapshot the previous scan's records up front so we can reuse them
    // verbatim for files whose mtime hasn't changed — no re-parse cost.
    let cached: HashMap<PathBuf, ClaudeSession> = {
        let m = sessions.lock().unwrap_or_else(|e| e.into_inner());
        m.clone()
    };

    let mut new_map: HashMap<PathBuf, ClaudeSession> = HashMap::new();
    for entry in project_dirs.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let files = match fs::read_dir(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        for f in files.flatten() {
            let jsonl_path = f.path();
            if !jsonl_path.extension().map_or(false, |x| x == "jsonl") {
                continue;
            }
            let mtime = match f.metadata().and_then(|m| m.modified()) {
                Ok(t) => t,
                Err(_) => continue,
            };
            // Re-use the cached record if the file hasn't been touched
            // since we last parsed it.
            if let Some(prev) = cached.get(&jsonl_path) {
                if prev.last_modified == mtime {
                    new_map.insert(jsonl_path, prev.clone());
                    continue;
                }
            }
            if let Some(session) = scan_jsonl(&jsonl_path, mtime) {
                new_map.insert(jsonl_path, session);
            }
        }
    }

    let mut m = sessions.lock().unwrap_or_else(|e| e.into_inner());
    *m = new_map;
}

/// Tail-read one jsonl and assemble a `ClaudeSession`. Returns `None` if
/// the file is empty or no `cwd` could be parsed from any line.
fn scan_jsonl(path: &Path, last_modified: SystemTime) -> Option<ClaudeSession> {
    let len = fs::metadata(path).ok()?.len();
    let mut file = fs::File::open(path).ok()?;
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);

    let mut project_path: Option<PathBuf> = None;
    let mut session_id: Option<String> = None;
    let mut last_summary: Option<String> = None;
    let mut last_speaker = LastSpeaker::Unknown;
    let mut message_count: u32 = 0;

    let mut lines = text.lines();
    if start > 0 {
        // We seeked mid-line; throw away the partial leader.
        lines.next();
    }
    for line in lines {
        let val: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if project_path.is_none() {
            if let Some(c) = val.get("cwd").and_then(|x| x.as_str()) {
                project_path = Some(PathBuf::from(c));
            }
        }
        if session_id.is_none() {
            if let Some(s) = val.get("sessionId").and_then(|x| x.as_str()) {
                session_id = Some(s.to_string());
            }
        }
        let kind = val.get("type").and_then(|x| x.as_str()).unwrap_or("");
        if kind == "user" || kind == "assistant" {
            // claude's tool loops fake "user" entries for tool results —
            // distinguish them from real human input by content shape.
            let speaker = if kind == "assistant" {
                LastSpeaker::Assistant
            } else if is_tool_result_user(&val) {
                LastSpeaker::ToolResult
            } else {
                LastSpeaker::Human
            };
            last_speaker = speaker;
            // Only count actual messages — tool_result entries shouldn't
            // inflate the conversation length.
            if !matches!(speaker, LastSpeaker::ToolResult) {
                message_count = message_count.saturating_add(1);
            }
            if let Some(s) = extract_message_text(&val) {
                last_summary = Some(s);
            }
        }
    }
    if project_path.is_none() && diagnose::is_enabled() {
        diagnose::log(format!(
            "claude_store: no cwd found in {}",
            path.display()
        ));
    }
    let project_path = project_path?;
    Some(ClaudeSession {
        jsonl_path: path.to_path_buf(),
        session_id: session_id.unwrap_or_default(),
        project_path,
        last_modified,
        last_message_summary: last_summary
            .map(|s| truncate_summary(&s))
            .unwrap_or_default(),
        message_count,
        last_speaker,
    })
}

/// True if a `type: "user"` entry's `message.content` is an array of
/// `tool_result` blocks rather than a plain human prompt string. claude
/// emits these between assistant tool_use turns to feed tool output back
/// into the next agent step.
fn is_tool_result_user(val: &serde_json::Value) -> bool {
    let Some(content) = val.get("message").and_then(|m| m.get("content")) else {
        return false;
    };
    let Some(arr) = content.as_array() else {
        return false;
    };
    arr.iter()
        .any(|b| b.get("type").and_then(|x| x.as_str()) == Some("tool_result"))
}

/// Pull plain-text content out of one jsonl message entry. User entries
/// store `message.content` as a string; assistant entries store it as a
/// `[{type, text}]` array.
fn extract_message_text(val: &serde_json::Value) -> Option<String> {
    let msg = val.get("message")?;
    let content = msg.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        let mut out = String::new();
        for block in arr {
            if block.get("type").and_then(|x| x.as_str()) == Some("text") {
                if let Some(t) = block.get("text").and_then(|x| x.as_str()) {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(t);
                }
            }
        }
        if !out.is_empty() {
            return Some(out);
        }
    }
    None
}

fn truncate_summary(s: &str) -> String {
    let trimmed = s.trim();
    let mut out = String::new();
    let mut count = 0;
    for ch in trimmed.chars() {
        if count >= SUMMARY_MAX_CHARS {
            out.push('…');
            return out;
        }
        if ch == '\n' || ch == '\r' || ch == '\t' {
            if !out.ends_with(' ') {
                out.push(' ');
                count += 1;
            }
        } else {
            out.push(ch);
            count += 1;
        }
    }
    out
}
