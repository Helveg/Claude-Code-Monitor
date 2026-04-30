//! Sidebar tile — vertical list of mini status widgets, one per session,
//! plus a "+ New session" entry at the bottom.
//!
//! Click a session row → panel focuses that session.
//! Click the new-session row → panel spawns a new session.

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;

use crate::dashboard::{CursorHint, TileAction};
use crate::native_interop::{self, Color};
use crate::sessions::{SessionId, SessionStatus, Sessions};

/// Design pixels at 96 DPI.
const ROW_HEIGHT: i32 = 36;
const PAD_X: i32 = 14;
const PAD_TOP: i32 = 12;
const STATUS_DOT_SIZE: i32 = 8;
const STATUS_DOT_GAP: i32 = 10;
const NAME_FONT_PT: i32 = 10;

const PANEL_BG_HEX: &str = "#262624";
const ROW_BG_HEX: &str = "#2E2E2C";
const ROW_BG_FOCUSED_HEX: &str = "#3A3A38";
const NAME_FG_HEX: &str = "#E8E8E8";
const NAME_FG_DIM_HEX: &str = "#A8A29E";
const ORANGE_HEX: &str = "#D97757";
const STATUS_IDLE_HEX: &str = "#5A5F58";
const STATUS_THINKING_HEX: &str = "#5BD16B";
const STATUS_NEEDS_HEX: &str = "#E07A5F";

/// Compute the row rect at index `i` in `bounds`, including the trailing
/// "+ New session" entry. Returns `None` if `i` is past the visible rows.
fn row_rect(bounds: &RECT, dpi: u32, i: usize, total: usize) -> Option<RECT> {
    let scale = dpi as f64 / 96.0;
    let row_h = (ROW_HEIGHT as f64 * scale).round() as i32;
    let pad_top = (PAD_TOP as f64 * scale).round() as i32;
    let visible = ((bounds.bottom - bounds.top - pad_top) / row_h).max(0) as usize;
    if i >= visible || i >= total {
        return None;
    }
    let top = bounds.top + pad_top + i as i32 * row_h;
    Some(RECT {
        left: bounds.left,
        top,
        right: bounds.right,
        bottom: top + row_h,
    })
}

pub fn paint(hdc: HDC, bounds: RECT, dpi: u32, sessions: &Sessions, focused: Option<SessionId>) {
    let scale = dpi as f64 / 96.0;
    let panel_bg = Color::from_hex(PANEL_BG_HEX);
    let row_bg = Color::from_hex(ROW_BG_HEX);
    let row_bg_focused = Color::from_hex(ROW_BG_FOCUSED_HEX);
    let name_fg = Color::from_hex(NAME_FG_HEX);
    let name_fg_dim = Color::from_hex(NAME_FG_DIM_HEX);
    let orange = Color::from_hex(ORANGE_HEX);

    unsafe {
        // Bg fill — sidebar uses the panel bg so it visually disappears
        // into the surrounding chrome.
        let bg = CreateSolidBrush(COLORREF(panel_bg.to_colorref()));
        FillRect(hdc, &bounds, bg);
        let _ = DeleteObject(bg);
        let _ = SetBkMode(hdc, TRANSPARENT);
    }

    // Build a small font for row labels.
    let height = -(NAME_FONT_PT * dpi as i32 / 72);
    let face = native_interop::wide_str("Segoe UI");
    let font = unsafe {
        CreateFontW(
            height,
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
    let old_font = unsafe { SelectObject(hdc, font) };

    let total = sessions.len() + 1; // +1 for "+ New session" row
    for (i, s) in sessions.iter().enumerate() {
        let Some(rect) = row_rect(&bounds, dpi, i, total) else {
            break;
        };
        let is_focused = focused == Some(s.id);
        let row_color = if is_focused { row_bg_focused } else { row_bg };

        let dot_size = (STATUS_DOT_SIZE as f64 * scale).round() as i32;
        let dot_gap = (STATUS_DOT_GAP as f64 * scale).round() as i32;
        let pad_x = (PAD_X as f64 * scale).round() as i32;

        // Indented row body so it doesn't run flush to the edges.
        let body = RECT {
            left: rect.left + (pad_x / 2),
            top: rect.top + 2,
            right: rect.right - (pad_x / 2),
            bottom: rect.bottom - 2,
        };
        unsafe {
            let brush = CreateSolidBrush(COLORREF(row_color.to_colorref()));
            FillRect(hdc, &body, brush);
            let _ = DeleteObject(brush);
        }

        // Status dot.
        let status_color = match s.status {
            SessionStatus::Idle => Color::from_hex(STATUS_IDLE_HEX),
            SessionStatus::Thinking => Color::from_hex(STATUS_THINKING_HEX),
            SessionStatus::NeedsAttention => Color::from_hex(STATUS_NEEDS_HEX),
        };
        let dot_x = body.left + pad_x;
        let dot_y = (body.top + body.bottom) / 2 - dot_size / 2;
        let dot_rect = RECT {
            left: dot_x,
            top: dot_y,
            right: dot_x + dot_size,
            bottom: dot_y + dot_size,
        };
        unsafe {
            let brush = CreateSolidBrush(COLORREF(status_color.to_colorref()));
            let rgn = CreateRoundRectRgn(
                dot_rect.left,
                dot_rect.top,
                dot_rect.right + 1,
                dot_rect.bottom + 1,
                dot_size,
                dot_size,
            );
            let _ = FillRgn(hdc, rgn, brush);
            let _ = DeleteObject(rgn);
            let _ = DeleteObject(brush);
        }

        // Name label.
        let label_x = dot_rect.right + dot_gap;
        let mut label_rect = RECT {
            left: label_x,
            top: body.top,
            right: body.right,
            bottom: body.bottom,
        };
        let mut label_wide: Vec<u16> = s.name.encode_utf16().collect();
        unsafe {
            let _ = SetTextColor(hdc, COLORREF(name_fg.to_colorref()));
            let _ = DrawTextW(
                hdc,
                &mut label_wide,
                &mut label_rect,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
        }
    }

    // "+ New session" row at the end.
    let new_idx = sessions.len();
    if let Some(rect) = row_rect(&bounds, dpi, new_idx, total) {
        let pad_x = (PAD_X as f64 * scale).round() as i32;
        let body = RECT {
            left: rect.left + (pad_x / 2),
            top: rect.top + 2,
            right: rect.right - (pad_x / 2),
            bottom: rect.bottom - 2,
        };
        let mut label_wide: Vec<u16> = "+  New session".encode_utf16().collect();
        let mut label_rect = RECT {
            left: body.left + pad_x,
            top: body.top,
            right: body.right,
            bottom: body.bottom,
        };
        unsafe {
            let _ = SetTextColor(hdc, COLORREF(name_fg_dim.to_colorref()));
            let _ = DrawTextW(
                hdc,
                &mut label_wide,
                &mut label_rect,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
        }
        let _ = orange; // suppressed — reserved for hover state in a future patch
    }

    unsafe {
        SelectObject(hdc, old_font);
        let _ = DeleteObject(font);
    }
}

/// Hand cursor over any of the clickable rows (sessions or "+ New session"),
/// arrow elsewhere.
pub fn cursor_at(x: i32, y: i32, bounds: RECT, dpi: u32, sessions: &Sessions) -> CursorHint {
    let total = sessions.len() + 1;
    for i in 0..total {
        let Some(rect) = row_rect(&bounds, dpi, i, total) else {
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
    let total = sessions.len() + 1;
    let session_count = sessions.len();
    for i in 0..total {
        let Some(rect) = row_rect(&bounds, dpi, i, total) else {
            break;
        };
        if x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom {
            if i < session_count {
                let id = sessions.iter().nth(i)?.id;
                return Some(TileAction::FocusSession(id));
            } else {
                return Some(TileAction::CreateSession);
            }
        }
    }
    None
}
