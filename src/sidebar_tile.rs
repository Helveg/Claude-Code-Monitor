//! Sidebar tile — vertical list of mini status widgets, one per running
//! claude session, plus a "+ New session" entry at the bottom.
//!
//! Rows mix two sources:
//!   * **Local** rows back a panel-spawned `Session` with its own PTY.
//!     Clicking them focuses the session in the main terminal.
//!   * **Remote** rows back a registry entry — a claude that's running
//!     under our shim *outside* the manager. They show up as soon as
//!     the shim registers and disappear when it disconnects. Clicking
//!     a remote row spawns `<shim>.exe --session-id <uuid>` inside a
//!     fresh PTY: the shim auto-detects the existing per-session pipe
//!     and runs in subscriber mode, so the new local view becomes a
//!     live relay onto the external owner.
//!
//! Click "+ New session" → panel spawns a new session.

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;

use crate::claude_store::{self, SessionActivity};
use crate::dashboard::{CursorHint, TileAction};
use crate::native_interop::{self, Color};
use crate::registry::{self, RegistryEntry};
use crate::sessions::{Session, SessionId, SessionStatus, Sessions};

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

/// One sidebar entry. The variant carries everything needed to paint that
/// row and route a click — the painter doesn't have to re-query the
/// registry per row.
enum SidebarRow<'a> {
    Local {
        session: &'a Session,
        attachable: bool,
    },
    Remote {
        name: String,
        session_id: String,
        cwd: String,
        activity: SessionActivity,
    },
}

/// Build the row list for a paint/hit-test pass: every panel-spawned
/// session in order, then every shim-registered session that *isn't*
/// already represented locally. The dedup key is `session_id` — a
/// manager-spawned PTY also registers with the registry under the same
/// id, and surfacing it twice would just confuse the user.
fn build_rows<'a>(sessions: &'a Sessions) -> Vec<SidebarRow<'a>> {
    use std::collections::HashSet;
    let mut local_ids: HashSet<String> = HashSet::new();
    let mut rows: Vec<SidebarRow<'a>> = Vec::new();
    for session in sessions.iter() {
        let attachable = !session.session_id.is_empty()
            && registry::lookup(&session.session_id).is_some();
        if !session.session_id.is_empty() {
            local_ids.insert(session.session_id.clone());
        }
        rows.push(SidebarRow::Local { session, attachable });
    }
    let now = std::time::SystemTime::now();
    let store = claude_store::global();
    for entry in registry::snapshot() {
        if local_ids.contains(&entry.session_id) {
            continue;
        }
        let activity = store
            .lookup_by_session_id(&entry.session_id)
            .map(|h| h.activity_at(now))
            .unwrap_or(SessionActivity::Idle);
        rows.push(SidebarRow::Remote {
            name: remote_label(&entry),
            session_id: entry.session_id,
            cwd: entry.cwd,
            activity,
        });
    }
    rows
}

/// Display name for a remote (registry-only) row: the cwd's basename
/// when available, falling back to the first eight chars of the session
/// id so the row is never blank.
fn remote_label(entry: &RegistryEntry) -> String {
    let basename = std::path::Path::new(&entry.cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .unwrap_or_default();
    if !basename.is_empty() {
        return basename;
    }
    let short: String = entry.session_id.chars().take(8).collect();
    if short.is_empty() {
        "session".into()
    } else {
        short
    }
}

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

/// Map the local-session bucket to a status-dot color.
fn local_status_color(status: SessionStatus) -> Color {
    match status {
        SessionStatus::Idle => Color::from_hex(STATUS_IDLE_HEX),
        SessionStatus::Thinking => Color::from_hex(STATUS_THINKING_HEX),
        SessionStatus::NeedsAttention => Color::from_hex(STATUS_NEEDS_HEX),
    }
}

/// Map the orphan-style activity bucket to a dot color, mirroring the
/// cards-grid palette so the same session reads consistently across tiles.
/// `Stale` returns `None` — remote sessions on the sidebar should always
/// be alive (they're in the registry), so this is mostly defensive.
fn remote_status_color(activity: SessionActivity) -> Option<Color> {
    match activity {
        SessionActivity::Thinking => Some(Color::from_hex(STATUS_THINKING_HEX)),
        SessionActivity::NeedsAttention => Some(Color::from_hex(STATUS_NEEDS_HEX)),
        SessionActivity::Idle => Some(Color::from_hex(STATUS_IDLE_HEX)),
        SessionActivity::Stale => None,
    }
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

    let rows = build_rows(sessions);
    let total = rows.len() + 1; // +1 for "+ New session" row
    for (i, row) in rows.iter().enumerate() {
        let Some(rect) = row_rect(&bounds, dpi, i, total) else {
            break;
        };

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

        let (status_color, name_text, attachable, is_focused) = match row {
            SidebarRow::Local { session, attachable } => (
                Some(local_status_color(session.status)),
                session.name.as_str(),
                *attachable,
                focused == Some(session.id),
            ),
            SidebarRow::Remote { name, activity, .. } => (
                remote_status_color(*activity),
                name.as_str(),
                true,
                false,
            ),
        };

        let row_color = if is_focused { row_bg_focused } else { row_bg };
        unsafe {
            let brush = CreateSolidBrush(COLORREF(row_color.to_colorref()));
            FillRect(hdc, &body, brush);
            let _ = DeleteObject(brush);
        }

        // Status dot.
        let dot_x = body.left + pad_x;
        let dot_y = (body.top + body.bottom) / 2 - dot_size / 2;
        if let Some(color) = status_color {
            let dot_rect = RECT {
                left: dot_x,
                top: dot_y,
                right: dot_x + dot_size,
                bottom: dot_y + dot_size,
            };
            unsafe {
                let brush = CreateSolidBrush(COLORREF(color.to_colorref()));
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
        }

        // Tiny "shimmed/attachable" link icon nestled right of the dot.
        // Drawn only when the row is backed by a registered shim — local
        // sessions only get it once their shim has registered, so the icon
        // doubles as a "yes, this PTY is reachable from outside" affordance.
        let after_dot_x = dot_x + dot_size + (dot_gap / 2);
        let label_left = if attachable {
            let icon_h = ((dot_size as f64 * 0.75).round() as i32).max(5);
            let icon_y = (body.top + body.bottom) / 2 - icon_h / 2;
            let icon_w = paint_attach_icon(hdc, after_dot_x, icon_y, icon_h, orange);
            after_dot_x + icon_w + dot_gap
        } else {
            dot_x + dot_size + dot_gap
        };

        let mut label_rect = RECT {
            left: label_left,
            top: body.top,
            right: body.right,
            bottom: body.bottom,
        };
        let mut label_wide: Vec<u16> = name_text.encode_utf16().collect();
        let fg = match row {
            SidebarRow::Local { .. } => name_fg,
            // Remote rows render dimmer to signal "external, click to
            // attach" — once attached, the same session graduates to a
            // local row (the dedupe in `build_rows` filters it out of
            // the registry list once we own a PTY for the same UUID).
            SidebarRow::Remote { .. } => name_fg_dim,
        };
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

    // "+ New session" row at the end.
    let new_idx = rows.len();
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

/// Draw a tiny "attachable" glyph at `(x, y)` with overall height `h` —
/// two small open circles connected by a horizontal bar, drawn in
/// `color`. Returns the pixel width consumed so the caller can advance
/// its layout cursor. A self-contained mini-icon means no font/glyph
/// dependency on whatever the user has installed.
fn paint_attach_icon(hdc: HDC, x: i32, y: i32, h: i32, color: Color) -> i32 {
    let h = h.max(6);
    let circle = h.max(4);
    let bar = (h * 2 / 3).max(3);
    let pen_w = ((h as f64 / 6.0).round() as i32).max(1);
    let mid_y = y + h / 2;
    unsafe {
        let pen = CreatePen(PS_SOLID, pen_w, COLORREF(color.to_colorref()));
        let old_pen = SelectObject(hdc, pen);
        let null_brush = GetStockObject(NULL_BRUSH);
        let old_brush = SelectObject(hdc, null_brush);

        // Left ring.
        let lx = x;
        let _ = Ellipse(hdc, lx, y, lx + circle, y + circle);
        // Right ring, leaving a small overlap with the bar.
        let rx = lx + circle + bar - pen_w;
        let _ = Ellipse(hdc, rx, y, rx + circle, y + circle);
        // Connecting bar.
        let _ = MoveToEx(hdc, lx + circle - pen_w, mid_y, None);
        let _ = LineTo(hdc, rx + pen_w, mid_y);

        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(pen);
    }
    (circle * 2 + bar - pen_w).max(1)
}

/// Hand cursor over any clickable row — local sessions (focus) or remote
/// sessions (attach via subscriber shim) — and the "+ New session" entry.
pub fn cursor_at(x: i32, y: i32, bounds: RECT, dpi: u32, sessions: &Sessions) -> CursorHint {
    let rows = build_rows(sessions);
    let total = rows.len() + 1;
    for (i, _) in rows.iter().enumerate() {
        let Some(rect) = row_rect(&bounds, dpi, i, total) else {
            break;
        };
        if x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom {
            return CursorHint::Hand;
        }
    }
    if let Some(rect) = row_rect(&bounds, dpi, rows.len(), total) {
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
    let total = rows.len() + 1;
    for (i, row) in rows.iter().enumerate() {
        let Some(rect) = row_rect(&bounds, dpi, i, total) else {
            break;
        };
        if x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom {
            return match row {
                SidebarRow::Local { session, .. } => Some(TileAction::FocusSession(session.id)),
                SidebarRow::Remote {
                    session_id,
                    cwd,
                    name,
                    ..
                } => Some(TileAction::AttachSession {
                    session_id: session_id.clone(),
                    cwd: std::path::PathBuf::from(cwd),
                    name: name.clone(),
                }),
            };
        }
    }
    if let Some(rect) = row_rect(&bounds, dpi, rows.len(), total) {
        if x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom {
            return Some(TileAction::CreateSession);
        }
    }
    None
}
