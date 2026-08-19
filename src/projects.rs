//! Project discovery: everything the nav tree needs to show
//! "directories claude has been used in", with the conversations that
//! happened in each.
//!
//! claude keeps one directory per project under
//! `~/.claude/projects/<sanitized-cwd>/`, holding a `<session-id>.jsonl`
//! per conversation. The directory name is a lossy encoding of the real
//! path (both `\` and `:` become `-`, and dashes in the path itself are
//! indistinguishable), so it is never parsed as the source of truth:
//!
//! 1. Preferred — the `cwd` field every transcript entry carries. Exact.
//! 2. Fallback for projects whose transcripts have been pruned (the
//!    directory survives because `memory/` lives alongside them) —
//!    [`decode_project_dir`] reconstructs the path by walking the real
//!    filesystem, so an answer is only ever returned if the directory
//!    exists.
//!
//! A background thread rescans every [`SCAN_INTERVAL_MS`]; transcript
//! heads are cached by path because the fields we read from them (`cwd`,
//! the opening prompt) are written once and never rewritten.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime};

use crate::sessions::{SessionId, Sessions};

const SCAN_INTERVAL_MS: u64 = 5_000;
/// Bytes read from the start of each transcript. `cwd` rides along on the
/// first conversation entry and the opening prompt is right behind it;
/// this only has to be big enough to clear the attachment blobs claude
/// writes in between.
const HEAD_BYTES: u64 = 96 * 1024;
/// Upper bound on the title we keep. The nav ellipsizes far shorter than
/// this — the cap just stops a pasted wall of text from living in memory.
const TITLE_MAX_CHARS: usize = 160;

/// One past conversation found in a project's transcript directory.
#[derive(Clone, Debug)]
pub struct HistorySession {
    /// claude session UUID — the jsonl file stem, and what `--resume`
    /// takes.
    pub session_id: String,
    /// Opening human prompt, one line, ellipsized at paint time. Falls
    /// back to a short form of the UUID for transcripts that never got a
    /// typed prompt.
    pub title: String,
    pub last_modified: SystemTime,
}

/// A directory claude has been run in, plus every conversation we found
/// that started there.
#[derive(Clone, Debug)]
pub struct Project {
    pub path: PathBuf,
    /// Last path component — what the nav labels the project with.
    pub name: String,
    /// Newest transcript mtime, or the project directory's own mtime when
    /// no transcripts survive. Orders the nav.
    pub last_active: SystemTime,
    /// Newest first.
    pub sessions: Vec<HistorySession>,
}

#[derive(Clone)]
pub struct ProjectStore {
    inner: Arc<Inner>,
}

struct Inner {
    projects: Mutex<Vec<Project>>,
    /// `<jsonl path> -> (cwd, opening prompt)`, cached across scans.
    heads: Mutex<HashMap<PathBuf, TranscriptHead>>,
    /// `<sanitized dir name> -> real path`, cached because resolving one
    /// costs a handful of directory probes.
    decoded: Mutex<HashMap<String, Option<PathBuf>>>,
}

#[derive(Clone, Debug)]
struct TranscriptHead {
    cwd: PathBuf,
    title: String,
}

static GLOBAL: OnceLock<ProjectStore> = OnceLock::new();

/// Process-wide instance. The first call starts the scanner thread, which
/// scans immediately — call it during startup so the tree is populated
/// before the user opens the panel. Reading the whole transcript tree
/// takes long enough (hundreds of milliseconds) that it must not sit on
/// the startup path itself.
pub fn global() -> &'static ProjectStore {
    GLOBAL.get_or_init(ProjectStore::start)
}

impl ProjectStore {
    fn start() -> Self {
        let inner = Arc::new(Inner {
            projects: Mutex::new(Vec::new()),
            heads: Mutex::new(HashMap::new()),
            decoded: Mutex::new(HashMap::new()),
        });
        let scan_inner = Arc::clone(&inner);
        thread::Builder::new()
            .name("project-scan".into())
            .spawn(move || {
                scan_once(&scan_inner);
                log_scan(&scan_inner);
                loop {
                    thread::sleep(Duration::from_millis(SCAN_INTERVAL_MS));
                    scan_once(&scan_inner);
                }
            })
            .ok();
        Self { inner }
    }

    /// Copy of the latest scan, newest project first.
    pub fn snapshot(&self) -> Vec<Project> {
        self.inner
            .projects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Dump the first scan's result to the diagnose log — the answer to "why
/// isn't project X in the nav" is almost always visible here: either its
/// directory name couldn't be resolved to a real path, or its transcripts
/// name a different cwd than expected.
fn log_scan(inner: &Inner) {
    if !crate::diagnose::is_enabled() {
        return;
    }
    let projects = inner.projects.lock().unwrap_or_else(|e| e.into_inner());
    crate::diagnose::log(format!("project scan found {} projects", projects.len()));
    for project in projects.iter() {
        crate::diagnose::log(format!(
            "  project {} ({} sessions)",
            project.path.display(),
            project.sessions.len()
        ));
    }
}

fn scan_once(inner: &Inner) {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let root = home.join(".claude").join("projects");
    let Ok(dirs) = fs::read_dir(&root) else {
        return;
    };

    // Keyed by the lowercased path so Windows' case-insensitive
    // filesystem doesn't split one project in two.
    let mut by_path: HashMap<String, Project> = HashMap::new();

    for entry in dirs.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&dir) else {
            continue;
        };

        // Transcripts first: they name their own cwd, so a directory with
        // any readable transcript never needs the name decoded.
        let mut found: Vec<(PathBuf, HistorySession)> = Vec::new();
        for f in files.flatten() {
            let path = f.path();
            if !path.extension().map_or(false, |x| x == "jsonl") {
                continue;
            }
            let Some(session_id) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string)
            else {
                continue;
            };
            let Ok(mtime) = f.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            let Some(head) = cached_head(inner, &path) else {
                continue;
            };
            let title = if head.title.is_empty() {
                short_id(&session_id)
            } else {
                head.title.clone()
            };
            found.push((
                head.cwd,
                HistorySession {
                    session_id,
                    title,
                    last_modified: mtime,
                },
            ));
        }

        if found.is_empty() {
            // No transcripts left (or none readable) — the directory only
            // proves a project *existed*. Recover its path from the name,
            // and drop it if that no longer resolves to a real directory.
            let Some(name) = dir.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(path) = cached_decode(inner, name) else {
                continue;
            };
            let mtime = fs::metadata(&dir)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            merge_project(&mut by_path, path, mtime, None);
            continue;
        }

        for (cwd, session) in found {
            let mtime = session.last_modified;
            merge_project(&mut by_path, cwd, mtime, Some(session));
        }
    }

    let mut projects: Vec<Project> = by_path.into_values().collect();
    for p in projects.iter_mut() {
        p.sessions.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));
    }
    projects.sort_by(|a, b| project_order(a).cmp(&project_order(b)));

    *inner.projects.lock().unwrap_or_else(|e| e.into_inner()) = projects;
}

/// Sort key for the nav: project name, then path to break ties between two
/// directories with the same name.
///
/// Deliberately *not* by recency. Every claude session anywhere — including
/// ones running outside the manager — touches a transcript every few
/// seconds, and ordering on that would have rows swapping places under the
/// cursor between aiming at a project's `+` and clicking it, which starts a
/// session in the wrong directory. The nav is a directory listing; it
/// should sit still. Recency instead decides which project opens expanded.
fn project_order(project: &Project) -> (String, String) {
    (project.name.to_lowercase(), path_key(&project.path))
}

/// Fold one transcript (or a bare project directory) into the map,
/// keeping the project's `last_active` at the newest contribution.
fn merge_project(
    by_path: &mut HashMap<String, Project>,
    path: PathBuf,
    mtime: SystemTime,
    session: Option<HistorySession>,
) {
    let key = path_key(&path);
    let project = by_path.entry(key).or_insert_with(|| Project {
        name: display_name(&path),
        path,
        last_active: SystemTime::UNIX_EPOCH,
        sessions: Vec::new(),
    });
    if mtime > project.last_active {
        project.last_active = mtime;
    }
    if let Some(session) = session {
        project.sessions.push(session);
    }
}

/// Read a transcript's head, going through the cache. Results are only
/// cached once both fields are populated — a transcript scanned in the gap
/// between claude creating the file and the user's first prompt has no
/// `cwd` yet and must be retried on the next scan.
fn cached_head(inner: &Inner, path: &Path) -> Option<TranscriptHead> {
    {
        let heads = inner.heads.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(head) = heads.get(path) {
            return Some(head.clone());
        }
    }
    let head = read_head(path)?;
    if !head.title.is_empty() {
        let mut heads = inner.heads.lock().unwrap_or_else(|e| e.into_inner());
        heads.insert(path.to_path_buf(), head.clone());
    }
    Some(head)
}

fn cached_decode(inner: &Inner, dir_name: &str) -> Option<PathBuf> {
    {
        let decoded = inner.decoded.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = decoded.get(dir_name) {
            return hit.clone();
        }
    }
    let resolved = decode_project_dir(dir_name);
    let mut decoded = inner.decoded.lock().unwrap_or_else(|e| e.into_inner());
    decoded.insert(dir_name.to_string(), resolved.clone());
    resolved
}

/// Pull `cwd` and the opening human prompt out of the first
/// [`HEAD_BYTES`] of a transcript. Returns `None` only when the file
/// can't be read at all; a missing prompt comes back as an empty title.
fn read_head(path: &Path) -> Option<TranscriptHead> {
    let mut file = fs::File::open(path).ok()?;
    let mut buf = vec![0u8; HEAD_BYTES as usize];
    let mut filled = 0usize;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => break,
        }
    }
    buf.truncate(filled);
    let text = String::from_utf8_lossy(&buf);

    let mut cwd: Option<PathBuf> = None;
    let mut title = String::new();
    for line in text.lines() {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if cwd.is_none() {
            if let Some(dir) = val.get("cwd").and_then(|c| c.as_str()) {
                cwd = Some(PathBuf::from(dir));
            }
        }
        if title.is_empty() && val.get("type").and_then(|t| t.as_str()) == Some("user") {
            if let Some(text) = val
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
            {
                title = clean_title(text);
            }
        }
        if cwd.is_some() && !title.is_empty() {
            break;
        }
    }
    Some(TranscriptHead {
        cwd: cwd?,
        title,
    })
}

/// Collapse a prompt to one nav-sized line. Returns empty for prompts
/// that aren't really the user talking — slash-command envelopes,
/// system-reminder blocks, and the resume caveat all start with markup or
/// boilerplate and would make every project's history look identical.
fn clean_title(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with('<') || trimmed.starts_with("Caveat:") {
        return String::new();
    }
    let line = trimmed.lines().next().unwrap_or("").trim();
    line.chars().take(TITLE_MAX_CHARS).collect()
}

/// `1968b431-…` → `1968b431`. Stand-in title for a transcript with no
/// typed prompt; still enough for the user to tell two rows apart.
fn short_id(session_id: &str) -> String {
    session_id.split('-').next().unwrap_or(session_id).to_string()
}

/// Comparison key for a path: separators normalized, case folded, no
/// trailing separator. Windows treats all of those as the same directory.
fn path_key(path: &Path) -> String {
    let s = path.to_string_lossy().replace('/', "\\");
    s.trim_end_matches('\\').to_lowercase()
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// Reconstruct the real path behind a sanitized project-directory name by
/// walking the filesystem, encoding each real directory name the same way
/// claude does and matching it against the remaining text.
///
/// The encoding is not invertible on its own: every non-alphanumeric
/// character collapses to `-`, so `git-lapuni-backend` could be
/// `git\lapuni-backend`, `git\lapuni_backend` or `git\lapuni\backend`.
/// Matching against directories that actually exist resolves it — and any
/// name that doesn't resolve (a detached drive, a UNC/WSL path whose host
/// we can't reconstruct) yields `None` and is left out of the nav: we
/// can't offer to start a session in a directory we can't name.
fn decode_project_dir(dir_name: &str) -> Option<PathBuf> {
    // Only `X--…` drive-rooted names are decodable.
    let drive = dir_name.chars().next()?;
    if !drive.is_ascii_alphabetic() || !dir_name[1..].starts_with("--") {
        return None;
    }
    let root = PathBuf::from(format!("{drive}:\\"));
    if !root.is_dir() {
        return None;
    }
    // Directory-listing budget: a safety net against a pathological name
    // fanning out, never reached by real paths (each level normally
    // resolves on its first candidate).
    let mut budget = 64usize;
    resolve_encoded(&root, &dir_name[3..], &mut budget)
}

fn resolve_encoded(base: &Path, remaining: &str, budget: &mut usize) -> Option<PathBuf> {
    if remaining.is_empty() {
        return Some(base.to_path_buf());
    }
    if *budget == 0 {
        return None;
    }
    *budget -= 1;

    let mut candidates: Vec<(PathBuf, String)> = Vec::new();
    for entry in fs::read_dir(base).ok()?.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let encoded = encode_component(&entry.file_name().to_string_lossy());
        if encoded.is_empty() {
            continue;
        }
        if remaining == encoded {
            return Some(path);
        }
        if let Some(rest) = remaining
            .strip_prefix(&encoded)
            .and_then(|r| r.strip_prefix('-'))
        {
            candidates.push((path, rest.to_string()));
        }
    }

    // Longest match first: the more specific directory name is the more
    // likely reading, and it leaves less ambiguity below.
    candidates.sort_by_key(|(_, rest)| rest.len());
    for (path, rest) in candidates {
        if let Some(found) = resolve_encoded(&path, &rest, budget) {
            return Some(found);
        }
    }
    None
}

/// Encode one path component the way claude names project directories:
/// every non-alphanumeric character becomes `-`. That covers separators,
/// the drive colon, dots, underscores and spaces alike.
fn encode_component(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// A session the manager is holding, as the nav renders it under its
/// project.
#[derive(Clone, Debug)]
pub struct LiveRow {
    pub id: SessionId,
    pub label: String,
    /// Restored from the last time the manager was open and not resumed
    /// yet. The row is drawn hollow — it's a session in the workspace, but
    /// nothing is running behind it.
    pub dormant: bool,
}

/// One row of the nav's "needs attention" section: a live session claude has
/// flagged, lifted out of its project so it can be found without hunting.
#[derive(Clone, Debug)]
pub struct AttentionRow {
    pub id: SessionId,
    /// Same label the session carries under its project.
    pub label: String,
    /// Why it wants you — claude's own wording where it gave one
    /// ("input needed", "sandbox request"), else the session's status label.
    pub reason: String,
}

/// Lift every flagged session out of the tree, in tree order, so the nav can
/// show them together above the projects. Reads the labels back out of the
/// tree so a session reads identically in both places.
pub fn attention_rows(tree: &[ProjectNode], sessions: &Sessions) -> Vec<AttentionRow> {
    let mut out: Vec<AttentionRow> = Vec::new();
    for node in tree {
        for live in &node.live {
            if live.dormant {
                continue;
            }
            let Some(session) = sessions.get(live.id) else {
                continue;
            };
            if session.status != crate::sessions::SessionStatus::NeedsAttention {
                continue;
            }
            let reason = if session.status_label.is_empty() {
                "waiting".to_string()
            } else {
                session.status_label.clone()
            };
            out.push(AttentionRow {
                id: live.id,
                label: live.label.clone(),
                reason,
            });
        }
    }
    out
}

/// What the nav actually draws: every known project with its live
/// sessions on top and its remaining history underneath.
#[derive(Clone, Debug)]
pub struct ProjectNode {
    pub path: PathBuf,
    pub name: String,
    pub live: Vec<LiveRow>,
    pub history: Vec<HistorySession>,
}

impl ProjectNode {
    pub fn child_count(&self) -> usize {
        self.live.len() + self.history.len()
    }
}

/// Join the scanned projects with the sessions this manager is running.
///
/// A live session claims the history entry with its UUID — the row moves
/// to the live group and is labelled from it (see
/// [`Session::label`](crate::sessions::Session::label)), so a resumed
/// conversation keeps its name and never appears twice. A session
/// spawned in a directory the scanner hasn't seen yet (no transcripts
/// there) gets a project row of its own, slotted into the same name order
/// as the rest.
pub fn build_tree(projects: &[Project], sessions: &Sessions) -> Vec<ProjectNode> {
    let mut nodes: Vec<ProjectNode> = projects
        .iter()
        .map(|p| ProjectNode {
            path: p.path.clone(),
            name: p.name.clone(),
            live: Vec::new(),
            history: p.sessions.clone(),
        })
        .collect();

    for session in sessions.iter() {
        let Some(cwd) = session.cwd.as_ref() else {
            continue;
        };
        let key = path_key(cwd);
        let idx = match nodes.iter().position(|n| path_key(&n.path) == key) {
            Some(i) => i,
            None => {
                let node = ProjectNode {
                    path: cwd.clone(),
                    name: display_name(cwd),
                    live: Vec::new(),
                    history: Vec::new(),
                };
                let at = insert_position(&nodes, &node);
                nodes.insert(at, node);
                at
            }
        };
        let node = &mut nodes[idx];
        // Take over the matching transcript row, if the scanner has seen
        // one — its opening prompt is a better label than "new session".
        let claimed = node
            .history
            .iter()
            .position(|h| h.session_id == session.session_id)
            .map(|i| node.history.remove(i));
        let label = session.label(claimed.as_ref().map(|h| h.title.as_str()));
        node.live.push(LiveRow {
            id: session.id,
            label,
            dormant: session.is_dormant(),
        });
    }

    nodes
}

/// Narrow the tree to what matches `query`, case-insensitively.
///
/// A project whose own name matches keeps every conversation under it — you
/// searched for the project, so you want to see what's in it. A project that
/// only matches through its conversations survives with just those rows, so
/// the tree reads as a list of hits rather than a list of directories. An
/// empty query is the identity.
pub fn filter_tree(tree: Vec<ProjectNode>, query: &str) -> Vec<ProjectNode> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return tree;
    }
    tree.into_iter()
        .filter_map(|mut node| {
            if node.name.to_lowercase().contains(&needle) {
                return Some(node);
            }
            node.live
                .retain(|l| l.label.to_lowercase().contains(&needle));
            node.history
                .retain(|h| h.title.to_lowercase().contains(&needle));
            (node.child_count() > 0).then_some(node)
        })
        .collect()
}

/// Where `node` belongs in an already name-ordered node list.
fn insert_position(nodes: &[ProjectNode], node: &ProjectNode) -> usize {
    let order = (node.name.to_lowercase(), path_key(&node.path));
    nodes
        .iter()
        .position(|n| (n.name.to_lowercase(), path_key(&n.path)) > order)
        .unwrap_or(nodes.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn clean_title_rejects_markup_and_boilerplate() {
        assert_eq!(clean_title("<command-name>/loop</command-name>"), "");
        assert_eq!(clean_title("Caveat: The messages below…"), "");
        assert_eq!(clean_title("   "), "");
        assert_eq!(clean_title("  fix the nav  "), "fix the nav");
    }

    #[test]
    fn clean_title_keeps_only_the_first_line() {
        assert_eq!(clean_title("add a tree\n\nand a plus button"), "add a tree");
    }

    #[test]
    fn clean_title_caps_length() {
        let long = "x".repeat(TITLE_MAX_CHARS + 50);
        assert_eq!(clean_title(&long).chars().count(), TITLE_MAX_CHARS);
    }

    #[test]
    fn path_key_folds_case_and_separators() {
        assert_eq!(
            path_key(Path::new("C:/Users/Robin/git/")),
            path_key(Path::new("c:\\users\\robin\\git"))
        );
    }

    #[test]
    fn short_id_takes_the_first_uuid_group() {
        assert_eq!(short_id("1968b431-12f0-4ee3-881f-3b3cd4fab514"), "1968b431");
    }

    #[test]
    fn decode_rejects_names_without_a_drive_prefix() {
        assert_eq!(decode_project_dir("--wsl-localhost-Ubuntu-home-robin"), None);
        assert_eq!(decode_project_dir("plain-name"), None);
    }

    fn project(name: &str, path: &str, last_active_secs: u64) -> Project {
        Project {
            path: PathBuf::from(path),
            name: name.to_string(),
            last_active: SystemTime::UNIX_EPOCH + Duration::from_secs(last_active_secs),
            sessions: Vec::new(),
        }
    }

    /// Rows must not move when a transcript is touched — a shifting nav
    /// turns a click on one project's `+` into a session in another.
    #[test]
    fn project_order_ignores_recency() {
        let mut projects = vec![
            project("beehive", "C:\\git\\beehive", 100),
            project("athena", "C:\\git\\athena", 1),
            project("Claude-Code-Monitor", "C:\\git\\Claude-Code-Monitor", 50),
        ];
        projects.sort_by(|a, b| project_order(a).cmp(&project_order(b)));
        let names: Vec<&str> = projects.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["athena", "beehive", "Claude-Code-Monitor"]);

        // Bump the last one's recency to newest; the order must not budge.
        projects[2].last_active = SystemTime::UNIX_EPOCH + Duration::from_secs(999);
        projects.sort_by(|a, b| project_order(a).cmp(&project_order(b)));
        let names: Vec<&str> = projects.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["athena", "beehive", "Claude-Code-Monitor"]);
    }

    #[test]
    fn unknown_project_is_slotted_into_name_order() {
        let node = |name: &str| ProjectNode {
            path: PathBuf::from(format!("C:\\git\\{name}")),
            name: name.to_string(),
            live: Vec::new(),
            history: Vec::new(),
        };
        let nodes = vec![node("athena"), node("beehive"), node("prismarine")];
        assert_eq!(insert_position(&nodes, &node("aardvark")), 0);
        assert_eq!(insert_position(&nodes, &node("Bravo")), 2);
        assert_eq!(insert_position(&nodes, &node("zebra")), 3);
    }

    fn tree_node(name: &str, live: &[&str], history: &[&str]) -> ProjectNode {
        ProjectNode {
            path: PathBuf::from(format!("C:\\git\\{name}")),
            name: name.to_string(),
            live: live
                .iter()
                .enumerate()
                .map(|(i, label)| LiveRow {
                    id: i as SessionId + 1,
                    label: label.to_string(),
                    dormant: false,
                })
                .collect(),
            history: history
                .iter()
                .map(|title| HistorySession {
                    session_id: title.to_string(),
                    title: title.to_string(),
                    last_modified: SystemTime::UNIX_EPOCH,
                })
                .collect(),
        }
    }

    /// A restored session takes its place in the tree, but hollow: it isn't
    /// running, so it can't be asking for anything either.
    #[test]
    fn a_restored_session_joins_the_tree_without_raising_its_hand() {
        use crate::session_view::SessionView;
        use windows::Win32::Foundation::HWND;

        let cwd = PathBuf::from("C:\\git\\beehive");
        let mut sessions = Sessions::new();
        let view = SessionView::new_dormant(
            HWND::default(),
            0,
            "fix the queue",
            "claude --resume id-1",
            Some(cwd.clone()),
        );
        sessions.add("fix the queue", view, Some(cwd), "id-1".to_string());

        let tree = build_tree(&[], &sessions);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].live.len(), 1);
        assert!(tree[0].live[0].dormant);
        assert!(attention_rows(&tree, &sessions).is_empty());
    }

    #[test]
    fn filtering_on_a_project_name_keeps_all_its_conversations() {
        let tree = vec![
            tree_node("beehive", &["fix the queue"], &["add a tile"]),
            tree_node("athena", &[], &["unrelated"]),
        ];
        let hits = filter_tree(tree, "BEE");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "beehive");
        assert_eq!(hits[0].child_count(), 2);
    }

    #[test]
    fn filtering_on_a_topic_keeps_only_the_matching_rows() {
        let tree = vec![
            tree_node("beehive", &["fix the queue"], &["add a tile", "other"]),
            tree_node("athena", &[], &["nothing here"]),
        ];
        let hits = filter_tree(tree, "tile");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].live.is_empty());
        assert_eq!(hits[0].history.len(), 1);
        assert_eq!(hits[0].history[0].title, "add a tile");
    }

    #[test]
    fn an_empty_query_is_the_identity() {
        let tree = vec![tree_node("beehive", &["a"], &["b"])];
        assert_eq!(filter_tree(tree.clone(), "   ").len(), 1);
        assert_eq!(filter_tree(tree, "").len(), 1);
    }

    #[test]
    fn encode_component_flattens_every_non_alphanumeric() {
        assert_eq!(encode_component("lapuni_backend"), "lapuni-backend");
        assert_eq!(encode_component("Abito EXPORT"), "Abito-EXPORT");
        assert_eq!(encode_component("wsl.localhost"), "wsl-localhost");
    }

    /// Both cases claude's naming makes ambiguous, in one tree: a
    /// component containing a dash, and one containing an underscore and a
    /// space. Only a filesystem walk can tell them apart.
    #[test]
    fn decode_resolves_ambiguous_components_against_the_filesystem() {
        let base = std::env::temp_dir().join("nav-decode-test");
        let deep = base.join("a-b").join("c_d e");
        fs::create_dir_all(&deep).expect("create test tree");

        let encoded: String = deep
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let decoded = decode_project_dir(&encoded).expect("should resolve");
        assert_eq!(path_key(&decoded), path_key(&deep));

        let _ = fs::remove_dir_all(&base);
    }
}
