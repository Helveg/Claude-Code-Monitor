//! Read-only view of the jsonl transcripts for sessions *this manager
//! spawned*.
//!
//! claude writes each conversation to
//! `~/.claude/projects/<sanitized-cwd>/<session-id>.jsonl` (NDJSON of
//! user/assistant/system events). Because we pre-supply the UUID at spawn
//! time via `--session-id`, we know exactly which filenames belong to us:
//! callers [`track`] an id when they start a session, and the scanner only
//! ever opens files whose stem is a tracked id. Sessions started outside
//! the manager are never read.
//!
//! The transcript is the status signal — it says who spoke last in
//! structured form, which is far steadier than inferring the same thing
//! from terminal output cadence.
//!
//! The scanner is mtime-driven: a 1 Hz tick walks the tree, skips any file
//! whose mtime matches the cached record, and only tail-reads files that
//! actually changed.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime};

const SCAN_INTERVAL_MS: u64 = 1000;
/// How many bytes to read from the tail of each jsonl. The entries we care
/// about live near the end; full files can be many MB.
const TAIL_BYTES: u64 = 65_536;

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
    pub last_modified: SystemTime,
    /// Role of the most recent meaningful entry. Separates "claude
    /// finished and is waiting on you" from "claude is mid-tool-loop".
    pub last_speaker: LastSpeaker,
    /// Tokens the conversation occupied at its last assistant turn — the
    /// whole prompt claude was charged for, cache hits included. `None`
    /// until an assistant entry with a `usage` block has been written.
    pub context_tokens: Option<u64>,
    /// Model id from the last assistant turn, e.g.
    /// `claude-opus-4-5-20251101`. Decides the context limit.
    pub model: Option<String>,
}

/// Models whose context window is the older 200K rather than the 1M the
/// current generation ships with, matched by id prefix. Haiku is 200K at
/// every version so far; the rest are the pre-4.6 Opus and Sonnet lines and
/// everything from the claude-3 era.
const SMALL_CONTEXT_MODELS: &[&str] = &[
    "claude-haiku",
    "claude-opus-4-5",
    "claude-opus-4-1",
    "claude-opus-4-0",
    "claude-sonnet-4-5",
    "claude-sonnet-4-0",
    "claude-3",
];

/// Context window of a model id, in tokens.
///
/// The current generation — Opus 4.6 and later, Sonnet 4.6 and later, Fable
/// and Mythos — is 1M, so that's the default and an unrecognized id gets it
/// too. A handful of older lines are still 200K and are listed out. The
/// legacy `[1m]` suffix marked a long-context variant of a model that was
/// otherwise 200K; ids carrying it are 1M whatever else they match.
pub fn context_limit(model: &str) -> u64 {
    if model.contains("[1m]") {
        return 1_000_000;
    }
    if SMALL_CONTEXT_MODELS
        .iter()
        .any(|prefix| model.starts_with(prefix))
    {
        return 200_000;
    }
    1_000_000
}

impl ClaudeSession {
    /// Context window this conversation is running against.
    pub fn context_limit(&self) -> u64 {
        self.model
            .as_deref()
            .map(context_limit)
            .unwrap_or(1_000_000)
    }
}

#[derive(Clone)]
pub struct ClaudeStore {
    inner: Arc<Inner>,
}

struct Inner {
    /// Session ids the manager has spawned. The scanner reads a jsonl only
    /// when its filename stem appears here.
    tracked: Mutex<HashSet<String>>,
    sessions: Mutex<HashMap<String, ClaudeSession>>,
}

static GLOBAL: OnceLock<ClaudeStore> = OnceLock::new();

/// Process-wide instance. Lazily starts the scanner thread on first call.
pub fn global() -> &'static ClaudeStore {
    GLOBAL.get_or_init(ClaudeStore::start)
}

/// Start following the jsonl for a session we just spawned. Until an id is
/// tracked its transcript is invisible to the store, so this must be called
/// for every session the panel creates.
pub fn track(session_id: &str) {
    global().track(session_id);
}

impl ClaudeStore {
    fn start() -> Self {
        let inner = Arc::new(Inner {
            tracked: Mutex::new(HashSet::new()),
            sessions: Mutex::new(HashMap::new()),
        });
        let scan_inner = Arc::clone(&inner);
        thread::Builder::new()
            .name("claude-store-scan".into())
            .spawn(move || scanner_loop(scan_inner))
            .ok();
        Self { inner }
    }

    fn track(&self, session_id: &str) {
        if session_id.is_empty() {
            return;
        }
        let mut t = self
            .inner
            .tracked
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        t.insert(session_id.to_string());
    }

    /// Latest transcript state for a tracked session. `None` until claude
    /// has written the file, which only happens on the first message —
    /// startup alone doesn't create it.
    pub fn lookup_by_session_id(&self, session_id: &str) -> Option<ClaudeSession> {
        if session_id.is_empty() {
            return None;
        }
        let m = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        m.get(session_id).cloned()
    }
}

fn scanner_loop(inner: Arc<Inner>) {
    loop {
        scan_once(&inner);
        thread::sleep(Duration::from_millis(SCAN_INTERVAL_MS));
    }
}

fn scan_once(inner: &Inner) {
    let tracked: HashSet<String> = {
        let t = inner.tracked.lock().unwrap_or_else(|e| e.into_inner());
        t.clone()
    };
    if tracked.is_empty() {
        return;
    }
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
    let cached: HashMap<String, ClaudeSession> = {
        let m = inner.sessions.lock().unwrap_or_else(|e| e.into_inner());
        m.clone()
    };

    let mut new_map: HashMap<String, ClaudeSession> = HashMap::new();
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
            // claude names the file after the session UUID, so the stem is
            // the whole ownership check — untracked transcripts are never
            // opened.
            let Some(session_id) = jsonl_path
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| tracked.contains(*s))
                .map(str::to_string)
            else {
                continue;
            };
            let mtime = match f.metadata().and_then(|m| m.modified()) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if let Some(prev) = cached.get(&session_id) {
                if prev.last_modified == mtime {
                    new_map.insert(session_id, prev.clone());
                    continue;
                }
            }
            if let Some(session) = scan_jsonl(&jsonl_path, mtime) {
                new_map.insert(session_id, session);
            }
        }
    }

    let mut m = inner.sessions.lock().unwrap_or_else(|e| e.into_inner());
    *m = new_map;
}

/// Tail-read one jsonl and assemble a `ClaudeSession`. Returns `None` if
/// the file couldn't be read.
fn scan_jsonl(path: &Path, last_modified: SystemTime) -> Option<ClaudeSession> {
    let len = fs::metadata(path).ok()?.len();
    let mut file = fs::File::open(path).ok()?;
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);

    let mut last_speaker = LastSpeaker::Unknown;
    let mut context_tokens = None;
    let mut model = None;

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
        let kind = val.get("type").and_then(|x| x.as_str()).unwrap_or("");
        if kind == "user" || kind == "assistant" {
            // claude's tool loops fake "user" entries for tool results —
            // distinguish them from real human input by content shape.
            last_speaker = if kind == "assistant" {
                LastSpeaker::Assistant
            } else if is_tool_result_user(&val) {
                LastSpeaker::ToolResult
            } else {
                LastSpeaker::Human
            };
        }
        if kind == "assistant" {
            // Later turns overwrite earlier ones, so what survives the loop
            // is the most recent turn's figure.
            if let Some(tokens) = context_tokens_of(&val) {
                context_tokens = Some(tokens);
            }
            if let Some(m) = val
                .get("message")
                .and_then(|m| m.get("model"))
                .and_then(|m| m.as_str())
            {
                model = Some(m.to_string());
            }
        }
    }
    Some(ClaudeSession {
        last_modified,
        last_speaker,
        context_tokens,
        model,
    })
}

/// Everything one assistant turn had in its context window: the fresh
/// prompt, both cache buckets, and what it wrote back. Summed because
/// cached input still occupies the window — it is only cheaper, not absent.
fn context_tokens_of(val: &serde_json::Value) -> Option<u64> {
    let usage = val.get("message").and_then(|m| m.get("usage"))?;
    let field = |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    let total = field("input_tokens")
        + field("cache_read_input_tokens")
        + field("cache_creation_input_tokens")
        + field("output_tokens");
    (total > 0).then_some(total)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_tokens_sum_cache_buckets_and_output() {
        let entry: serde_json::Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"model":"claude-opus-5",
                "usage":{"input_tokens":12,"cache_read_input_tokens":30000,
                         "cache_creation_input_tokens":500,"output_tokens":88}}}"#,
        )
        .unwrap();
        assert_eq!(context_tokens_of(&entry), Some(30_600));
    }

    #[test]
    fn a_turn_without_usage_reports_nothing() {
        let entry: serde_json::Value =
            serde_json::from_str(r#"{"type":"assistant","message":{"model":"x"}}"#).unwrap();
        assert_eq!(context_tokens_of(&entry), None);
    }

    /// The current generation is 1M — an id we don't recognize is far more
    /// likely to be a new model than an old one, so it gets 1M too.
    #[test]
    fn the_current_generation_has_the_million_token_window() {
        for model in [
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-opus-4-6",
            "claude-sonnet-5",
            "claude-sonnet-4-6",
            "claude-fable-5",
            "claude-something-not-released-yet",
        ] {
            assert_eq!(context_limit(model), 1_000_000, "{model}");
        }
    }

    #[test]
    fn the_older_lines_are_still_two_hundred_thousand() {
        for model in [
            "claude-haiku-4-5-20251001",
            "claude-opus-4-5-20251101",
            "claude-opus-4-1-20250805",
            "claude-sonnet-4-5-20250929",
            "claude-3-5-sonnet-20241022",
        ] {
            assert_eq!(context_limit(model), 200_000, "{model}");
        }
    }

    /// The legacy long-context suffix outranks the small-window list.
    #[test]
    fn the_long_context_suffix_wins() {
        assert_eq!(context_limit("claude-sonnet-4-5[1m]"), 1_000_000);
    }
}
