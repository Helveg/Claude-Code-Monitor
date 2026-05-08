//! Direct passthrough — used when the user invoked claude with one of
//! the picker / continue / fork / from-pr flags where we don't know the
//! session id ahead of time. We can't multiplex without a known id, so
//! we just spawn real claude with inherited stdio and forward the exit
//! code. No registry registration, no per-session pipe.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

pub fn run(real: PathBuf, args: Vec<OsString>) -> ExitCode {
    let is_exe = real
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.eq_ignore_ascii_case("exe"))
        .unwrap_or(false);

    let mut cmd = if is_exe {
        Command::new(&real)
    } else {
        // .cmd / .bat shims need cmd.exe in front (CreateProcessW can't
        // launch them directly).
        let mut c = Command::new("cmd.exe");
        c.arg("/c").arg(&real);
        c
    };
    cmd.args(&args);

    match cmd.status() {
        Ok(s) => ExitCode::from(s.code().unwrap_or(1).clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("claude (shim): failed to launch real claude: {e}");
            ExitCode::from(127)
        }
    }
}
