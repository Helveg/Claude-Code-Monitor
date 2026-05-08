//! Owner mode — the shim spawned real claude inside a ConPTY and
//! becomes the multiplexer for everyone watching that session.
//!
//! Threads:
//!   * `pty_out_fanout`     — reads claude's PTY output, writes a copy
//!     to the local stdout AND ships an `O` frame to every connected
//!     subscriber pipe.
//!   * `local_stdin_to_pty` — reads local stdin, writes to claude's PTY
//!     stdin (under a mutex shared with the subscriber input forwarders
//!     so writes don't interleave mid-escape-sequence).
//!   * `local_size_watcher` — polls the local console size every 500 ms
//!     and triggers a resize-merge if it changed.
//!   * `accept_loop`        — keeps creating fresh instances of
//!     `\\.\pipe\ccmonitor-session-<uuid>` and spawns a per-subscriber
//!     handler thread for each connection.
//!   * one `subscriber_handler` per connected client — parses incoming
//!     `H`/`I`/`R` frames, forwards `I` payloads to the PTY-in mutex,
//!     updates the size-merge map on `H`/`R`, and on disconnect removes
//!     itself from the subscriber map.
//!
//! Resize merge: the PTY size is the per-axis minimum across the local
//! console plus every connected subscriber. tmux uses the same rule —
//! everyone sees a frame that fits in the smallest viewer. When a
//! subscriber joins or leaves, or any party resizes, we recompute and
//! issue `ResizePseudoConsole`.
//!
//! Lifecycle: when claude exits, `WaitForSingleObject` returns on the
//! main thread. We close every subscriber pipe (their handlers' reads
//! return 0 → they detach), close the PTY, and return the child's
//! exit code. Detached threads (stdin reader, accept loop) are reaped
//! by the OS as the process exits.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PeekNamedPipe, PIPE_READMODE_BYTE,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::Foundation::BOOL;
use windows::Win32::System::Console::{FreeConsole, SetConsoleCtrlHandler};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, TerminateProcess, WaitForSingleObject, INFINITE,
};

use crate::claude;
use crate::registry;
use crate::shim::protocol::{
    self, FrameReader, TAG_HELLO, TAG_INPUT, TAG_METADATA, TAG_OUTPUT, TAG_QUERY, TAG_RESIZE,
};
use crate::shim::util::{
    self, close_pty, current_console_size, spawn_pty, ConsoleModeGuard,
};

/// Per-connection record. The pipe handle itself lives inside the
/// subscriber's own thread (we go single-thread-per-pipe to avoid
/// Windows' per-pipe sync I/O serialization deadlock). The map only
/// holds the channel sender that pty-out fanout sends through, plus
/// the size used by the resize-merge.
struct Subscriber {
    size: (u16, u16),
    out_tx: Sender<Vec<u8>>,
}

/// Static metadata about the session. Captured once at owner startup
/// and shared with every subscriber-thread so it can answer `Q`
/// metadata queries without re-reading process state mid-flight.
#[derive(Clone)]
struct SessionMeta {
    session_id: String,
    cwd: String,
    pid: u32,
    started_at: u128,
}

/// Set to false when this process receives a console close/break event
/// (user X'd out the window or hit Ctrl+Break). The lifecycle watcher
/// uses it as the "is the original terminal still attached?" signal —
/// when it flips to false AND the subscriber map is empty, claude is
/// terminated and the owner exits.
///
/// Static because `SetConsoleCtrlHandler` takes a plain `extern fn` with
/// no closure, so the handler can't carry state through a captured
/// reference. There's exactly one owner per process anyway.
static TERMINAL_ALIVE: AtomicBool = AtomicBool::new(true);

unsafe extern "system" fn console_ctrl_handler(ctrl_type: u32) -> BOOL {
    // CTRL_C_EVENT (0), CTRL_BREAK_EVENT (1), CTRL_CLOSE_EVENT (2),
    // CTRL_LOGOFF_EVENT (5), CTRL_SHUTDOWN_EVENT (6) — we treat all
    // close-ish signals as "terminal is going away". Ctrl+C should
    // pass through to claude (we don't claim it here) so we explicitly
    // skip 0.
    if matches!(ctrl_type, 1 | 2 | 5 | 6) {
        TERMINAL_ALIVE.store(false, Ordering::Release);
        // Detach from the (about-to-die) console so the OS doesn't
        // force-terminate us 5 seconds later. Without FreeConsole, the
        // shim can't outlive a `CTRL_CLOSE_EVENT` long enough to keep
        // serving subscribers — Windows force-kills any console
        // process whose console window goes away.
        let _ = FreeConsole();
        // Return TRUE = handled. The lifecycle watcher decides whether
        // to terminate claude (no subscribers left) or keep running
        // (subscribers still attached).
        BOOL(1)
    } else {
        BOOL(0)
    }
}

pub fn run(session_id: String, args: Vec<OsString>, real: PathBuf) -> ExitCode {
    util::shim_log(format!("owner::run session_id={session_id}"));
    let console = ConsoleModeGuard::install();
    let stdin = console.stdin;
    let stdout = console.stdout;

    let initial_size = current_console_size();
    let cmd_line = build_cmd_line(&real, &args);
    let pty = match spawn_pty(initial_size.0, initial_size.1, &cmd_line, None) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("claude (shim): spawn_pty failed: {e}");
            return ExitCode::from(127);
        }
    };

    let meta = std::sync::Arc::new(SessionMeta {
        session_id: session_id.clone(),
        cwd: std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        pid: std::process::id(),
        started_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    });

    // Install the close-event handler before any threads spawn so a
    // race-fast window-close still flips the flag.
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(console_ctrl_handler), true);
    }
    TERMINAL_ALIVE.store(true, Ordering::Release);

    // Background thread keeps a heartbeat connection to the manager's
    // registry pipe so the dashboard can list this session.
    register_with_manager(session_id.clone());

    let subscribers: Arc<Mutex<HashMap<u64, Subscriber>>> = Arc::new(Mutex::new(HashMap::new()));
    let local_size = Arc::new(Mutex::new(initial_size));
    let next_id = Arc::new(AtomicU64::new(1));
    let pty_in_lock: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
    let claude_alive = Arc::new(AtomicBool::new(true));

    // Addresses for closures (HANDLE/HPCON aren't Send).
    let h_out_addr = pty.h_out_read.0 as isize;
    let h_in_addr = pty.h_in_write.0 as isize;
    let stdout_addr = stdout.0 as isize;
    let stdin_addr = stdin.0 as isize;
    let hpc_addr = pty.hpc.0 as isize;

    // PTY-out fan-out.
    {
        let subs = subscribers.clone();
        thread::spawn(move || {
            pty_out_fanout(
                HANDLE(h_out_addr as *mut _),
                HANDLE(stdout_addr as *mut _),
                subs,
            );
        });
    }

    // Local stdin → PTY in.
    {
        let lock = pty_in_lock.clone();
        let alive = claude_alive.clone();
        thread::spawn(move || {
            local_stdin_to_pty(
                HANDLE(stdin_addr as *mut _),
                HANDLE(h_in_addr as *mut _),
                lock,
                alive,
            );
        });
    }

    // Subscriber accept loop + per-connection handlers.
    {
        let subs = subscribers.clone();
        let lock = pty_in_lock.clone();
        let local_sz = local_size.clone();
        let nid = next_id.clone();
        let meta = meta.clone();
        thread::spawn(move || {
            accept_loop(meta, subs, lock, local_sz, nid, h_in_addr, hpc_addr);
        });
    }

    // Local size watcher → recomputes merge on change.
    {
        let subs = subscribers.clone();
        let local_sz = local_size.clone();
        let alive = claude_alive.clone();
        thread::spawn(move || {
            local_size_watcher(local_sz, subs, hpc_addr, alive);
        });
    }

    // Lifecycle watcher: terminates claude when refcount hits zero
    // (no terminal AND no subscribers). The original terminal counts
    // as an implicit subscriber via TERMINAL_ALIVE; CTRL_CLOSE_EVENT
    // flips it. So claude only outlives its original window when
    // somebody else is still attached.
    {
        let subs = subscribers.clone();
        let claude_addr = pty.h_process.0 as isize;
        let alive = claude_alive.clone();
        thread::spawn(move || {
            lifecycle_watcher(claude_addr, subs, alive);
        });
    }

    // Wait for claude to exit.
    let mut exit_code: u32 = 1;
    unsafe {
        WaitForSingleObject(pty.h_process, INFINITE);
        let _ = GetExitCodeProcess(pty.h_process, &mut exit_code);
    }
    claude_alive.store(false, Ordering::Release);

    // Clearing the map drops every Subscriber's `out_tx`, which makes
    // each subscriber_thread's `try_recv` return Disconnected on its
    // next iteration; they then close their own pipe handles. We don't
    // close the handles here because the subscriber_thread owns them.
    {
        let mut subs = subscribers.lock().unwrap_or_else(|p| p.into_inner());
        subs.clear();
    }

    close_pty(pty);
    drop(console); // restore console modes before exiting

    ExitCode::from((exit_code as i32).clamp(0, 255) as u8)
}

fn build_cmd_line(real: &PathBuf, args: &[OsString]) -> String {
    let real_str = real.to_string_lossy().into_owned();
    let is_exe = real
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.eq_ignore_ascii_case("exe"))
        .unwrap_or(false);

    let mut parts: Vec<String> = if is_exe {
        vec![real_str]
    } else {
        // .cmd / .bat shims need cmd.exe in front.
        vec!["cmd.exe".into(), "/c".into(), real_str]
    };
    for a in args {
        parts.push(a.to_string_lossy().into_owned());
    }
    parts
        .into_iter()
        .map(claude::quote_arg)
        .collect::<Vec<_>>()
        .join(" ")
}

fn pty_out_fanout(
    h_out: HANDLE,
    stdout: HANDLE,
    subscribers: Arc<Mutex<HashMap<u64, Subscriber>>>,
) {
    let mut buf = [0u8; 4096];
    loop {
        let mut n: u32 = 0;
        let ok = unsafe { ReadFile(h_out, Some(&mut buf[..]), Some(&mut n), None) };
        if ok.is_err() || n == 0 {
            return;
        }
        let chunk = &buf[..n as usize];
        let _ = write_all_h(stdout, chunk);

        // Build a full O frame once and send a clone to every
        // subscriber's outbound channel. The actual blocking pipe write
        // happens in each subscriber's writer thread, so a slow
        // subscriber doesn't stall the fan-out (and therefore doesn't
        // stall claude's stdin via PTY backpressure).
        let mut frame = Vec::with_capacity(5 + chunk.len());
        frame.extend_from_slice(&protocol::frame_header(TAG_OUTPUT, n));
        frame.extend_from_slice(chunk);

        let snapshot: Vec<(u64, Sender<Vec<u8>>)> = {
            let g = subscribers.lock().unwrap_or_else(|p| p.into_inner());
            g.iter().map(|(id, s)| (*id, s.out_tx.clone())).collect()
        };
        let mut dead: Vec<u64> = Vec::new();
        for (id, tx) in snapshot {
            if tx.send(frame.clone()).is_err() {
                dead.push(id);
            }
        }
        if !dead.is_empty() {
            // Remove dead entries from the map. The subscriber_thread for
            // each will also notice via its own try_recv → Disconnected
            // and tear down its own pipe handle.
            let mut g = subscribers.lock().unwrap_or_else(|p| p.into_inner());
            for id in dead {
                g.remove(&id);
            }
        }
    }
}

fn local_stdin_to_pty(
    stdin: HANDLE,
    pty_in: HANDLE,
    pty_in_lock: Arc<Mutex<()>>,
    claude_alive: Arc<AtomicBool>,
) {
    let mut buf = [0u8; 1024];
    while claude_alive.load(Ordering::Acquire) {
        let mut n: u32 = 0;
        let ok = unsafe { ReadFile(stdin, Some(&mut buf[..]), Some(&mut n), None) };
        if ok.is_err() || n == 0 {
            // Stdin EOF means our console is gone — either the user X'd
            // out their terminal (which usually fires CTRL_CLOSE_EVENT
            // first, but not always), or a parent process closed the
            // ConPTY we were spawned into (manager panel closing). The
            // CTRL_CLOSE_EVENT path may not fire for ConPTY-attached
            // children, so we use stdin-EOF as a second signal that
            // flips the same flag — the lifecycle watcher then decides
            // whether to terminate claude or keep serving subscribers.
            TERMINAL_ALIVE.store(false, Ordering::Release);
            return;
        }
        if !claude_alive.load(Ordering::Acquire) {
            return;
        }
        let _g = pty_in_lock.lock();
        let _ = write_all_h(pty_in, &buf[..n as usize]);
    }
}

fn accept_loop(
    meta: Arc<SessionMeta>,
    subscribers: Arc<Mutex<HashMap<u64, Subscriber>>>,
    pty_in_lock: Arc<Mutex<()>>,
    local_size: Arc<Mutex<(u16, u16)>>,
    next_id: Arc<AtomicU64>,
    h_in_addr: isize,
    hpc_addr: isize,
) {
    let pipe_path = protocol::session_pipe_name(&meta.session_id);
    let mut name_w: Vec<u16> = pipe_path.encode_utf16().chain(std::iter::once(0)).collect();
    loop {
        let h = unsafe {
            CreateNamedPipeW(
                PCWSTR::from_raw(name_w.as_mut_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                65_536,
                65_536,
                0,
                None,
            )
        };
        if h.is_invalid() {
            thread::sleep(Duration::from_millis(500));
            continue;
        }
        let connected = unsafe { ConnectNamedPipe(h, None) };
        let benign = match connected.as_ref() {
            Ok(()) => true,
            Err(e) => e.code().0 as u32 == ERROR_PIPE_CONNECTED.0,
        };
        if !benign {
            unsafe {
                let _ = CloseHandle(h);
            }
            continue;
        }

        let pipe_addr = h.0 as isize;
        let id = next_id.fetch_add(1, Ordering::Relaxed);

        let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
        {
            let mut g = subscribers.lock().unwrap_or_else(|p| p.into_inner());
            g.insert(
                id,
                Subscriber {
                    size: (0, 0),
                    out_tx,
                },
            );
        }

        // ONE thread per subscriber doing both directions. Windows
        // synchronous-mode named pipe I/O is serialized per pipe (not
        // per handle — DuplicateHandle doesn't help), so two threads
        // doing concurrent ReadFile/WriteFile on the same pipe deadlock.
        // We multiplex via PeekNamedPipe (non-blocking incoming check)
        // + try_recv (non-blocking outgoing check), with a small sleep
        // when both directions are quiet.
        let subs = subscribers.clone();
        let lock = pty_in_lock.clone();
        let local_sz = local_size.clone();
        let meta = meta.clone();
        thread::spawn(move || {
            subscriber_thread(
                id, pipe_addr, out_rx, subs, lock, local_sz, h_in_addr, hpc_addr, meta,
            );
        });
    }
}

fn subscriber_thread(
    id: u64,
    pipe_addr: isize,
    out_rx: Receiver<Vec<u8>>,
    subscribers: Arc<Mutex<HashMap<u64, Subscriber>>>,
    pty_in_lock: Arc<Mutex<()>>,
    local_size: Arc<Mutex<(u16, u16)>>,
    h_in_addr: isize,
    hpc_addr: isize,
    meta: Arc<SessionMeta>,
) {
    util::shim_log(format!("owner: subscriber-thread {id} started"));
    let pipe = HANDLE(pipe_addr as *mut _);
    let pty_in = HANDLE(h_in_addr as *mut _);
    let mut reader = FrameReader::new();
    let mut buf = [0u8; 4096];
    'outer: loop {
        let mut did_work = false;

        // Outbound: drain one frame from the channel if there is one.
        match out_rx.try_recv() {
            Ok(frame) => {
                if !write_all_h(pipe, &frame) {
                    util::shim_log(format!(
                        "owner: subscriber-thread {id} outbound WriteFile failed; closing"
                    ));
                    break 'outer;
                }
                did_work = true;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                // Last sender (the map entry) was dropped; we're done.
                break 'outer;
            }
        }

        // Inbound: peek for available bytes, then read once if any.
        let mut avail: u32 = 0;
        let peek_ok = unsafe {
            PeekNamedPipe(pipe, None, 0, None, Some(&mut avail), None)
        };
        if peek_ok.is_err() {
            util::shim_log(format!(
                "owner: subscriber-thread {id} PeekNamedPipe err; closing"
            ));
            break 'outer;
        }
        if avail > 0 {
            let mut n: u32 = 0;
            let ok = unsafe { ReadFile(pipe, Some(&mut buf[..]), Some(&mut n), None) };
            if ok.is_err() || n == 0 {
                break 'outer;
            }
            reader.feed(&buf[..n as usize]);
            while let Some((tag, payload)) = reader.next_frame() {
                match tag {
                    TAG_INPUT => {
                        let _g = pty_in_lock.lock();
                        let _ = write_all_h(pty_in, &payload);
                    }
                    TAG_QUERY => {
                        // JSON request → JSON reply. Owner sends `M`
                        // back through the outbound channel so it
                        // serializes with any in-flight `O` frames on
                        // this connection. Unknown ops get a JSON
                        // error reply rather than silently closing.
                        let req: serde_json::Value = serde_json::from_slice(&payload)
                            .unwrap_or_else(|_| serde_json::json!({"op": "?"}));
                        let op = req.get("op").and_then(|v| v.as_str()).unwrap_or("");
                        let reply = match op {
                            "metadata" => serde_json::json!({
                                "session_id": meta.session_id,
                                "cwd": meta.cwd,
                                "pid": meta.pid,
                                "started_at": meta.started_at,
                            }),
                            other => serde_json::json!({
                                "error": format!("unknown op: {other}"),
                            }),
                        };
                        let body = reply.to_string();
                        let mut frame = Vec::with_capacity(5 + body.len());
                        frame.extend_from_slice(&protocol::frame_header(
                            TAG_METADATA,
                            body.len() as u32,
                        ));
                        frame.extend_from_slice(body.as_bytes());
                        let tx = {
                            let g =
                                subscribers.lock().unwrap_or_else(|p| p.into_inner());
                            g.get(&id).map(|s| s.out_tx.clone())
                        };
                        if let Some(tx) = tx {
                            let _ = tx.send(frame);
                        }
                    }
                    TAG_HELLO | TAG_RESIZE => {
                        if let Some(sz) = protocol::decode_size_payload(&payload) {
                            let was_hello = tag == TAG_HELLO;
                            let mut g =
                                subscribers.lock().unwrap_or_else(|p| p.into_inner());
                            if let Some(sub) = g.get_mut(&id) {
                                sub.size = sz;
                            }
                            let merged = merge_size_locked(
                                &g,
                                *local_size.lock().unwrap_or_else(|p| p.into_inner()),
                            );
                            drop(g);
                            if was_hello {
                                // Send a synthetic prelude (alt-screen +
                                // clear + cursor home) directly via the
                                // pipe — we're already the only thread
                                // writing here.
                                let prelude = b"\x1b[?1049h\x1b[2J\x1b[H";
                                let mut frame = Vec::with_capacity(5 + prelude.len());
                                frame.extend_from_slice(&protocol::frame_header(
                                    TAG_OUTPUT,
                                    prelude.len() as u32,
                                ));
                                frame.extend_from_slice(prelude);
                                if !write_all_h(pipe, &frame) {
                                    break 'outer;
                                }
                            }
                            if let Some((c, r)) = merged {
                                // Pulse the PTY size so claude's TUI
                                // re-renders. For HELLO the pulse is the
                                // only way the new subscriber's screen
                                // gets populated; for RESIZE it just
                                // propagates the new dimensions.
                                if was_hello && r > 1 {
                                    util::resize_pty_by_addr(hpc_addr, c, r - 1);
                                    thread::sleep(Duration::from_millis(50));
                                }
                                util::resize_pty_by_addr(hpc_addr, c, r);
                            }
                        }
                    }
                    _ => {}
                }
            }
            did_work = true;
        }

        if !did_work {
            thread::sleep(Duration::from_millis(5));
        }
    }
    util::shim_log(format!("owner: subscriber-thread {id} exiting"));
    // Remove ourselves from the map and tear down the pipe.
    let mut g = subscribers.lock().unwrap_or_else(|p| p.into_inner());
    g.remove(&id);
    let merged =
        merge_size_locked(&g, *local_size.lock().unwrap_or_else(|p| p.into_inner()));
    drop(g);
    unsafe {
        let _ = DisconnectNamedPipe(pipe);
        let _ = CloseHandle(pipe);
    }
    if let Some((c, r)) = merged {
        util::resize_pty_by_addr(hpc_addr, c, r);
    }
}

fn lifecycle_watcher(
    claude_handle_addr: isize,
    subscribers: Arc<Mutex<HashMap<u64, Subscriber>>>,
    claude_alive: Arc<AtomicBool>,
) {
    // Poll every 100ms. When TERMINAL_ALIVE is false AND there are no
    // subscribers, terminate claude. Main thread's WaitForSingleObject
    // will return; cleanup proceeds normally.
    loop {
        if !claude_alive.load(Ordering::Acquire) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
        if !TERMINAL_ALIVE.load(Ordering::Acquire) {
            let count = subscribers
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .len();
            if count == 0 {
                util::shim_log(
                    "owner: lifecycle terminating claude (terminal closed, 0 subscribers)",
                );
                let h = HANDLE(claude_handle_addr as *mut _);
                unsafe {
                    let _ = TerminateProcess(h, 1);
                }
                return;
            }
        }
    }
}

fn local_size_watcher(
    local_size: Arc<Mutex<(u16, u16)>>,
    subscribers: Arc<Mutex<HashMap<u64, Subscriber>>>,
    hpc_addr: isize,
    claude_alive: Arc<AtomicBool>,
) {
    let mut last = current_console_size();
    while claude_alive.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(500));
        if !claude_alive.load(Ordering::Acquire) {
            break;
        }
        let cur = current_console_size();
        if cur == last {
            continue;
        }
        last = cur;
        {
            let mut g = local_size.lock().unwrap_or_else(|p| p.into_inner());
            *g = cur;
        }
        let g = subscribers.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((c, r)) = merge_size_locked(&g, cur) {
            util::resize_pty_by_addr(hpc_addr, c, r);
        }
    }
}

fn merge_size_locked(
    subs: &HashMap<u64, Subscriber>,
    local: (u16, u16),
) -> Option<(u16, u16)> {
    let mut cols = local.0;
    let mut rows = local.1;
    for s in subs.values() {
        if s.size.0 > 0 {
            cols = cols.min(s.size.0);
        }
        if s.size.1 > 0 {
            rows = rows.min(s.size.1);
        }
    }
    if cols == 0 || rows == 0 {
        None
    } else {
        Some((cols, rows))
    }
}

fn write_all_h(h: HANDLE, mut data: &[u8]) -> bool {
    while !data.is_empty() {
        let mut n: u32 = 0;
        let ok = unsafe { WriteFile(h, Some(data), Some(&mut n), None) };
        if ok.is_err() || n == 0 {
            return false;
        }
        data = &data[n as usize..];
    }
    true
}

fn register_with_manager(session_id: String) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let pid = std::process::id();
    thread::spawn(move || {
        let Some(h) = connect_with_timeout(Duration::from_millis(500)) else {
            return;
        };
        let msg = serde_json::json!({
            "op": "register",
            "session_id": session_id,
            "cwd": cwd.to_string_lossy().to_string(),
            "pid": pid,
        });
        let body = format!("{msg}\n");
        let _ = write_all_h(h, body.as_bytes());
        let mut buf = [0u8; 64];
        loop {
            let mut n: u32 = 0;
            let ok = unsafe { ReadFile(h, Some(&mut buf[..]), Some(&mut n), None) };
            if ok.is_err() || n == 0 {
                break;
            }
        }
        unsafe {
            let _ = CloseHandle(h);
        }
    });
}

fn connect_with_timeout(timeout: Duration) -> Option<HANDLE> {
    let name: Vec<u16> = registry::PIPE_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
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
        thread::sleep(Duration::from_millis(20));
    }
}
