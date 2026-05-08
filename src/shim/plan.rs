//! Argv → dispatch plan: decides whether the shim should run as
//! owner/subscriber (we know the session id) or fall back to plain
//! passthrough (claude itself decides the id, we can't coordinate).
//!
//! Lives in the lib so it's reachable from `src/bin/claude.rs` AND
//! testable without spinning up Win32 side effects.

use std::ffi::OsString;

use crate::claude;

pub struct Plan {
    pub forwarded: Vec<OsString>,
    /// `Some` means we know (or generated) the session id. Owner /
    /// subscriber path. `None` means passthrough.
    pub session_id: Option<String>,
}

pub fn plan_args(raw: Vec<OsString>) -> Plan {
    let mut force_passthrough = false;
    let mut known_id: Option<String> = None;
    for (i, arg) in raw.iter().enumerate() {
        let a = arg.to_string_lossy();
        match a.as_ref() {
            "-r" | "--resume" => match raw.get(i + 1) {
                Some(next) => {
                    let nstr = next.to_string_lossy();
                    if !nstr.is_empty() && !nstr.starts_with('-') {
                        known_id = Some(nstr.into_owned());
                    } else {
                        force_passthrough = true;
                    }
                }
                None => {
                    force_passthrough = true;
                }
            },
            "--session-id" => {
                if let Some(next) = raw.get(i + 1) {
                    let nstr = next.to_string_lossy();
                    if !nstr.is_empty() && !nstr.starts_with('-') {
                        known_id = Some(nstr.into_owned());
                    }
                }
            }
            "-c" | "--continue" | "--fork-session" | "--from-pr" => {
                force_passthrough = true;
            }
            _ => {}
        }
    }

    if force_passthrough {
        return Plan {
            forwarded: raw,
            session_id: None,
        };
    }

    if let Some(id) = known_id {
        return Plan {
            forwarded: raw,
            session_id: Some(id),
        };
    }

    let id = claude::new_session_id();
    let mut forwarded = raw;
    forwarded.push(OsString::from("--session-id"));
    forwarded.push(OsString::from(&id));
    Plan {
        forwarded,
        session_id: Some(id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(|s| OsString::from(*s)).collect()
    }

    #[test]
    fn plain_invocation_generates_uuid_and_injects_session_id() {
        let plan = plan_args(os(&[]));
        let id = plan.session_id.expect("plain claude should get an id");
        assert_eq!(id.len(), 36, "expected canonical UUID, got {id:?}");
        // forwarded should now end with --session-id <generated>
        assert_eq!(plan.forwarded.len(), 2);
        assert_eq!(plan.forwarded[0], OsString::from("--session-id"));
        assert_eq!(plan.forwarded[1], OsString::from(&id));
    }

    #[test]
    fn explicit_resume_uses_that_id_no_injection() {
        let plan = plan_args(os(&["--resume", "abc-123"]));
        assert_eq!(plan.session_id.as_deref(), Some("abc-123"));
        assert_eq!(plan.forwarded, os(&["--resume", "abc-123"]));
    }

    #[test]
    fn short_resume_flag_is_recognized() {
        let plan = plan_args(os(&["-r", "xyz"]));
        assert_eq!(plan.session_id.as_deref(), Some("xyz"));
    }

    #[test]
    fn resume_picker_falls_back_to_passthrough() {
        // `claude --resume` with no following id opens claude's interactive
        // picker; we have no way to know which session it'll resolve to.
        let plan = plan_args(os(&["--resume"]));
        assert!(plan.session_id.is_none(), "picker mode → no coordination");
    }

    #[test]
    fn resume_followed_by_flag_is_picker_too() {
        // `--resume --foo` — the next token is another flag, not an id.
        let plan = plan_args(os(&["--resume", "--foo"]));
        assert!(plan.session_id.is_none());
    }

    #[test]
    fn continue_flag_is_passthrough() {
        let plan = plan_args(os(&["-c"]));
        assert!(plan.session_id.is_none());
        let plan = plan_args(os(&["--continue"]));
        assert!(plan.session_id.is_none());
    }

    #[test]
    fn fork_session_is_passthrough() {
        let plan = plan_args(os(&["--fork-session"]));
        assert!(plan.session_id.is_none());
    }

    #[test]
    fn from_pr_is_passthrough() {
        let plan = plan_args(os(&["--from-pr", "123"]));
        assert!(plan.session_id.is_none());
    }

    #[test]
    fn explicit_session_id_passes_through_unchanged() {
        // User-supplied --session-id: we honor their id and don't
        // double-inject ours. The forwarded args are exactly what the
        // user gave.
        let id = "ffc187c1-1030-4297-b431-6ad327d57482";
        let plan = plan_args(os(&["--session-id", id]));
        assert_eq!(plan.session_id.as_deref(), Some(id));
        assert_eq!(plan.forwarded, os(&["--session-id", id]));
    }

    #[test]
    fn resume_takes_precedence_over_other_flags_for_id() {
        // If the user happens to combine flags, --resume's id is what
        // we coordinate against.
        let plan = plan_args(os(&[
            "--resume",
            "abc-123",
            "--model",
            "sonnet",
        ]));
        assert_eq!(plan.session_id.as_deref(), Some("abc-123"));
    }

    #[test]
    fn passthrough_flags_dominate_resume_with_id() {
        // If the user supplies both --resume <id> and --continue, we
        // can't coordinate — claude's behavior with both flags is
        // ambiguous, so passthrough is the safe choice.
        let plan = plan_args(os(&["--resume", "abc-123", "-c"]));
        assert!(plan.session_id.is_none());
    }

    #[test]
    fn unrelated_flags_dont_disturb_id_generation() {
        let plan = plan_args(os(&["--model", "sonnet", "--effort", "high"]));
        assert!(plan.session_id.is_some());
        // forwarded should be original args + --session-id <generated>
        assert_eq!(plan.forwarded.len(), 6);
        assert_eq!(plan.forwarded[0], OsString::from("--model"));
        assert_eq!(plan.forwarded[4], OsString::from("--session-id"));
    }
}
