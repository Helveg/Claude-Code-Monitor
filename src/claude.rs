//! Build command lines for spawning the claude code CLI inside a session
//! terminal. Handles the Windows-launch dance (`cmd.exe /c claude ...`) so
//! `PATHEXT` resolves whichever shim the user installed (`claude.cmd`,
//! `claude.exe`, `claude.bat`) without us probing for them.
//!
//! Worktree / project-specific sessions don't need a special CLI flag —
//! pass the worktree path as the session's `cwd` instead. claude reads its
//! project context from the working directory.

#[derive(Default, Clone, Debug)]
pub struct ClaudeArgs {
    /// `--resume <session-id>` — resume a specific saved conversation.
    pub resume: Option<String>,
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

impl ClaudeArgs {
    /// Render the full Windows command line. The leading `cmd.exe /c` lets
    /// CreateProcessW launch `claude.cmd` shims which it can't run directly.
    pub fn build_command_line(&self) -> String {
        let mut parts: Vec<String> = vec!["cmd.exe".into(), "/c".into(), "claude".into()];
        if let Some(id) = &self.resume {
            parts.push("--resume".into());
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
fn quote_arg(arg: String) -> String {
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

    #[test]
    fn default_command_is_just_claude() {
        let cmd = ClaudeArgs::default().build_command_line();
        assert_eq!(cmd, "cmd.exe /c claude");
    }

    #[test]
    fn resume_with_session_id() {
        let args = ClaudeArgs {
            resume: Some("abc123".into()),
            ..Default::default()
        };
        assert_eq!(args.build_command_line(), "cmd.exe /c claude --resume abc123");
    }

    #[test]
    fn continue_with_initial_prompt() {
        let args = ClaudeArgs {
            continue_last: true,
            initial_prompt: Some("hello world".into()),
            ..Default::default()
        };
        assert_eq!(
            args.build_command_line(),
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
            args.build_command_line(),
            "cmd.exe /c claude \"she said \\\"hi\\\"\""
        );
    }
}
