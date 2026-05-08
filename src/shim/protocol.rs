//! Wire format for the per-session pipe `\\.\pipe\ccmonitor-session-<uuid>`.
//!
//! Frames are simple length-prefixed blobs:
//!
//! ```text
//!   byte 0:    type tag (one of `O`, `I`, `R`, `H`)
//!   bytes 1-4: payload length, u32 big-endian
//!   bytes 5..: payload (length bytes)
//! ```
//!
//! Direction:
//!   - `O` output  — owner → subscriber. Payload = raw bytes from claude's PTY.
//!   - `I` input   — subscriber → owner. Payload = raw bytes to feed claude.
//!   - `R` resize  — subscriber → owner. Payload = `cols:u16 BE | rows:u16 BE`.
//!   - `H` hello   — subscriber → owner, sent first on every connection.
//!     Payload = same shape as `R` (initial size). Lets the owner add the
//!     subscriber to its size-merge map before any input flows.

pub const TAG_OUTPUT: u8 = b'O';
pub const TAG_INPUT: u8 = b'I';
pub const TAG_RESIZE: u8 = b'R';
pub const TAG_HELLO: u8 = b'H';
/// Control-plane query (JSON payload) sent by manager-side clients
/// that don't want the terminal stream — they just want metadata or
/// other small RPC-style answers. Owner replies with `M`.
pub const TAG_QUERY: u8 = b'Q';
/// Owner's reply to a `Q` frame. Payload is JSON.
pub const TAG_METADATA: u8 = b'M';

pub fn session_pipe_name(session_id: &str) -> String {
    format!(r"\\.\pipe\ccmonitor-session-{session_id}")
}

/// Build a complete frame header (5 bytes) for a payload of `len` bytes.
/// Caller appends the payload after this header.
pub fn frame_header(tag: u8, len: u32) -> [u8; 5] {
    let mut h = [0u8; 5];
    h[0] = tag;
    h[1..5].copy_from_slice(&len.to_be_bytes());
    h
}

pub fn encode_size_payload(cols: u16, rows: u16) -> [u8; 4] {
    let mut p = [0u8; 4];
    p[0..2].copy_from_slice(&cols.to_be_bytes());
    p[2..4].copy_from_slice(&rows.to_be_bytes());
    p
}

pub fn decode_size_payload(payload: &[u8]) -> Option<(u16, u16)> {
    if payload.len() != 4 {
        return None;
    }
    Some((
        u16::from_be_bytes([payload[0], payload[1]]),
        u16::from_be_bytes([payload[2], payload[3]]),
    ))
}

/// Streaming frame parser. Feed bytes via `feed`; pull complete frames via
/// `next_frame`. Stores partial frames internally so it's safe to feed
/// arbitrary chunk boundaries.
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn feed(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub fn next_frame(&mut self) -> Option<(u8, Vec<u8>)> {
        if self.buf.len() < 5 {
            return None;
        }
        let tag = self.buf[0];
        let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if self.buf.len() < 5 + len {
            return None;
        }
        let payload = self.buf[5..5 + len].to_vec();
        self.buf.drain(..5 + len);
        Some((tag, payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip() {
        let mut r = FrameReader::new();
        // Build two frames back-to-back across a chunk boundary.
        let mut wire = Vec::new();
        wire.extend_from_slice(&frame_header(TAG_OUTPUT, 3));
        wire.extend_from_slice(b"abc");
        wire.extend_from_slice(&frame_header(TAG_RESIZE, 4));
        wire.extend_from_slice(&encode_size_payload(120, 30));

        // Feed in two chunks straddling the boundary.
        r.feed(&wire[..7]);
        assert!(r.next_frame().is_none(), "incomplete frame must wait");
        r.feed(&wire[7..]);

        let (t1, p1) = r.next_frame().unwrap();
        assert_eq!(t1, TAG_OUTPUT);
        assert_eq!(p1, b"abc");

        let (t2, p2) = r.next_frame().unwrap();
        assert_eq!(t2, TAG_RESIZE);
        assert_eq!(decode_size_payload(&p2), Some((120, 30)));

        assert!(r.next_frame().is_none());
    }

    #[test]
    fn pipe_name_format() {
        let n = session_pipe_name("abc-123");
        assert_eq!(n, r"\\.\pipe\ccmonitor-session-abc-123");
    }

    #[test]
    fn tag_constants_are_distinct() {
        // Sanity check that two tags didn't collide on the same byte
        // — a bug that would silently misroute frames.
        let tags = [TAG_OUTPUT, TAG_INPUT, TAG_RESIZE, TAG_HELLO, TAG_QUERY, TAG_METADATA];
        let mut sorted = tags.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            tags.len(),
            "tag constants must be pairwise distinct"
        );
    }

    #[test]
    fn query_metadata_round_trip() {
        // Build a Q frame with a JSON payload, parse it back, then
        // build an M reply and parse that. Confirms the wire format
        // works for control-plane traffic, not just terminal bytes.
        let req = br#"{"op":"metadata"}"#;
        let mut wire = Vec::new();
        wire.extend_from_slice(&frame_header(TAG_QUERY, req.len() as u32));
        wire.extend_from_slice(req);

        let reply = br#"{"session_id":"abc","cwd":"C:\\","pid":42}"#;
        wire.extend_from_slice(&frame_header(TAG_METADATA, reply.len() as u32));
        wire.extend_from_slice(reply);

        let mut r = FrameReader::new();
        r.feed(&wire);

        let (t1, p1) = r.next_frame().unwrap();
        assert_eq!(t1, TAG_QUERY);
        assert_eq!(p1, req);

        let (t2, p2) = r.next_frame().unwrap();
        assert_eq!(t2, TAG_METADATA);
        assert_eq!(p2, reply);

        assert!(r.next_frame().is_none());
    }

    #[test]
    fn empty_payload_frame_round_trips() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&frame_header(TAG_INPUT, 0));
        let mut r = FrameReader::new();
        r.feed(&wire);
        let (tag, payload) = r.next_frame().unwrap();
        assert_eq!(tag, TAG_INPUT);
        assert!(payload.is_empty());
    }

    #[test]
    fn size_payload_round_trip_at_boundaries() {
        // Exercise the cols/rows packing at the edges of u16.
        for &(c, r) in &[(0u16, 0u16), (1, 1), (80, 24), (u16::MAX, u16::MAX)] {
            let p = encode_size_payload(c, r);
            assert_eq!(decode_size_payload(&p), Some((c, r)));
        }
        // Wrong-length payload returns None.
        assert!(decode_size_payload(&[0, 0, 0]).is_none());
        assert!(decode_size_payload(&[0; 5]).is_none());
    }
}
