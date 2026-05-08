//! Subscriber mode — used when a per-session pipe already exists for the
//! requested session id (someone else owns it). We become a relay
//! between the local console and the owner's pipe: render their output
//! locally, ship our keystrokes to them.
//!
//! Single thread per pipe. Windows synchronous-mode pipe I/O serializes
//! per pipe (not per handle), so two threads doing concurrent
//! Read/WriteFile on the same pipe deadlock the OS-level I/O. Instead,
//! the main thread multiplexes both directions via PeekNamedPipe (for
//! incoming) + try_recv (for outgoing) with a small idle sleep. The
//! stdin reader is its own thread because stdin is a different handle
//! and ReadFile on it is unavoidably blocking.

use std::process::ExitCode;
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows::Win32::System::Pipes::PeekNamedPipe;
use windows::Win32::Foundation::CloseHandle;

use crate::shim::protocol::{
    self, FrameReader, TAG_HELLO, TAG_INPUT, TAG_OUTPUT, TAG_RESIZE,
};
use crate::shim::util::{current_console_size, ConsoleModeGuard};

pub fn run(pipe: HANDLE) -> ExitCode {
    let console = ConsoleModeGuard::install();
    let stdin = console.stdin;
    let stdout = console.stdout;

    crate::shim::util::shim_log(format!(
        "sub: run started, pipe addr=0x{:x}",
        pipe.0 as isize
    ));

    // Tell the owner who we are and our initial size before any I/O flows.
    let (cols, rows) = current_console_size();
    let mut hello = Vec::with_capacity(9);
    hello.extend_from_slice(&protocol::frame_header(TAG_HELLO, 4));
    hello.extend_from_slice(&protocol::encode_size_payload(cols, rows));
    if !write_all(pipe, &hello) {
        unsafe {
            let _ = CloseHandle(pipe);
        }
        return ExitCode::from(1);
    }
    crate::shim::util::shim_log("sub: H frame sent");

    // Channel from stdin reader thread to the main pipe thread. Stdin
    // ReadFile blocks indefinitely so it has to live in its own thread;
    // it just queues frames for the main thread to send when the pipe
    // is between operations.
    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();

    let stdin_addr = stdin.0 as isize;
    thread::spawn(move || {
        let stdin = HANDLE(stdin_addr as *mut _);
        let mut buf = [0u8; 1024];
        loop {
            let mut n: u32 = 0;
            let ok = unsafe { ReadFile(stdin, Some(&mut buf[..]), Some(&mut n), None) };
            if ok.is_err() || n == 0 {
                crate::shim::util::shim_log(format!(
                    "sub: stdin reader exit (ok={} n={n})",
                    ok.is_ok()
                ));
                return;
            }
            let mut frame = Vec::with_capacity(5 + n as usize);
            frame.extend_from_slice(&protocol::frame_header(TAG_INPUT, n));
            frame.extend_from_slice(&buf[..n as usize]);
            if out_tx.send(frame).is_err() {
                return;
            }
        }
    });

    // Main thread: pipe I/O for both directions + periodic resize check.
    let mut frame_reader = FrameReader::new();
    let mut buf = [0u8; 4096];
    let mut last_size = (cols, rows);
    let mut last_resize_check = Instant::now();
    let resize_interval = Duration::from_millis(500);

    'main: loop {
        let mut did_work = false;

        // Outbound: drain one frame from the channel if available.
        match out_rx.try_recv() {
            Ok(frame) => {
                if !write_all(pipe, &frame) {
                    crate::shim::util::shim_log(
                        "sub: outbound WriteFile failed; exiting",
                    );
                    break 'main;
                }
                did_work = true;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                // Stdin reader died — almost always means our console
                // went away (terminal closed, or the manager closed
                // the ConPTY we were spawned into). A subscriber with
                // no input source is useless: we'd keep receiving
                // owner output but couldn't forward keystrokes, and
                // the owner has no way to tell. Exit cleanly so the
                // owner sees the disconnect and its refcount drops.
                crate::shim::util::shim_log(
                    "sub: stdin reader gone; exiting",
                );
                break 'main;
            }
        }

        // Inbound: peek for available bytes, read once if any.
        let mut avail: u32 = 0;
        let peek_ok = unsafe {
            PeekNamedPipe(pipe, None, 0, None, Some(&mut avail), None)
        };
        if peek_ok.is_err() {
            crate::shim::util::shim_log("sub: PeekNamedPipe err; exiting");
            break 'main;
        }
        if avail > 0 {
            let mut n: u32 = 0;
            let ok = unsafe { ReadFile(pipe, Some(&mut buf[..]), Some(&mut n), None) };
            if ok.is_err() || n == 0 {
                crate::shim::util::shim_log("sub: pipe ReadFile EOF; exiting");
                break 'main;
            }
            frame_reader.feed(&buf[..n as usize]);
            while let Some((tag, payload)) = frame_reader.next_frame() {
                if tag == TAG_OUTPUT {
                    let _ = write_all(stdout, &payload);
                }
            }
            did_work = true;
        }

        // Periodic resize check — once per `resize_interval`.
        if last_resize_check.elapsed() >= resize_interval {
            let cur = current_console_size();
            if cur != last_size {
                let mut frame = Vec::with_capacity(9);
                frame.extend_from_slice(&protocol::frame_header(TAG_RESIZE, 4));
                frame.extend_from_slice(&protocol::encode_size_payload(cur.0, cur.1));
                if !write_all(pipe, &frame) {
                    break 'main;
                }
                last_size = cur;
                did_work = true;
            }
            last_resize_check = Instant::now();
        }

        if !did_work {
            thread::sleep(Duration::from_millis(5));
        }
    }

    drop(console); // restore console modes
    unsafe {
        let _ = CloseHandle(pipe);
    }
    ExitCode::from(0)
}

fn write_all(h: HANDLE, mut data: &[u8]) -> bool {
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

