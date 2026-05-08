//! Console mode + ConPTY helpers used by the owner and subscriber loops.
//! Kept here so each mode reads as a sequence of bridge / accept / fan-out
//! decisions rather than Win32 boilerplate.

use std::ffi::c_void;
use std::mem;
use std::path::Path;
use std::ptr;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE, TRUE,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, GetConsoleCP, GetConsoleMode,
    GetConsoleOutputCP, GetConsoleScreenBufferInfo, GetStdHandle, ResizePseudoConsole,
    SetConsoleCP, SetConsoleMode, SetConsoleOutputCP, CONSOLE_MODE,
    CONSOLE_SCREEN_BUFFER_INFO, COORD, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
    ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING, ENABLE_WRAP_AT_EOL_OUTPUT, HPCON, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess,
    InitializeProcThreadAttributeList, UpdateProcThreadAttribute, EXTENDED_STARTUPINFO_PRESENT,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, STARTUPINFOEXW,
};

/// PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE — same magic value `terminal.rs` uses.
const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;

pub struct ShimPty {
    pub hpc: HPCON,
    pub h_in_write: HANDLE,
    pub h_out_read: HANDLE,
    pub h_process: HANDLE,
}

/// Spawn `cmd` attached to a fresh ConPTY of size `cols x rows`. Returns
/// the handles the caller needs to fan out output, feed input, resize,
/// and wait for child exit.
pub fn spawn_pty(
    cols: u16,
    rows: u16,
    cmd: &str,
    cwd: Option<&Path>,
) -> Result<ShimPty, String> {
    unsafe {
        let sa = SECURITY_ATTRIBUTES {
            nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: TRUE,
        };
        let sa_ptr: *const SECURITY_ATTRIBUTES = &sa;

        let mut h_pty_in_read = HANDLE::default();
        let mut h_pty_in_write = HANDLE::default();
        let mut h_pty_out_read = HANDLE::default();
        let mut h_pty_out_write = HANDLE::default();

        if CreatePipe(&mut h_pty_in_read, &mut h_pty_in_write, Some(sa_ptr), 0).is_err() {
            return Err("CreatePipe (in) failed".into());
        }
        if CreatePipe(&mut h_pty_out_read, &mut h_pty_out_write, Some(sa_ptr), 0).is_err() {
            let _ = CloseHandle(h_pty_in_read);
            let _ = CloseHandle(h_pty_in_write);
            return Err("CreatePipe (out) failed".into());
        }

        let size = COORD {
            X: cols.max(1) as i16,
            Y: rows.max(1) as i16,
        };
        let hpc = match CreatePseudoConsole(size, h_pty_in_read, h_pty_out_write, 0) {
            Ok(h) => h,
            Err(e) => {
                let _ = CloseHandle(h_pty_in_read);
                let _ = CloseHandle(h_pty_out_write);
                let _ = CloseHandle(h_pty_in_write);
                let _ = CloseHandle(h_pty_out_read);
                return Err(format!("CreatePseudoConsole failed: {e}"));
            }
        };
        // ConPTY duplicates the handles internally; close our refs.
        let _ = CloseHandle(h_pty_in_read);
        let _ = CloseHandle(h_pty_out_write);

        let mut attr_size: usize = 0;
        let _ = InitializeProcThreadAttributeList(
            LPPROC_THREAD_ATTRIBUTE_LIST(ptr::null_mut()),
            1,
            0,
            &mut attr_size,
        );
        if attr_size == 0 {
            ClosePseudoConsole(hpc);
            let _ = CloseHandle(h_pty_in_write);
            let _ = CloseHandle(h_pty_out_read);
            return Err("attr list sizing failed".into());
        }
        let mut attr_buf: Vec<u8> = vec![0u8; attr_size];
        let attr_list = LPPROC_THREAD_ATTRIBUTE_LIST(attr_buf.as_mut_ptr() as *mut _);
        if InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size).is_err() {
            ClosePseudoConsole(hpc);
            let _ = CloseHandle(h_pty_in_write);
            let _ = CloseHandle(h_pty_out_read);
            return Err("attr list init failed".into());
        }
        if UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
            Some(hpc.0 as *const c_void),
            mem::size_of::<HPCON>(),
            None,
            None,
        )
        .is_err()
        {
            DeleteProcThreadAttributeList(attr_list);
            ClosePseudoConsole(hpc);
            let _ = CloseHandle(h_pty_in_write);
            let _ = CloseHandle(h_pty_out_read);
            return Err("UpdateProcThreadAttribute failed".into());
        }

        let mut si: STARTUPINFOEXW = mem::zeroed();
        si.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = attr_list;

        let mut cmd_wide: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
        let cwd_wide: Option<Vec<u16>> = cwd.map(|p| {
            p.to_string_lossy()
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect()
        });
        let cwd_ptr = match &cwd_wide {
            Some(v) => PCWSTR::from_raw(v.as_ptr()),
            None => PCWSTR::null(),
        };
        let mut pi = PROCESS_INFORMATION::default();

        let ok = CreateProcessW(
            PCWSTR::null(),
            PWSTR::from_raw(cmd_wide.as_mut_ptr()),
            None,
            None,
            false,
            EXTENDED_STARTUPINFO_PRESENT,
            None,
            cwd_ptr,
            &si.StartupInfo,
            &mut pi,
        )
        .is_ok();

        DeleteProcThreadAttributeList(attr_list);

        if !ok {
            ClosePseudoConsole(hpc);
            let _ = CloseHandle(h_pty_in_write);
            let _ = CloseHandle(h_pty_out_read);
            return Err("CreateProcessW failed".into());
        }
        let _ = CloseHandle(pi.hThread);

        Ok(ShimPty {
            hpc,
            h_in_write: h_pty_in_write,
            h_out_read: h_pty_out_read,
            h_process: pi.hProcess,
        })
    }
}

pub fn resize_pty(hpc: HPCON, cols: u16, rows: u16) {
    unsafe {
        let _ = ResizePseudoConsole(
            hpc,
            COORD {
                X: cols.max(1) as i16,
                Y: rows.max(1) as i16,
            },
        );
    }
}

/// Address-based resize helper. HPCON is `pub struct HPCON(pub isize)` —
/// the inner field isn't a Send pointer but we still ferry it across
/// threads as a plain isize for symmetry with the HANDLE pattern.
pub fn resize_pty_by_addr(hpc_addr: isize, cols: u16, rows: u16) {
    resize_pty(HPCON(hpc_addr), cols, rows);
}

pub fn close_pty(pty: ShimPty) {
    unsafe {
        ClosePseudoConsole(pty.hpc);
        let _ = CloseHandle(pty.h_in_write);
        let _ = CloseHandle(pty.h_out_read);
        let _ = CloseHandle(pty.h_process);
    }
}

/// Read the current console buffer size from stdout. Falls back to a
/// reasonable default when stdout isn't a real console (e.g. shim spawned
/// inside another shim's ConPTY).
pub fn current_console_size() -> (u16, u16) {
    unsafe {
        let h = match GetStdHandle(STD_OUTPUT_HANDLE) {
            Ok(h) if !h.is_invalid() => h,
            _ => return (120, 30),
        };
        let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
        if GetConsoleScreenBufferInfo(h, &mut info).is_err() {
            return (120, 30);
        }
        let cols = (info.srWindow.Right - info.srWindow.Left + 1).max(1) as u16;
        let rows = (info.srWindow.Bottom - info.srWindow.Top + 1).max(1) as u16;
        (cols, rows)
    }
}

/// Save current stdin/stdout console modes + codepages and switch to
/// "raw + VT + UTF-8". The guard restores everything on Drop, including
/// on panic. Best effort — failures are silently ignored so the shim
/// still works in non-console contexts (e.g. spawned by a tool that
/// piped its handles).
///
/// Why UTF-8: claude's TUI emits box-drawing chars and other non-ASCII
/// glyphs as multi-byte UTF-8. The default Windows console codepage
/// (CP-1252 or local ANSI) interprets each byte as a separate char,
/// producing mojibake. We have to tell the console "these bytes are
/// UTF-8" before any of claude's output is meaningful.
pub struct ConsoleModeGuard {
    pub stdin: HANDLE,
    pub stdout: HANDLE,
    saved_in: Option<u32>,
    saved_out: Option<u32>,
    saved_input_cp: Option<u32>,
    saved_output_cp: Option<u32>,
}

impl ConsoleModeGuard {
    pub fn install() -> Self {
        unsafe {
            let stdin = GetStdHandle(STD_INPUT_HANDLE).unwrap_or_default();
            let stdout = GetStdHandle(STD_OUTPUT_HANDLE).unwrap_or_default();

            let saved_in = read_mode(stdin);
            let saved_out = read_mode(stdout);
            let saved_input_cp = nonzero(GetConsoleCP());
            let saved_output_cp = nonzero(GetConsoleOutputCP());

            // UTF-8 codepages: matches what claude (and modern Windows
            // Terminal / wt) expects for I/O.
            let _ = SetConsoleCP(65001);
            let _ = SetConsoleOutputCP(65001);

            if let Some(orig) = saved_in {
                // Raw-ish stdin: LINE_INPUT off so ReadFile returns each
                // keystroke immediately; ECHO_INPUT off so claude's TUI
                // controls echo. PROCESSED_INPUT stays ON because
                // ENABLE_VIRTUAL_TERMINAL_INPUT requires it on Windows
                // (and it's what most TUI apps assume). Ctrl+C will be
                // intercepted by the console host — we accept that
                // for now; claude's TUI handles its own quit affordance.
                let new = (orig & !(ENABLE_LINE_INPUT.0 | ENABLE_ECHO_INPUT.0))
                    | ENABLE_VIRTUAL_TERMINAL_INPUT.0
                    | ENABLE_PROCESSED_INPUT.0;
                let _ = SetConsoleMode(stdin, CONSOLE_MODE(new));
            }
            if let Some(orig) = saved_out {
                let new = orig
                    | ENABLE_VIRTUAL_TERMINAL_PROCESSING.0
                    | ENABLE_PROCESSED_OUTPUT.0
                    | ENABLE_WRAP_AT_EOL_OUTPUT.0;
                let _ = SetConsoleMode(stdout, CONSOLE_MODE(new));
            }

            ConsoleModeGuard {
                stdin,
                stdout,
                saved_in,
                saved_out,
                saved_input_cp,
                saved_output_cp,
            }
        }
    }
}

impl Drop for ConsoleModeGuard {
    fn drop(&mut self) {
        unsafe {
            if let Some(m) = self.saved_in {
                let _ = SetConsoleMode(self.stdin, CONSOLE_MODE(m));
            }
            if let Some(m) = self.saved_out {
                let _ = SetConsoleMode(self.stdout, CONSOLE_MODE(m));
            }
            if let Some(cp) = self.saved_input_cp {
                let _ = SetConsoleCP(cp);
            }
            if let Some(cp) = self.saved_output_cp {
                let _ = SetConsoleOutputCP(cp);
            }
        }
    }
}

fn nonzero(v: u32) -> Option<u32> {
    if v == 0 {
        None
    } else {
        Some(v)
    }
}

/// Duplicate a HANDLE in the current process. Used so each pipe-using
/// thread gets its own handle — Windows synchronous-mode pipe I/O is
/// sequenced per handle, so reader and writer threads sharing one
/// handle deadlock when the reader's blocked waiting for data the
/// writer is trying to send. Different handles, different I/O slots,
/// no deadlock.
pub fn duplicate_handle(h: HANDLE) -> Option<HANDLE> {
    let mut h2 = HANDLE::default();
    unsafe {
        let proc = GetCurrentProcess();
        if DuplicateHandle(proc, h, proc, &mut h2, 0, false, DUPLICATE_SAME_ACCESS).is_err() {
            return None;
        }
    }
    if h2.is_invalid() {
        None
    } else {
        Some(h2)
    }
}

/// Append a one-line debug record to `%TEMP%\ccmonitor-shim.log`. Used
/// to diagnose hangs in owner/subscriber where stdout is hijacked by
/// claude's TUI and stderr would corrupt the alt-screen.
pub fn shim_log(msg: impl AsRef<str>) {
    let path = std::env::temp_dir().join("ccmonitor-shim.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let pid = std::process::id();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = writeln!(f, "[{now} pid={pid}] {}", msg.as_ref());
    }
}

fn read_mode(h: HANDLE) -> Option<u32> {
    if h.is_invalid() {
        return None;
    }
    let mut mode = CONSOLE_MODE(0);
    if unsafe { GetConsoleMode(h, &mut mode) }.is_err() {
        return None;
    }
    Some(mode.0)
}
