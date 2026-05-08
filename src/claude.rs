//! Build command lines for spawning the claude code CLI inside a session
//! terminal. Handles the Windows-launch dance (`cmd.exe /c claude ...`) so
//! `PATHEXT` resolves whichever shim the user installed (`claude.cmd`,
//! `claude.exe`, `claude.bat`) without us probing for them.
//!
//! Worktree / project-specific sessions don't need a special CLI flag —
//! pass the worktree path as the session's `cwd` instead. claude reads its
//! project context from the working directory.

use std::path::PathBuf;

use windows::Win32::System::Com::CoCreateGuid;

/// Generate a fresh session UUID in the canonical 8-4-4-4-12 lowercase-hex
/// format claude's `--session-id` flag accepts. Pre-supplying the id at
/// spawn time means we never have to discover it post-hoc by scanning
/// `~/.claude/projects/`, which fixed the cross-contamination problem
/// where the newest jsonl in a cwd might belong to a different process.
pub fn new_session_id() -> String {
    let guid = unsafe { CoCreateGuid() }.expect("CoCreateGuid failed");
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        guid.data1,
        guid.data2,
        guid.data3,
        guid.data4[0],
        guid.data4[1],
        guid.data4[2],
        guid.data4[3],
        guid.data4[4],
        guid.data4[5],
        guid.data4[6],
        guid.data4[7],
    )
}

#[derive(Default, Clone, Debug)]
pub struct ClaudeArgs {
    /// `--resume <session-id>` — resume a specific saved conversation.
    pub resume: Option<String>,
    /// `--session-id <uuid>` — pre-supply the UUID claude will use for
    /// this conversation. Set on fresh spawns; mutually exclusive with
    /// `resume` (callers should set one or the other, never both).
    pub session_id: Option<String>,
    /// `--continue` (`-c`) — pick up where the most recent conversation
    /// in this directory left off.
    pub continue_last: bool,
    /// `--model <id>` — pin a specific model for this session.
    pub model: Option<String>,
    /// Initial prompt; goes through as the trailing positional argument.
    pub initial_prompt: Option<String>,
    /// Extra raw args appended verbatim, after everything above. Useful
    /// for flags this struct doesn't model yet.
    pub extra: Vec<String>,
}

/// Locate our `claude.exe` shim co-located with the running manager
/// binary. In production the installer puts `claude-manager.exe` and
/// `claude.exe` in the same directory; dev builds put both in
/// `target/debug/`. When found, the manager invokes the shim directly so
/// subscribe-vs-spawn dispatch happens through our code path — without
/// this, `cmd.exe /c claude` resolves to the user's real claude on PATH
/// and the manager bypasses the shim entirely (so attach silently spawns
/// a duplicate session instead of subscribing to the live one).
fn shim_path() -> Option<PathBuf> {
    let me = std::env::current_exe().ok()?;
    let parent = me.parent()?;
    let candidate = parent.join("claude.exe");
    if candidate.is_file() && candidate != me {
        Some(candidate)
    } else {
        None
    }
}

impl ClaudeArgs {
    /// Render the full Windows command line. Prefers our co-located shim
    /// when present (so manager-launched sessions go through subscribe/
    /// spawn dispatch); falls back to `cmd.exe /c claude` to handle
    /// `claude.cmd`/`claude.bat` shims if the user has those on PATH and
    /// our shim isn't installed.
    pub fn build_command_line(&self) -> String {
        let prefix: Vec<String> = if let Some(shim) = shim_path() {
            vec![shim.to_string_lossy().into_owned()]
        } else {
            vec!["cmd.exe".into(), "/c".into(), "claude".into()]
        };
        self.build_with_prefix(prefix)
    }

    /// Compose `prefix` (the leading process spawn) with the rendered
    /// claude args. Pulled out so unit tests can assert on a stable
    /// prefix without depending on what `shim_path()` discovers in the
    /// test runner's directory.
    fn build_with_prefix(&self, mut parts: Vec<String>) -> String {
        if let Some(id) = &self.resume {
            parts.push("--resume".into());
            parts.push(id.clone());
        }
        if let Some(id) = &self.session_id {
            parts.push("--session-id".into());
            parts.push(id.clone());
        }
        if self.continue_last {
            parts.push("-c".into());
        }
        if let Some(model) = &self.model {
            parts.push("--model".into());
            parts.push(model.clone());
        }
        for a in &self.extra {
            parts.push(a.clone());
        }
        if let Some(prompt) = &self.initial_prompt {
            parts.push(prompt.clone());
        }
        parts
            .into_iter()
            .map(quote_arg)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Quote a single argument so Windows' `CommandLineToArgvW` round-trips it
/// back to the same string. Follows the standard backslash + quote rules.
pub fn quote_arg(arg: String) -> String {
    if arg.is_empty() {
        return "\"\"".into();
    }
    if !arg.chars().any(|c| c.is_whitespace() || c == '"') {
        return arg;
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
            continue;
        }
        if c == '"' {
            for _ in 0..(backslashes * 2 + 1) {
                out.push('\\');
            }
            out.push('"');
            backslashes = 0;
            continue;
        }
        for _ in 0..backslashes {
            out.push('\\');
        }
        backslashes = 0;
        out.push(c);
    }
    for _ in 0..(backslashes * 2) {
        out.push('\\');
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_prefix() -> Vec<String> {
        vec!["cmd.exe".into(), "/c".into(), "claude".into()]
    }

    #[test]
    fn default_command_is_just_claude() {
        let cmd = ClaudeArgs::default().build_with_prefix(legacy_prefix());
        assert_eq!(cmd, "cmd.exe /c claude");
    }

    #[test]
    fn resume_with_session_id() {
        let args = ClaudeArgs {
            resume: Some("abc123".into()),
            ..Default::default()
        };
        assert_eq!(
            args.build_with_prefix(legacy_prefix()),
            "cmd.exe /c claude --resume abc123"
        );
    }

    #[test]
    fn continue_with_initial_prompt() {
        let args = ClaudeArgs {
            continue_last: true,
            initial_prompt: Some("hello world".into()),
            ..Default::default()
        };
        assert_eq!(
            args.build_with_prefix(legacy_prefix()),
            "cmd.exe /c claude -c \"hello world\""
        );
    }

    #[test]
    fn quoting_preserves_quotes_inside_args() {
        let args = ClaudeArgs {
            initial_prompt: Some("she said \"hi\"".into()),
            ..Default::default()
        };
        assert_eq!(
            args.build_with_prefix(legacy_prefix()),
            "cmd.exe /c claude \"she said \\\"hi\\\"\""
        );
    }

    #[test]
    fn session_id_emitted_when_set() {
        let args = ClaudeArgs {
            session_id: Some("1904330e-14aa-4bee-a999-ed2371c72a76".into()),
            ..Default::default()
        };
        assert_eq!(
            args.build_with_prefix(legacy_prefix()),
            "cmd.exe /c claude --session-id 1904330e-14aa-4bee-a999-ed2371c72a76"
        );
    }

    #[test]
    fn new_session_id_is_canonical_uuid() {
        let id = new_session_id();
        assert_eq!(id.len(), 36, "uuid string should be 36 chars: {id}");
        let bytes = id.as_bytes();
        for &i in &[8usize, 13, 18, 23] {
            assert_eq!(bytes[i], b'-', "expected dash at position {i} in {id}");
        }
        for (i, &b) in bytes.iter().enumerate() {
            if [8usize, 13, 18, 23].contains(&i) {
                continue;
            }
            assert!(
                b.is_ascii_hexdigit() && !b.is_ascii_uppercase(),
                "non-lowercase-hex char {} at {i} in {id}",
                b as char
            );
        }
    }
}
