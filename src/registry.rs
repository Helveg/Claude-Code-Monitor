//! In-process registry of live claude sessions, exposed to other
//! processes over a named pipe. The shim registers itself on spawn; the
//! manager uses the registry to know which externally-launched claudes
//! are alive (so its dashboard can list them and, in v2, attach).
//!
//! Wire format: newline-delimited JSON, one frame per line.
//!
//! Shim → server, on connect:
//!   ```json
//!   {"op":"register","session_id":"<uuid>","cwd":"<path>","pid":<u32>}
//!   ```
//! Server → shim:
//!   ```json
//!   {"ok":true}
//!   ```
//!
//! After the register/ack handshake the connection stays open and idle —
//! the connection itself is the heartbeat. When the shim exits or the
//! pipe disconnects for any reason, the server removes that session.
//! No additional message types in v1.
//!
//! The pipe lives at `\\.\pipe\ccmonitor-registry`. Only one instance of
//! the manager runs at a time (single-instance mutex elsewhere in the
//! app), so the pipe name is fixed and the server "owns" it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

use crate::diagnose;
use crate::shim::protocol::{self, FrameReader, TAG_METADATA, TAG_QUERY};

pub const PIPE_NAME: &str = r"\\.\pipe\ccmonitor-registry";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub session_id: String,
    pub cwd: String,
    pub pid: u32,
}

#[derive(Deserialize)]
struct RegisterMessage {
    #[allow(dead_code)]
    op: Option<String>,
    session_id: String,
    cwd: String,
    pid: u32,
}

#[derive(Serialize)]
struct AckOk {
    ok: bool,
}

static REGISTRY: OnceLock<Arc<Mutex<HashMap<String, RegistryEntry>>>> = OnceLock::new();
static SERVER_STARTED: AtomicBool = AtomicBool::new(false);

fn registry() -> &'static Arc<Mutex<HashMap<String, RegistryEntry>>> {
    REGISTRY.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
}

#[allow(dead_code)]
pub fn snapshot() -> Vec<RegistryEntry> {
    let r = registry();
    let g = r.lock().unwrap_or_else(|p| p.into_inner());
    g.values().cloned().collect()
}

#[allow(dead_code)]
pub fn lookup(session_id: &str) -> Option<RegistryEntry> {
    let r = registry();
    let g = r.lock().unwrap_or_else(|p| p.into_inner());
    g.get(session_id).cloned()
}

/// Spawn the registry server thread. Idempotent — second-and-later calls
/// are no-ops, so it's safe to wire this into multiple startup paths.
/// Also kicks off a one-shot rediscovery scan in the background that
/// finds sessions owned by shims that started before this manager
/// instance (e.g. after a manager restart).
pub fn start_server() {
    if SERVER_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    thread::Builder::new()
        .name("registry-server".into())
        .spawn(server_main)
        .ok();
    thread::Builder::new()
        .name("registry-rediscover".into())
        .spawn(rediscover_sessions)
        .ok();
}

/// Walk `\\.\pipe\` for `ccmonitor-session-*` entries — each one is a
/// live shim-owned session. For each found pipe, open a control
/// connection and ask for metadata via a `Q`/`M` exchange. On success
/// the session is inserted into the in-memory registry. Best-effort:
/// failures are logged and the loop moves on.
fn rediscover_sessions() {
    // Tiny pause so the registry server has its named pipe up before
    // any newly-spawned shims try to call back, avoiding a confusing
    // race in the diagnose log.
    thread::sleep(Duration::from_millis(50));

    let entries = match std::fs::read_dir(r"\\.\pipe\") {
        Ok(e) => e,
        Err(e) => {
            diagnose::log(format!("registry: rediscover read_dir failed: {e}"));
            return;
        }
    };
    let mut found = 0u32;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let Some(session_id) = name_str.strip_prefix("ccmonitor-session-") else {
            continue;
        };
        let pipe_path = format!(r"\\.\pipe\{name_str}");
        match query_session_metadata(&pipe_path) {
            Some(entry) => {
                let r = registry();
                let mut g = r.lock().unwrap_or_else(|p| p.into_inner());
                // Don't clobber an entry that arrived via active
                // registration in the meantime.
                g.entry(entry.session_id.clone()).or_insert(entry);
                found += 1;
            }
            None => {
                diagnose::log(format!(
                    "registry: rediscover {} → metadata query failed",
                    session_id
                ));
            }
        }
    }
    if found > 0 {
        diagnose::log(format!("registry: rediscovered {found} live session(s)"));
    }
}

fn query_session_metadata(pipe_path: &str) -> Option<RegistryEntry> {
    let name: Vec<u16> = pipe_path.encode_utf16().chain(std::iter::once(0)).collect();
    let h = unsafe {
        CreateFileW(
            PCWSTR::from_raw(name.as_ptr()),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .ok()?;
    if h.is_invalid() {
        return None;
    }
    // Always close the handle on every exit path.
    let result = (|| -> Option<RegistryEntry> {
        // Send Q with op=metadata
        let body = serde_json::json!({ "op": "metadata" }).to_string();
        let mut req = Vec::with_capacity(5 + body.len());
        req.extend_from_slice(&protocol::frame_header(TAG_QUERY, body.len() as u32));
        req.extend_from_slice(body.as_bytes());
        if !pipe_write_all(h, &req) {
            return None;
        }

        // Read frames until we see an M (or fail). Owner may interleave
        // O frames if the connection becomes a terminal subscriber, but
        // we only sent Q so we just ignore non-M frames.
        let mut reader = FrameReader::new();
        let mut buf = [0u8; 4096];
        let deadline =
            std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            let mut n: u32 = 0;
            let ok = unsafe { ReadFile(h, Some(&mut buf[..]), Some(&mut n), None) };
            if ok.is_err() || n == 0 {
                return None;
            }
            reader.feed(&buf[..n as usize]);
            while let Some((tag, payload)) = reader.next_frame() {
                if tag == TAG_METADATA {
                    let v: serde_json::Value =
                        serde_json::from_slice(&payload).ok()?;
                    return Some(RegistryEntry {
                        session_id: v
                            .get("session_id")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                        cwd: v
                            .get("cwd")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                        pid: v
                            .get("pid")
                            .and_then(|x| x.as_u64())
                            .unwrap_or(0) as u32,
                    });
                }
            }
        }
        None
    })();
    unsafe {
        let _ = CloseHandle(h);
    }
    result
}

fn pipe_write_all(h: HANDLE, mut data: &[u8]) -> bool {
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

fn server_main() {
    diagnose::log("registry: server thread starting");
    let mut name_w: Vec<u16> = PIPE_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    loop {
        let h = unsafe {
            CreateNamedPipeW(
                PCWSTR::from_raw(name_w.as_mut_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                None,
            )
        };
        if h.is_invalid() {
            diagnose::log("registry: CreateNamedPipeW returned invalid handle");
            thread::sleep(std::time::Duration::from_millis(500));
            continue;
        }
        // ConnectNamedPipe blocks until a client connects. ERROR_PIPE_CONNECTED
        // is a benign race-win where the client beat our call.
        let connect_res = unsafe { ConnectNamedPipe(h, None) };
        let benign_already_connected = match connect_res.as_ref() {
            Ok(()) => true,
            Err(e) => e.code().0 as u32 == ERROR_PIPE_CONNECTED.0,
        };
        if !benign_already_connected {
            unsafe {
                let _ = CloseHandle(h);
            }
            continue;
        }
        // HANDLE wraps *mut c_void which isn't Send; ferry the raw address
        // across the thread boundary as isize and rebuild on the far side.
        let h_addr = h.0 as isize;
        thread::Builder::new()
            .name("registry-conn".into())
            .spawn(move || handle_connection(HANDLE(h_addr as *mut _)))
            .ok();
    }
}

fn handle_connection(h: HANDLE) {
    let mut buf = [0u8; 4096];
    let mut accumulated: Vec<u8> = Vec::new();
    let mut registered_id: Option<String> = None;

    loop {
        let mut n: u32 = 0;
        let ok = unsafe { ReadFile(h, Some(&mut buf[..]), Some(&mut n), None) };
        if ok.is_err() || n == 0 {
            break;
        }
        accumulated.extend_from_slice(&buf[..n as usize]);
        while let Some(nl) = accumulated.iter().position(|&b| b == b'\n') {
            let line_str = String::from_utf8_lossy(&accumulated[..nl]).into_owned();
            accumulated.drain(..=nl);
            if registered_id.is_some() {
                // v1: no further messages expected — silently ignore.
                continue;
            }
            let msg: RegisterMessage = match serde_json::from_str(&line_str) {
                Ok(m) => m,
                Err(e) => {
                    diagnose::log(format!("registry: bad register frame: {e}"));
                    return;
                }
            };
            let entry = RegistryEntry {
                session_id: msg.session_id.clone(),
                cwd: msg.cwd,
                pid: msg.pid,
            };
            {
                let r = registry();
                let mut g = r.lock().unwrap_or_else(|p| p.into_inner());
                g.insert(entry.session_id.clone(), entry.clone());
            }
            diagnose::log(format!(
                "registry: + {} pid={} cwd={}",
                entry.session_id, entry.pid, entry.cwd
            ));
            registered_id = Some(entry.session_id);
            let resp = serde_json::to_string(&AckOk { ok: true }).unwrap_or_default();
            let body = format!("{resp}\n");
            let mut written: u32 = 0;
            unsafe {
                let _ = WriteFile(h, Some(body.as_bytes()), Some(&mut written), None);
            }
        }
    }

    if let Some(id) = registered_id {
        let r = registry();
        let mut g = r.lock().unwrap_or_else(|p| p.into_inner());
        g.remove(&id);
        diagnose::log(format!("registry: - {} (disconnect)", id));
    }
    unsafe {
        let _ = DisconnectNamedPipe(h);
        let _ = CloseHandle(h);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_entry_round_trips_via_json() {
        // The wire format for both the register handshake and the
        // metadata query reply is JSON. If a field name or type drifts,
        // the manager's snapshot will silently de-populate.
        let original = RegistryEntry {
            session_id: "8ffdc1f0-f702-4279-a906-505a49959ee3".into(),
            cwd: r"C:\Users\robin\Documents\git\Claude-Code-Monitor".into(),
            pid: 12345,
        };
        let s = serde_json::to_string(&original).unwrap();
        let parsed: RegistryEntry = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed.session_id, original.session_id);
        assert_eq!(parsed.cwd, original.cwd);
        assert_eq!(parsed.pid, original.pid);
    }

    #[test]
    fn register_message_accepts_either_with_or_without_op_field() {
        // Forward-compat: the op field is optional so older shims
        // (that didn't include it) and newer ones both parse.
        let with_op: RegisterMessage = serde_json::from_str(
            r#"{"op":"register","session_id":"abc","cwd":"C:\\","pid":1}"#,
        )
        .unwrap();
        assert_eq!(with_op.session_id, "abc");
        assert_eq!(with_op.pid, 1);

        let without_op: RegisterMessage = serde_json::from_str(
            r#"{"session_id":"abc","cwd":"C:\\","pid":1}"#,
        )
        .unwrap();
        assert_eq!(without_op.session_id, "abc");
    }

    #[test]
    fn pipe_name_constant_is_well_formed() {
        assert!(PIPE_NAME.starts_with(r"\\.\pipe\"));
    }
}
