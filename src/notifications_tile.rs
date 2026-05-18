//! Top-right notifications panel — lists every session that's currently
//! flagged as `NeedsAttention`, regardless of whether it lives locally
//! or under an external shim.
//!
//! Sources:
//!   * Local panel-spawned sessions whose computed `SessionStatus` is
//!     `NeedsAttention`. Clicking focuses the session.
//!   * Remote shim-registered sessions whose latest jsonl activity reads
//!     as `NeedsAttention` (claude finished, your turn). Clicking spawns
//!     a subscriber view via `AttachSession` so you can answer it
//!     without leaving the manager.
//!
//! When the queue is empty, renders a muted "All clear" placeholder so
//! the panel area isn't visually empty.

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;

use crate::claude_store::{self, SessionActivity};
use crate::dashboard::{CursorHint, TileAction};
use crate::native_interop::{self, Color};
use crate::registry;
use crate::sessions::{SessionId, SessionStatus, Sessions};

const ROW_HEIGHT: i32 = 36;
const PAD_X: i32 = 14;
const PAD_TOP: i32 = 12;
const HEADER_H: i32 = 22;
const STATUS_DOT_SIZE: i32 = 8;
const STATUS_DOT_GAP: i32 = 10;
const NAME_FONT_PT: i32 = 10;
const HEADER_FONT_PT: i32 = 9;

const PANEL_BG_HEX: &str = "#262624";
const ROW_BG_HEX: &str = "#2E2E2C";
const NAME_FG_HEX: &str = "#E8E8E8";
const NAME_FG_DIM_HEX: &str = "#A8A29E";
const HEADER_FG_HEX: &str = "#A8A29E";
const STATUS_NEEDS_HEX: &str = "#D97757";

fn pad_top(dpi: u32) -> i32 {
    (PAD_TOP as f64 * dpi as f64 / 96.0).round() as i32
}

fn pad_x(dpi: u32) -> i32 {
    (PAD_X as f64 * dpi as f64 / 96.0).round() as i32
}

fn header_h(dpi: u32) -> i32 {
    (HEADER_H as f64 * dpi as f64 / 96.0).round() as i32
}

fn row_h(dpi: u32) -> i32 {
    (ROW_HEIGHT as f64 * dpi as f64 / 96.0).round() as i32
}

/// Compute the rect for the i-th attention row (after the header).
fn row_rect(bounds: &RECT, dpi: u32, i: usize) -> Option<RECT> {
    let row_h = row_h(dpi);
    let top0 = bounds.top + pad_top(dpi) + header_h(dpi);
    let visible = ((bounds.bottom - top0) / row_h).max(0) as usize;
    if i >= visible {
        return None;
    }
    let top = top0 + i as i32 * row_h;
    Some(RECT {
        left: bounds.left,
        top,
        right: bounds.right,
        bottom: top + row_h,
    })
}

/// One row in the attention list. Local rows reuse the panel's session
/// id for click → focus; remote rows carry the registry/jsonl info needed
/// to spawn an attaching subscriber view.
enum AttnRow {
    Local {
        id: SessionId,
        name: String,
    },
    Remote {
        session_id: String,
        cwd: std::path::PathBuf,
        name: String,
    },
}

/// Build the notifications row list from both the local sessions and
/// the shim registry. Local sessions are surfaced when the status
/// recompute has bucketed them as `NeedsAttention`; remote sessions
/// when their latest jsonl activity reads the same. Sessions that are
/// already represented locally (same UUID) are deduped to the local row
/// — once you've attached, you don't need a second alert for the same
/// thing.
fn build_rows(sessions: &Sessions) -> Vec<AttnRow> {
    use std::collections::HashSet;
    let mut rows: Vec<AttnRow> = Vec::new();
    let mut local_ids: HashSet<String> = HashSet::new();
    for s in sessions.iter() {
        if !s.session_id.is_empty() {
            local_ids.insert(s.session_id.clone());
        }
        if s.status == SessionStatus::NeedsAttention {
            rows.push(AttnRow::Local {
                id: s.id,
                name: s.name.clone(),
            });
        }
    }
    let now = std::time::SystemTime::now();
    let store = claude_store::global();
    for entry in registry::snapshot() {
        if entry.session_id.is_empty() || local_ids.contains(&entry.session_id) {
            continue;
        }
        let Some(history) = store.lookup_by_session_id(&entry.session_id) else {
            continue;
        };
        if history.activity_at(now) != SessionActivity::NeedsAttention {
            continue;
        }
        let cwd = std::path::PathBuf::from(&entry.cwd);
        let name = remote_label(&entry.session_id, &cwd);
        rows.push(AttnRow::Remote {
            session_id: entry.session_id,
            cwd,
            name,
        });
    }
    rows
}

fn remote_label(session_id: &str, cwd: &std::path::Path) -> String {
    let basename = cwd.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if !basename.is_empty() {
        return basename.to_string();
    }
    let short: String = session_id.chars().take(8).collect();
    if short.is_empty() {
        "session".into()
    } else {
        short
    }
}

pub fn paint(hdc: HDC, bounds: RECT, dpi: u32, sessions: &Sessions) {
    let panel_bg = Color::from_hex(PANEL_BG_HEX);
    let row_bg = Color::from_hex(ROW_BG_HEX);
    let name_fg = Color::from_hex(NAME_FG_HEX);
    let name_fg_dim = Color::from_hex(NAME_FG_DIM_HEX);
    let header_fg = Color::from_hex(HEADER_FG_HEX);
    let attn_color = Color::from_hex(STATUS_NEEDS_HEX);

    unsafe {
        let bg = CreateSolidBrush(COLORREF(panel_bg.to_colorref()));
        FillRect(hdc, &bounds, bg);
        let _ = DeleteObject(bg);
        let _ = SetBkMode(hdc, TRANSPARENT);
    }

    // Header label.
    let header_height = -(HEADER_FONT_PT * dpi as i32 / 72);
    let header_face = native_interop::wide_str("Segoe UI");
    let header_font = unsafe {
        CreateFontW(
            header_height,
            0,
            0,
            0,
            FW_SEMIBOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(header_face.as_ptr()),
        )
    };
    let old_font = unsafe { SelectObject(hdc, header_font) };
    unsafe {
        let _ = SetTextColor(hdc, COLORREF(header_fg.to_colorref()));
    }
    let mut header_text: Vec<u16> = "NEEDS ATTENTION".encode_utf16().collect();
    let mut header_rect = RECT {
        left: bounds.left + pad_x(dpi),
        top: bounds.top + pad_top(dpi),
        right: bounds.right - pad_x(dpi),
        bottom: bounds.top + pad_top(dpi) + header_h(dpi),
    };
    unsafe {
        let _ = DrawTextW(
            hdc,
            &mut header_text,
            &mut header_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        SelectObject(hdc, old_font);
        let _ = DeleteObject(header_font);
    }

    // Rows.
    let name_font_h = -(NAME_FONT_PT * dpi as i32 / 72);
    let face = native_interop::wide_str("Segoe UI");
    let row_font = unsafe {
        CreateFontW(
            name_font_h,
            0,
            0,
            0,
            FW_MEDIUM.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(face.as_ptr()),
        )
    };
    let old_font = unsafe { SelectObject(hdc, row_font) };

    let rows = build_rows(sessions);
    if rows.is_empty() {
        // Empty-state placeholder, dim and centred horizontally.
        unsafe {
            let _ = SetTextColor(hdc, COLORREF(header_fg.to_colorref()));
        }
        let mut empty_text: Vec<u16> = "All clear".encode_utf16().collect();
        let mut empty_rect = RECT {
            left: bounds.left + pad_x(dpi),
            top: bounds.top + pad_top(dpi) + header_h(dpi),
            right: bounds.right - pad_x(dpi),
            bottom: bounds.bottom - pad_top(dpi),
        };
        unsafe {
            let _ = DrawTextW(
                hdc,
                &mut empty_text,
                &mut empty_rect,
                DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX,
            );
        }
    } else {
        let pad_x_px = pad_x(dpi);
        let dot_size = (STATUS_DOT_SIZE as f64 * dpi as f64 / 96.0).round() as i32;
        let dot_gap = (STATUS_DOT_GAP as f64 * dpi as f64 / 96.0).round() as i32;

        for (i, row) in rows.iter().enumerate() {
            let Some(rect) = row_rect(&bounds, dpi, i) else {
                break;
            };

            let body = RECT {
                left: rect.left + pad_x_px / 2,
                top: rect.top + 2,
                right: rect.right - pad_x_px / 2,
                bottom: rect.bottom - 2,
            };
            unsafe {
                let brush = CreateSolidBrush(COLORREF(row_bg.to_colorref()));
                FillRect(hdc, &body, brush);
                let _ = DeleteObject(brush);
            }

            let dot_x = body.left + pad_x_px;
            let dot_y = (body.top + body.bottom) / 2 - dot_size / 2;
            unsafe {
                let brush = CreateSolidBrush(COLORREF(attn_color.to_colorref()));
                let rgn = CreateRoundRectRgn(
                    dot_x,
                    dot_y,
                    dot_x + dot_size + 1,
                    dot_y + dot_size + 1,
                    dot_size,
                    dot_size,
                );
                let _ = FillRgn(hdc, rgn, brush);
                let _ = DeleteObject(rgn);
                let _ = DeleteObject(brush);
            }

            let label_x = dot_x + dot_size + dot_gap;
            let mut label_rect = RECT {
                left: label_x,
                top: body.top,
                right: body.right,
                bottom: body.bottom,
            };
            let (text, fg) = match row {
                AttnRow::Local { name, .. } => (name.as_str(), name_fg),
                // Remote rows render in dim text — same convention as
                // the sidebar, so the user can tell at a glance which
                // attention items will require attaching first.
                AttnRow::Remote { name, .. } => (name.as_str(), name_fg_dim),
            };
            let mut label_wide: Vec<u16> = text.encode_utf16().collect();
            unsafe {
                let _ = SetTextColor(hdc, COLORREF(fg.to_colorref()));
                let _ = DrawTextW(
                    hdc,
                    &mut label_wide,
                    &mut label_rect,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
                );
            }
        }
    }

    unsafe {
        SelectObject(hdc, old_font);
        let _ = DeleteObject(row_font);
    }
}

pub fn cursor_at(x: i32, y: i32, bounds: RECT, dpi: u32, sessions: &Sessions) -> CursorHint {
    let rows = build_rows(sessions);
    for i in 0..rows.len() {
        let Some(rect) = row_rect(&bounds, dpi, i) else {
            break;
        };
        if x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom {
            return CursorHint::Hand;
        }
    }
    CursorHint::Arrow
}

pub fn handle_lbutton_down(
    x: i32,
    y: i32,
    bounds: RECT,
    dpi: u32,
    sessions: &Sessions,
) -> Option<TileAction> {
    let rows = build_rows(sessions);
    for (i, row) in rows.iter().enumerate() {
        let Some(rect) = row_rect(&bounds, dpi, i) else {
            break;
        };
        if x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom {
            return match row {
                AttnRow::Local { id, .. } => Some(TileAction::FocusSession(*id)),
                AttnRow::Remote {
                    session_id,
                    cwd,
                    name,
                } => Some(TileAction::AttachSession {
                    session_id: session_id.clone(),
                    cwd: cwd.clone(),
                    name: name.clone(),
                }),
            };
        }
    }
    None
}
