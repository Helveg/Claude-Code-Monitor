//! Shim binary installed as `claude.exe` ahead of the real claude on
//! PATH. Three modes:
//!
//!   * **owner**       — generates a UUID (or reuses an explicit one),
//!     spawns real claude inside a ConPTY, and serves a per-session
//!     pipe so other shims (or the manager) can subscribe.
//!   * **subscriber**  — when the per-session pipe already exists,
//!     becomes a relay: ships the local console's keystrokes upstream
//!     and renders the owner's output locally.
//!   * **passthrough** — fallback for picker/continue/fork flows where
//!     we don't know the session id ahead of time. Spawns real claude
//!     with inherited stdio and waits.
//!
//! Argv handling mirrors the user's intent:
//!   * `--resume <uuid>` (explicit id), or `--session-id <uuid>`: use
//!     that id for coordination.
//!   * `--resume` with no id, `--continue`, `--fork-session`,
//!     `--from-pr`: passthrough — claude itself decides the id and we
//!     have no way to learn it.
//!   * none of the above: generate a UUID, append `--session-id <uuid>`
//!     to the args.

use std::ffi::OsString;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use claude_manager::{real_claude, shim};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE, OPEN_EXISTING,
};

fn main() -> ExitCode {
    let raw: Vec<OsString> = std::env::args_os().skip(1).collect();
    let plan = shim::plan::plan_args(raw);

    let me = std::env::current_exe().ok();
    let Some(real) = real_claude::resolve(me.as_deref()) else {
        eprintln!("claude (shim): real claude not found on PATH");
        return ExitCode::from(127);
    };

    match plan.session_id {
        None => shim::passthrough::run(real, plan.forwarded),
        Some(id) => {
            // Briefly try to attach to an existing owner. The window is
            // tight (50ms) — if no pipe responds, become owner ourselves.
            let pipe_name = shim::protocol::session_pipe_name(&id);
            match try_subscribe(&pipe_name, Duration::from_millis(50)) {
                Some(h) => shim::subscriber::run(h),
                None => shim::owner::run(id, plan.forwarded, real),
            }
        }
    }
}

fn try_subscribe(pipe_name: &str, timeout: Duration) -> Option<HANDLE> {
    let name: Vec<u16> = pipe_name.encode_utf16().chain(std::iter::once(0)).collect();
    let started = Instant::now();
    loop {
        let res = unsafe {
            CreateFileW(
                PCWSTR::from_raw(name.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        };
        if let Ok(h) = res {
            if !h.is_invalid() {
                return Some(h);
            }
        }
        if started.elapsed() >= timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
