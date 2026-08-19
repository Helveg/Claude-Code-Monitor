//! Nav tile — a two-level tree of projects and the sessions inside them.
//!
//! ```text
//! Projects                          ⌕  New
//! ▾ Claude-Code-Monitor                    +
//!   ● turn the nav into a project tree
//!   ○ cleaned up terminal code
//! ▸ beehive                            24  +
//! ```
//!
//! Level one is a project (a directory claude has been run in, discovered
//! by [`crate::projects`]); level two is one conversation. A filled dot is
//! a session this manager is running — clicking it focuses its terminal. A
//! hollow dot is a conversation from the transcript history — clicking it
//! resumes it into a new terminal. The `+` on a project row starts a fresh
//! session in that directory.
//!
//! The header band is pinned above the rows and never scrolls: `New` picks
//! a directory the scanner hasn't seen, and the magnifier slides a filter
//! box open leftwards. Everything below it walks the [`rows`] list, so
//! hit-testing can't drift from what's drawn.

use std::collections::HashSet;
use std::path::PathBuf;

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;

use crate::dashboard::{CursorHint, NavSearch, NavState, TileAction};
use crate::native_interop::{self, Color};
use crate::projects::{AttentionRow, ProjectNode};
use crate::sessions::{SessionId, SessionStatus, Sessions};

/// Design pixels at 96 DPI.
/// Header band, above the scrolling rows.
const HEADER_H: i32 = 24;
const PROJECT_ROW_H: i32 = 30;
const SESSION_ROW_H: i32 = 26;
const PAD_X: i32 = 10;
const PAD_TOP: i32 = 8;
const PAD_BOTTOM: i32 = 8;
const CHEVRON_SIZE: i32 = 7;
const CHEVRON_GAP: i32 = 8;
/// Extra left offset for session rows, on top of the project row's own
/// `PAD_X`, so children line up under their project's label.
const CHILD_INDENT: i32 = 16;
const DOT_SIZE: i32 = 7;
const DOT_GAP: i32 = 9;
const PLUS_SIZE: i32 = 10;
const PLUS_STROKE: i32 = 2;
/// Inflation applied to the `+` visual rect for hit-testing — a 10 px glyph
/// is not a comfortable click target on its own.
const PLUS_HIT_INFLATE: i32 = 7;
const COUNT_GAP: i32 = 8;

/// "New" badge: an outlined pill, no fill — the panel's controls are
/// strokes and glyphs on the grey, never button chrome.
const BADGE_W: i32 = 36;
const BADGE_H: i32 = 16;
/// Magnifier glyph box, and the gap between header controls.
const GLYPH_SIZE: i32 = 12;
const GLYPH_GAP: i32 = 8;
/// Filter box at full extension. It gives way to the left edge on a narrow
/// tile rather than overflowing it.
const SEARCH_FIELD_W: i32 = 132;
const SEARCH_FIELD_H: i32 = 18;
const SEARCH_TEXT_PAD: i32 = 6;
/// Inflation applied to header glyph rects for hit-testing.
const HEADER_HIT_INFLATE: i32 = 5;

const PROJECT_FONT_PT: i32 = 10;
const SESSION_FONT_PT: i32 = 9;
const COUNT_FONT_PT: i32 = 8;

const ROW_BG_HOVER_HEX: &str = "#2E2E2C";
const ROW_BG_FOCUSED_HEX: &str = "#3A3A38";
const PROJECT_FG_HEX: &str = "#E8E8E8";
const SESSION_FG_HEX: &str = "#C8C4BE";
const HISTORY_FG_HEX: &str = "#8B857E";
const META_FG_HEX: &str = "#7C766F";
const ORANGE_HEX: &str = "#D97757";
/// Rule under the pinned "needs attention" section once the tree scrolls
/// beneath it.
const DIVIDER_HEX: &str = "#3A3A38";
const STATUS_IDLE_HEX: &str = "#5A5F58";
const STATUS_THINKING_HEX: &str = "#5BD16B";
const STATUS_NEEDS_HEX: &str = "#E07A5F";

/// A clickable thing in the tree. Indices address the `tree` slice the
/// caller passed in — they're only valid for as long as that snapshot is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavTarget {
    /// The project row itself — collapse / expand.
    Project(usize),
    /// The `+` on a project row — start a session in that directory.
    NewSession(usize),
    /// A running session.
    Live(SessionId),
    /// A past conversation: `(project index, index into its history)`.
    History(usize, usize),
    /// A session in the "needs attention" section. Focuses the same session
    /// as its `Live` row; a target of its own so hovering one row doesn't
    /// light up both places the session appears.
    Attention(SessionId),
    /// The header's "New" badge — pick a directory to open.
    NewProject,
    /// The header's magnifier — slide the filter box open or shut.
    SearchToggle,
    /// The filter box itself. Clickable so a click inside it doesn't fall
    /// through to the row underneath and dismiss what you're typing in.
    SearchField,
}

enum RowKind<'a> {
    /// Header of the "needs attention" section. Not clickable — it's a label
    /// for the rows under it.
    AttentionHeader {
        count: usize,
    },
    Attention {
        id: SessionId,
        label: &'a str,
        reason: &'a str,
    },
    Project {
        index: usize,
        name: &'a str,
        expanded: bool,
        children: usize,
    },
    Live {
        id: SessionId,
        label: &'a str,
        status: SessionStatus,
        /// Restored but not resumed — drawn hollow, like a conversation
        /// from the history, because that is what it is until you say go.
        dormant: bool,
    },
    History {
        project: usize,
        index: usize,
        title: &'a str,
    },
}

struct Row<'a> {
    kind: RowKind<'a>,
    /// Height in design pixels; scaled at layout time.
    height: i32,
}

/// Flatten the tree into the visible row sequence. Collapsed projects
/// contribute their header only — except while a filter is active, where
/// every surviving project is open: the rows that matched are the point.
///
/// The "needs attention" section is a sibling of the projects and always sits
/// first — it only exists while something is in it, so it never costs a row
/// when nothing is waiting.
fn rows<'a>(nav: &NavState<'a>, sessions: &Sessions) -> Vec<Row<'a>> {
    let tree: &'a [ProjectNode] = nav.tree;
    let attention: &'a [AttentionRow] = nav.attention;
    let expanded: &HashSet<PathBuf> = nav.expanded;
    let filtering = !nav.search.query.trim().is_empty();

    let mut out: Vec<Row<'a>> = Vec::new();
    if !attention.is_empty() {
        out.push(Row {
            kind: RowKind::AttentionHeader {
                count: attention.len(),
            },
            height: PROJECT_ROW_H,
        });
        for row in attention {
            out.push(Row {
                kind: RowKind::Attention {
                    id: row.id,
                    label: row.label.as_str(),
                    reason: row.reason.as_str(),
                },
                height: SESSION_ROW_H,
            });
        }
    }
    for (pi, node) in tree.iter().enumerate() {
        let is_expanded = filtering || expanded.contains(&node.path);
        out.push(Row {
            kind: RowKind::Project {
                index: pi,
                name: node.name.as_str(),
                expanded: is_expanded,
                children: node.child_count(),
            },
            height: PROJECT_ROW_H,
        });
        if !is_expanded {
            continue;
        }
        for live in &node.live {
            out.push(Row {
                kind: RowKind::Live {
                    id: live.id,
                    label: live.label.as_str(),
                    status: sessions
                        .get(live.id)
                        .map(|s| s.status)
                        .unwrap_or(SessionStatus::Idle),
                    dormant: live.dormant,
                },
                height: SESSION_ROW_H,
            });
        }
        for (hi, history) in node.history.iter().enumerate() {
            out.push(Row {
                kind: RowKind::History {
                    project: pi,
                    index: hi,
                    title: history.title.as_str(),
                },
                height: SESSION_ROW_H,
            });
        }
    }
    out
}

/// The pinned header band at the top of the tile.
fn header_rect(bounds: &RECT, dpi: u32) -> RECT {
    RECT {
        bottom: (bounds.top + scaled(HEADER_H, dpi)).min(bounds.bottom),
        ..*bounds
    }
}

/// Everything under the header — the scrolling row area.
fn body_rect(bounds: &RECT, dpi: u32) -> RECT {
    RECT {
        top: (bounds.top + scaled(HEADER_H, dpi)).min(bounds.bottom),
        ..*bounds
    }
}

/// The "New" badge, pinned to the header's right edge.
fn badge_rect(bounds: &RECT, dpi: u32) -> RECT {
    let header = header_rect(bounds, dpi);
    let w = scaled(BADGE_W, dpi);
    let h = scaled(BADGE_H, dpi);
    let right = header.right - scaled(PAD_X, dpi);
    let top = (header.top + header.bottom) / 2 - h / 2;
    RECT {
        left: right - w,
        top,
        right,
        bottom: top + h,
    }
}

/// The magnifier, left of the badge.
fn search_icon_rect(bounds: &RECT, dpi: u32) -> RECT {
    let badge = badge_rect(bounds, dpi);
    let size = scaled(GLYPH_SIZE, dpi);
    let right = badge.left - scaled(GLYPH_GAP, dpi);
    let top = (badge.top + badge.bottom) / 2 - size / 2;
    RECT {
        left: right - size,
        top,
        right,
        bottom: top + size,
    }
}

/// The filter box, extending leftwards from the magnifier by `anim` of its
/// full width. `None` while it is closed, or when the tile is too narrow to
/// give it any room at all.
fn search_field_rect(bounds: &RECT, dpi: u32, anim: f64) -> Option<RECT> {
    let anim = anim.clamp(0.0, 1.0);
    if anim <= 0.0 {
        return None;
    }
    let icon = search_icon_rect(bounds, dpi);
    let right = icon.left - scaled(GLYPH_GAP, dpi);
    let full = scaled(SEARCH_FIELD_W, dpi).min(right - (bounds.left + scaled(PAD_X, dpi)));
    if full <= 0 {
        return None;
    }
    let w = (full as f64 * anim).round() as i32;
    if w < 2 {
        return None;
    }
    let h = scaled(SEARCH_FIELD_H, dpi);
    let top = (icon.top + icon.bottom) / 2 - h / 2;
    Some(RECT {
        left: right - w,
        top,
        right,
        bottom: top + h,
    })
}

/// How many leading rows stay put while the rest scrolls: the "needs
/// attention" section, header and all.
///
/// `0` when nothing is flagged, and also when the section is tall enough
/// that pinning it would bury the tree — a band that eats the tile is worse
/// than one that scrolls away, since the projects underneath become
/// unreachable.
fn pinned_run(rows: &[Row<'_>], body: &RECT, dpi: u32) -> usize {
    if !matches!(
        rows.first().map(|r| &r.kind),
        Some(RowKind::AttentionHeader { .. })
    ) {
        return 0;
    }
    let count = 1 + rows
        .iter()
        .skip(1)
        .take_while(|r| matches!(r.kind, RowKind::Attention { .. }))
        .count();
    let band = pinned_bottom(body, dpi, rows, count) - body.top;
    let room = (body.bottom - body.top).max(1);
    if band * 5 > room * 3 {
        return 0;
    }
    count
}

/// Bottom edge of the pinned band — the top padding plus the pinned rows.
fn pinned_bottom(body: &RECT, dpi: u32, rows: &[Row<'_>], pinned: usize) -> i32 {
    let scale = dpi as f64 / 96.0;
    let mut y = body.top + (PAD_TOP as f64 * scale).round() as i32;
    for row in rows.iter().take(pinned) {
        y += (row.height as f64 * scale).round() as i32;
    }
    y
}

/// Device-pixel rect of row `i`, in panel client coordinates. `body` is the
/// row area, i.e. the tile minus its header. Rows above or below it still
/// get a rect — callers skip what they can't see.
///
/// The first `pinned` rows ignore `scroll_y`: they hold the top of the body
/// while everything after them slides underneath.
fn row_rect(
    body: &RECT,
    dpi: u32,
    rows: &[Row<'_>],
    scroll_y: i32,
    i: usize,
    pinned: usize,
) -> RECT {
    let scale = dpi as f64 / 96.0;
    let offset = if i < pinned { 0 } else { scroll_y };
    let mut y = body.top + (PAD_TOP as f64 * scale).round() as i32 - offset;
    for row in rows.iter().take(i) {
        y += (row.height as f64 * scale).round() as i32;
    }
    let h = (rows[i].height as f64 * scale).round() as i32;
    RECT {
        left: body.left,
        top: y,
        right: body.right,
        bottom: y + h,
    }
}

/// Total scrollable height of the tree in device pixels. The header doesn't
/// scroll, so it isn't part of it.
pub fn content_height(dpi: u32, nav: &NavState<'_>, sessions: &Sessions) -> i32 {
    let scale = dpi as f64 / 96.0;
    let rows = rows(nav, sessions);
    let body: i32 = rows
        .iter()
        .map(|r| (r.height as f64 * scale).round() as i32)
        .sum();
    body + ((PAD_TOP + PAD_BOTTOM) as f64 * scale).round() as i32
}

/// Clamp a candidate scroll offset to the tree's content. Never scrolls
/// when everything fits.
pub fn clamp_scroll(bounds: RECT, dpi: u32, nav: &NavState<'_>, sessions: &Sessions, y: i32) -> i32 {
    let body = body_rect(&bounds, dpi);
    let visible = (body.bottom - body.top).max(0);
    let max = (content_height(dpi, nav, sessions) - visible).max(0);
    y.clamp(0, max)
}

/// The `+` glyph's visual rect on a project row. Both extents are an even
/// number of pixels so the arms can be centred exactly — with an odd extent
/// the cross-point lands half a pixel off and the four arms come out
/// visibly unequal.
fn plus_rect(row: &RECT, dpi: u32) -> RECT {
    let scale = dpi as f64 / 96.0;
    let half = ((PLUS_SIZE as f64 * scale) / 2.0).round().max(2.0) as i32;
    let pad_x = (PAD_X as f64 * scale).round() as i32;
    let right = row.right - pad_x;
    let cy = (row.top + row.bottom) / 2;
    RECT {
        left: right - half * 2,
        top: cy - half,
        right,
        bottom: cy + half,
    }
}

fn inflate(rect: RECT, by: i32) -> RECT {
    RECT {
        left: rect.left - by,
        top: rect.top - by,
        right: rect.right + by,
        bottom: rect.bottom + by,
    }
}

fn contains(rect: &RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

/// What sits under `(x, y)`, or `None` for empty space. The `+` hit
/// region wins over the project row it lives on, and the header owns every
/// point inside it — a click that lands beside its controls does nothing
/// rather than reaching the row scrolled under them.
pub fn target_at(
    x: i32,
    y: i32,
    bounds: RECT,
    dpi: u32,
    nav: &NavState<'_>,
    sessions: &Sessions,
) -> Option<NavTarget> {
    if !contains(&bounds, x, y) {
        return None;
    }
    let inflate_by = scaled(HEADER_HIT_INFLATE, dpi);
    if contains(&header_rect(&bounds, dpi), x, y) {
        if contains(&inflate(badge_rect(&bounds, dpi), inflate_by), x, y) {
            return Some(NavTarget::NewProject);
        }
        if contains(&inflate(search_icon_rect(&bounds, dpi), inflate_by), x, y) {
            return Some(NavTarget::SearchToggle);
        }
        return match search_field_rect(&bounds, dpi, nav.search.anim) {
            Some(field) if contains(&field, x, y) => Some(NavTarget::SearchField),
            _ => None,
        };
    }

    let body = body_rect(&bounds, dpi);
    let rows = rows(nav, sessions);
    let pinned = pinned_run(&rows, &body, dpi);
    // The band is drawn over whatever has scrolled under it, so a point
    // inside it can only belong to a pinned row — and a point below it can
    // only belong to a scrolling one.
    let in_band = pinned > 0 && y < pinned_bottom(&body, dpi, &rows, pinned);
    for i in 0..rows.len() {
        if in_band != (i < pinned) {
            continue;
        }
        let rect = row_rect(&body, dpi, &rows, nav.scroll_y, i, pinned);
        if rect.bottom <= body.top || rect.top >= body.bottom {
            continue;
        }
        if !contains(&rect, x, y) {
            continue;
        }
        return match rows[i].kind {
            // The section header is a label, not a control.
            RowKind::AttentionHeader { .. } => None,
            RowKind::Attention { id, .. } => Some(NavTarget::Attention(id)),
            RowKind::Project { index, .. } => Some({
                let plus = inflate(plus_rect(&rect, dpi), scaled(PLUS_HIT_INFLATE, dpi));
                if contains(&plus, x, y) {
                    NavTarget::NewSession(index)
                } else {
                    NavTarget::Project(index)
                }
            }),
            RowKind::Live { id, .. } => Some(NavTarget::Live(id)),
            RowKind::History { project, index, .. } => Some(NavTarget::History(project, index)),
        };
    }
    None
}

pub fn cursor_at(
    x: i32,
    y: i32,
    bounds: RECT,
    dpi: u32,
    nav: &NavState<'_>,
    sessions: &Sessions,
) -> CursorHint {
    match target_at(x, y, bounds, dpi, nav, sessions) {
        // The filter box takes typing, so it gets the text cursor.
        Some(NavTarget::SearchField) => CursorHint::IBeam,
        Some(_) => CursorHint::Hand,
        None => CursorHint::Arrow,
    }
}

pub fn handle_lbutton_down(
    x: i32,
    y: i32,
    bounds: RECT,
    dpi: u32,
    nav: &NavState<'_>,
    sessions: &Sessions,
) -> Option<TileAction> {
    let target = target_at(x, y, bounds, dpi, nav, sessions)?;
    action_for(target, nav.tree)
}

/// Translate a nav target into the panel-level action it performs.
/// Returns `None` when the target addresses a stale snapshot index.
pub fn action_for(target: NavTarget, tree: &[ProjectNode]) -> Option<TileAction> {
    match target {
        NavTarget::Project(i) => Some(TileAction::ToggleProject(tree.get(i)?.path.clone())),
        NavTarget::NewSession(i) => Some(TileAction::NewSessionIn(tree.get(i)?.path.clone())),
        NavTarget::Live(id) | NavTarget::Attention(id) => Some(TileAction::FocusSession(id)),
        NavTarget::History(pi, hi) => {
            let node = tree.get(pi)?;
            let history = node.history.get(hi)?;
            Some(TileAction::ResumeSession {
                cwd: node.path.clone(),
                session_id: history.session_id.clone(),
            })
        }
        NavTarget::NewProject => Some(TileAction::PickNewProject),
        NavTarget::SearchToggle => Some(TileAction::ToggleSearch),
        // Clicking inside the open box is just "keep typing".
        NavTarget::SearchField => None,
    }
}

fn scaled(design_px: i32, dpi: u32) -> i32 {
    (design_px as f64 * dpi as f64 / 96.0).round() as i32
}

fn status_color(status: SessionStatus) -> Color {
    match status {
        SessionStatus::Idle => Color::from_hex(STATUS_IDLE_HEX),
        SessionStatus::Thinking => Color::from_hex(STATUS_THINKING_HEX),
        SessionStatus::NeedsAttention => Color::from_hex(STATUS_NEEDS_HEX),
    }
}

fn make_font(dpi: u32, point_size: i32, weight: i32) -> HFONT {
    let face = native_interop::wide_str("Segoe UI");
    unsafe {
        CreateFontW(
            -(point_size * dpi as i32 / 72),
            0,
            0,
            0,
            weight,
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
    }
}

fn draw_text(hdc: HDC, text: &str, rect: RECT, color: Color, flags: DRAW_TEXT_FORMAT) {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    if wide.is_empty() {
        return;
    }
    let mut rect = rect;
    unsafe {
        let _ = SetTextColor(hdc, COLORREF(color.to_colorref()));
        let _ = DrawTextW(hdc, &mut wide, &mut rect, flags);
    }
}

fn fill(hdc: HDC, rect: &RECT, color: Color) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color.to_colorref()));
        FillRect(hdc, rect, brush);
        let _ = DeleteObject(brush);
    }
}

/// Draw the tree into `bounds`. The caller owns the background — the panel
/// chrome already covers the sidebar, and inside the terminal grid the tree
/// sits on its cell's own fill.
pub fn paint(
    hdc: HDC,
    bounds: RECT,
    dpi: u32,
    nav: &NavState<'_>,
    sessions: &Sessions,
    focused: Option<SessionId>,
) {
    let rows = rows(nav, sessions);
    let body = body_rect(&bounds, dpi);

    let project_font = make_font(dpi, PROJECT_FONT_PT, FW_MEDIUM.0 as i32);
    let session_font = make_font(dpi, SESSION_FONT_PT, FW_NORMAL.0 as i32);
    let count_font = make_font(dpi, COUNT_FONT_PT, FW_NORMAL.0 as i32);
    let old_font = unsafe {
        let _ = SetBkMode(hdc, TRANSPARENT);
        SelectObject(hdc, project_font)
    };

    let pad_x = scaled(PAD_X, dpi);
    let text_flags = DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX;

    paint_header(
        hdc,
        &bounds,
        dpi,
        nav,
        project_font,
        session_font,
        count_font,
    );

    // Scrolled rows can straddle the tile edges; clip so a half-visible
    // row can't bleed into the header above or the tile below. Narrowing
    // the caller's clip rather than replacing it keeps the grid's own
    // cell clipping intact.
    let saved_clip = unsafe {
        let saved = SaveDC(hdc);
        let _ = IntersectClipRect(hdc, body.left, body.top, body.right, body.bottom);
        saved
    };

    let pinned = pinned_run(&rows, &body, dpi);
    let band_bottom = pinned_bottom(&body, dpi, &rows, pinned);

    for i in 0..rows.len() {
        let rect = row_rect(&body, dpi, &rows, nav.scroll_y, i, pinned);
        if rect.bottom <= body.top || rect.top >= body.bottom {
            continue;
        }
        // Rows that scroll are held below the band, so nothing slides
        // through the pinned section on its way past.
        let scrolled_clip = (i >= pinned && pinned > 0).then(|| unsafe {
            let saved = SaveDC(hdc);
            let _ = IntersectClipRect(hdc, body.left, band_bottom, body.right, body.bottom);
            saved
        });

        match rows[i].kind {
            RowKind::AttentionHeader { count } => {
                let label_rect = RECT {
                    left: rect.left + pad_x,
                    top: rect.top,
                    right: rect.right - pad_x,
                    bottom: rect.bottom,
                };
                draw_text(
                    hdc,
                    &format!("needs attention ({count})"),
                    label_rect,
                    Color::from_hex(STATUS_NEEDS_HEX),
                    text_flags,
                );
            }
            RowKind::Attention { id, label, reason } => {
                let hovered = nav.hovered == Some(NavTarget::Attention(id));
                if focused == Some(id) {
                    fill(hdc, &rect, Color::from_hex(ROW_BG_FOCUSED_HEX));
                } else if hovered {
                    fill(hdc, &rect, Color::from_hex(ROW_BG_HOVER_HEX));
                }
                let dot_x = rect.left + pad_x + scaled(CHILD_INDENT, dpi);
                paint_dot(
                    hdc,
                    dot_x,
                    rect,
                    Color::from_hex(STATUS_NEEDS_HEX),
                    true,
                    dpi,
                );
                let old = unsafe { SelectObject(hdc, session_font) };
                // The reason claude gave sits right-aligned; the session's own
                // label takes whatever is left.
                let mut reason_wide: Vec<u16> = reason.encode_utf16().collect();
                let mut reason_size = SIZE::default();
                unsafe {
                    let _ = GetTextExtentPoint32W(hdc, &reason_wide, &mut reason_size);
                }
                let reason_left = (rect.right - pad_x - reason_size.cx).max(rect.left);
                unsafe {
                    let _ = SetTextColor(hdc, COLORREF(Color::from_hex(META_FG_HEX).to_colorref()));
                    let mut r = RECT {
                        left: reason_left,
                        top: rect.top,
                        right: rect.right - pad_x,
                        bottom: rect.bottom,
                    };
                    let _ = DrawTextW(hdc, &mut reason_wide, &mut r, text_flags);
                }
                let label_rect = RECT {
                    left: dot_x + scaled(DOT_SIZE + DOT_GAP, dpi),
                    top: rect.top,
                    right: reason_left - scaled(COUNT_GAP, dpi),
                    bottom: rect.bottom,
                };
                draw_text(
                    hdc,
                    label,
                    label_rect,
                    Color::from_hex(SESSION_FG_HEX),
                    text_flags,
                );
                unsafe { SelectObject(hdc, old) };
            }
            RowKind::Project {
                index,
                name,
                expanded,
                children,
            } => {
                let row_hovered = nav.hovered == Some(NavTarget::Project(index));
                if row_hovered {
                    fill(hdc, &rect, Color::from_hex(ROW_BG_HOVER_HEX));
                }

                let chevron_size = scaled(CHEVRON_SIZE, dpi);
                let chevron_x = rect.left + pad_x;
                paint_chevron(
                    hdc,
                    chevron_x,
                    (rect.top + rect.bottom) / 2,
                    chevron_size,
                    expanded,
                    dpi,
                );

                let plus = plus_rect(&rect, dpi);
                let plus_hovered = nav.hovered == Some(NavTarget::NewSession(index));
                paint_plus(hdc, &plus, plus_hovered, dpi);

                // Child count sits left of the `+`, and only when the
                // project is collapsed — expanded, the rows speak for
                // themselves.
                let mut label_right = plus.left - scaled(COUNT_GAP, dpi);
                if !expanded && children > 0 {
                    let count = children.to_string();
                    let old = unsafe { SelectObject(hdc, count_font) };
                    let mut wide: Vec<u16> = count.encode_utf16().collect();
                    let mut size = SIZE::default();
                    unsafe {
                        let _ = GetTextExtentPoint32W(hdc, &wide, &mut size);
                    }
                    let count_rect = RECT {
                        left: label_right - size.cx,
                        top: rect.top,
                        right: label_right,
                        bottom: rect.bottom,
                    };
                    unsafe {
                        let _ = SetTextColor(hdc, COLORREF(Color::from_hex(META_FG_HEX).to_colorref()));
                        let mut r = count_rect;
                        let _ = DrawTextW(hdc, &mut wide, &mut r, text_flags);
                        SelectObject(hdc, old);
                    }
                    label_right = count_rect.left - scaled(COUNT_GAP, dpi);
                }

                let label_rect = RECT {
                    left: chevron_x + chevron_size + scaled(CHEVRON_GAP, dpi),
                    top: rect.top,
                    right: label_right,
                    bottom: rect.bottom,
                };
                draw_text(
                    hdc,
                    name,
                    label_rect,
                    Color::from_hex(PROJECT_FG_HEX),
                    text_flags,
                );
            }
            RowKind::Live {
                id,
                label,
                status,
                dormant,
            } => {
                let hovered = nav.hovered == Some(NavTarget::Live(id));
                if focused == Some(id) {
                    fill(hdc, &rect, Color::from_hex(ROW_BG_FOCUSED_HEX));
                } else if hovered {
                    fill(hdc, &rect, Color::from_hex(ROW_BG_HOVER_HEX));
                }
                let dot_x = rect.left + pad_x + scaled(CHILD_INDENT, dpi);
                let (dot_color, fg) = if dormant {
                    (Color::from_hex(META_FG_HEX), Color::from_hex(HISTORY_FG_HEX))
                } else {
                    (status_color(status), Color::from_hex(SESSION_FG_HEX))
                };
                paint_dot(hdc, dot_x, rect, dot_color, !dormant, dpi);
                let label_rect = RECT {
                    left: dot_x + scaled(DOT_SIZE + DOT_GAP, dpi),
                    top: rect.top,
                    right: rect.right - pad_x,
                    bottom: rect.bottom,
                };
                let old = unsafe { SelectObject(hdc, session_font) };
                draw_text(hdc, label, label_rect, fg, text_flags);
                unsafe { SelectObject(hdc, old) };
            }
            RowKind::History {
                project,
                index,
                title,
            } => {
                let hovered = nav.hovered == Some(NavTarget::History(project, index));
                if hovered {
                    fill(hdc, &rect, Color::from_hex(ROW_BG_HOVER_HEX));
                }
                let dot_x = rect.left + pad_x + scaled(CHILD_INDENT, dpi);
                paint_dot(hdc, dot_x, rect, Color::from_hex(META_FG_HEX), false, dpi);
                let label_rect = RECT {
                    left: dot_x + scaled(DOT_SIZE + DOT_GAP, dpi),
                    top: rect.top,
                    right: rect.right - pad_x,
                    bottom: rect.bottom,
                };
                let old = unsafe { SelectObject(hdc, session_font) };
                draw_text(
                    hdc,
                    title,
                    label_rect,
                    Color::from_hex(HISTORY_FG_HEX),
                    text_flags,
                );
                unsafe { SelectObject(hdc, old) };
            }
        }

        if let Some(saved) = scrolled_clip {
            unsafe {
                let _ = RestoreDC(hdc, saved);
            }
        }
    }

    // Bottom border for the pinned band, drawn only once something has
    // actually slid under it — at rest the section is just the top of the
    // list and needs no rule.
    if pinned > 0 && nav.scroll_y > 0 {
        let divider = RECT {
            left: body.left + pad_x,
            top: band_bottom,
            right: body.right - pad_x,
            bottom: band_bottom + scaled(1, dpi).max(1),
        };
        fill(hdc, &divider, Color::from_hex(DIVIDER_HEX));
    }

    unsafe {
        // RestoreDC first: it puts our own font back on the DC, so the
        // caller's font has to be reselected after it, before the fonts
        // can be deleted.
        let _ = RestoreDC(hdc, saved_clip);
        SelectObject(hdc, old_font);
        let _ = DeleteObject(project_font);
        let _ = DeleteObject(session_font);
        let _ = DeleteObject(count_font);
    }
}

/// The pinned header: title on the left, then the filter box, the
/// magnifier and the "New" badge running to the right edge. The title is
/// given whatever the controls leave it, so the box sliding open simply
/// pushes it out of the way.
fn paint_header(
    hdc: HDC,
    bounds: &RECT,
    dpi: u32,
    nav: &NavState<'_>,
    title_font: HFONT,
    field_font: HFONT,
    badge_font: HFONT,
) {
    let header = header_rect(bounds, dpi);
    let badge = badge_rect(bounds, dpi);
    let icon = search_icon_rect(bounds, dpi);
    let field = search_field_rect(bounds, dpi, nav.search.anim);

    paint_badge(
        hdc,
        &badge,
        nav.hovered == Some(NavTarget::NewProject),
        dpi,
        badge_font,
    );
    paint_search_icon(
        hdc,
        &icon,
        nav.hovered == Some(NavTarget::SearchToggle) || nav.search.open,
        dpi,
    );
    if let Some(field) = field {
        paint_search_field(hdc, &field, dpi, nav.search, field_font);
    }

    let title_right = field.map(|f| f.left).unwrap_or(icon.left) - scaled(GLYPH_GAP, dpi);
    let title_rect = RECT {
        left: header.left + scaled(PAD_X, dpi),
        top: header.top,
        right: title_right,
        bottom: header.bottom,
    };
    if title_rect.right <= title_rect.left {
        return;
    }
    let old = unsafe { SelectObject(hdc, title_font) };
    draw_text(
        hdc,
        "Projects",
        title_rect,
        Color::from_hex(PROJECT_FG_HEX),
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
    );
    unsafe { SelectObject(hdc, old) };
}

/// "New": an outlined pill with the word inside, both in one colour so the
/// whole badge lights up together on hover.
fn paint_badge(hdc: HDC, rect: &RECT, hovered: bool, dpi: u32, font: HFONT) {
    let color = if hovered {
        Color::from_hex(ORANGE_HEX)
    } else {
        Color::from_hex(META_FG_HEX)
    };
    stroke_pill(hdc, rect, color, scaled(1, dpi).max(1));
    unsafe {
        let old_font = SelectObject(hdc, font);
        draw_text_centered(hdc, "New", rect, color);
        SelectObject(hdc, old_font);
    }
}

/// Outlined pill, no fill.
///
/// The stroke is inset by half the pen width: GDI centres a wide pen on the
/// path and treats the right and bottom edges as exclusive, so drawing on
/// the rect itself puts the top arc a pixel outside it and the bottom arc a
/// pixel inside — which reads as two different corner radii.
fn stroke_pill(hdc: HDC, rect: &RECT, color: Color, pen_w: i32) {
    let half = pen_w / 2;
    let left = rect.left + half;
    let top = rect.top + half;
    let right = rect.right - half;
    let bottom = rect.bottom - half;
    if right <= left || bottom <= top {
        return;
    }
    // Corner ellipse as tall as the pill is: semicircular ends.
    let diameter = bottom - top;
    unsafe {
        let pen = CreatePen(PS_SOLID, pen_w, COLORREF(color.to_colorref()));
        let old_pen = SelectObject(hdc, pen);
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        let _ = RoundRect(hdc, left, top, right, bottom, diameter, diameter);
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(pen);
    }
}

/// Draw one line centred on `rect`, measured rather than left to
/// `DT_CENTER | DT_VCENTER`. `DT_VCENTER` centres the font's *line box*,
/// and all of its internal leading sits above the glyphs, so the text comes
/// out visibly low — a couple of pixels at 96 DPI and twice that at 192.
fn draw_text_centered(hdc: HDC, text: &str, rect: &RECT, color: Color) {
    let wide: Vec<u16> = text.encode_utf16().collect();
    if wide.is_empty() {
        return;
    }
    unsafe {
        let mut size = SIZE::default();
        let _ = GetTextExtentPoint32W(hdc, &wide, &mut size);
        let mut tm = TEXTMETRICW::default();
        let _ = GetTextMetricsW(hdc, &mut tm);
        // What is actually inked: cap top to descender bottom.
        let glyphs_h = tm.tmAscent - tm.tmInternalLeading + tm.tmDescent;
        let x = (rect.left + rect.right - size.cx) / 2;
        // ExtTextOut anchors the top of the cell, which starts at the
        // internal leading rather than at the glyphs.
        let y = (rect.top + rect.bottom - glyphs_h) / 2 - tm.tmInternalLeading;
        let _ = SetTextColor(hdc, COLORREF(color.to_colorref()));
        let _ = ExtTextOutW(
            hdc,
            x,
            y,
            ETO_OPTIONS(0),
            None,
            PCWSTR::from_raw(wide.as_ptr()),
            wide.len() as u32,
            None,
        );
    }
}

/// Magnifier: a circle in the upper-left of the glyph box with a handle
/// running to its lower-right corner.
fn paint_search_icon(hdc: HDC, rect: &RECT, hovered: bool, dpi: u32) {
    let color = if hovered {
        Color::from_hex(ORANGE_HEX)
    } else {
        Color::from_hex(META_FG_HEX)
    };
    let stroke = scaled(1, dpi).max(1);
    let size = rect.right - rect.left;
    let lens = size * 3 / 4;
    unsafe {
        let pen = CreatePen(PS_SOLID, stroke, COLORREF(color.to_colorref()));
        let old_pen = SelectObject(hdc, pen);
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        let _ = Ellipse(hdc, rect.left, rect.top, rect.left + lens, rect.top + lens);
        // Start the handle inside the rim so the two strokes meet cleanly.
        let _ = MoveToEx(
            hdc,
            rect.left + lens - stroke,
            rect.top + lens - stroke,
            None,
        );
        let _ = LineTo(hdc, rect.right, rect.bottom);
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(pen);
    }
}

/// The filter box: outlined, with the query and a caret. Long queries scroll
/// so the caret — the end of what you typed — stays in view.
fn paint_search_field(hdc: HDC, rect: &RECT, dpi: u32, search: NavSearch<'_>, font: HFONT) {
    let border = Color::from_hex(if search.open {
        ORANGE_HEX
    } else {
        META_FG_HEX
    });
    stroke_pill(hdc, rect, border, scaled(1, dpi).max(1));

    let pad = scaled(SEARCH_TEXT_PAD, dpi);
    let inner = RECT {
        left: rect.left + pad,
        top: rect.top,
        right: rect.right - pad,
        bottom: rect.bottom,
    };
    if inner.right <= inner.left {
        return;
    }

    let saved = unsafe {
        let saved = SaveDC(hdc);
        let _ = IntersectClipRect(hdc, inner.left, rect.top, inner.right, rect.bottom);
        saved
    };
    let old_font = unsafe { SelectObject(hdc, font) };

    let (text, color) = if search.query.is_empty() {
        ("filter", Color::from_hex(META_FG_HEX))
    } else {
        (search.query, Color::from_hex(SESSION_FG_HEX))
    };
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    let mut size = SIZE::default();
    unsafe {
        let _ = GetTextExtentPoint32W(hdc, &wide, &mut size);
    }
    // Anchor the tail to the right edge once the text outgrows the box.
    let overflow = (size.cx - (inner.right - inner.left)).max(0);
    let text_rect = RECT {
        left: inner.left - overflow,
        ..inner
    };
    unsafe {
        let _ = SetTextColor(hdc, COLORREF(color.to_colorref()));
        let mut r = text_rect;
        let _ = DrawTextW(
            hdc,
            &mut wide,
            &mut r,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
    }

    if search.open {
        let caret_x = if search.query.is_empty() {
            inner.left
        } else {
            (text_rect.left + size.cx + scaled(1, dpi)).min(inner.right - scaled(1, dpi))
        };
        let caret = RECT {
            left: caret_x,
            top: rect.top + scaled(4, dpi),
            right: caret_x + scaled(1, dpi).max(1),
            bottom: rect.bottom - scaled(4, dpi),
        };
        fill(hdc, &caret, Color::from_hex(SESSION_FG_HEX));
    }

    unsafe {
        SelectObject(hdc, old_font);
        let _ = RestoreDC(hdc, saved);
    }
}

/// Disclosure triangle: two strokes forming a caret, pointing right when
/// collapsed and down when expanded.
fn paint_chevron(hdc: HDC, left: i32, center_y: i32, size: i32, expanded: bool, dpi: u32) {
    let stroke = scaled(2, dpi).max(1);
    let half = size / 2;
    let color = Color::from_hex(META_FG_HEX);
    unsafe {
        let pen = CreatePen(PS_SOLID, stroke, COLORREF(color.to_colorref()));
        let old_pen = SelectObject(hdc, pen);
        if expanded {
            let top = center_y - half / 2;
            let _ = MoveToEx(hdc, left, top, None);
            let _ = LineTo(hdc, left + half, top + half);
            let _ = LineTo(hdc, left + size, top);
        } else {
            let left = left + half / 2;
            let _ = MoveToEx(hdc, left, center_y - half, None);
            let _ = LineTo(hdc, left + half, center_y);
            let _ = LineTo(hdc, left, center_y + half);
        }
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(pen);
    }
}

/// Two crossed bars. Turns orange while hovered — the only colour cue that
/// the glyph is a button.
///
/// The arms are filled rects rather than pen strokes: a wide GDI pen puts
/// its extra pixel on one side of the path and clips the far endpoint, so
/// line-drawn arms come out a pixel long on the right and bottom. Filling
/// bars around the exact centre keeps all four arms the same length.
fn paint_plus(hdc: HDC, rect: &RECT, hovered: bool, dpi: u32) {
    let half_stroke = ((PLUS_STROKE as f64 * dpi as f64 / 96.0) / 2.0)
        .round()
        .max(1.0) as i32;
    let color = if hovered {
        Color::from_hex(ORANGE_HEX)
    } else {
        Color::from_hex(META_FG_HEX)
    };
    let cx = (rect.left + rect.right) / 2;
    let cy = (rect.top + rect.bottom) / 2;
    fill(
        hdc,
        &RECT {
            left: rect.left,
            top: cy - half_stroke,
            right: rect.right,
            bottom: cy + half_stroke,
        },
        color,
    );
    fill(
        hdc,
        &RECT {
            left: cx - half_stroke,
            top: rect.top,
            right: cx + half_stroke,
            bottom: rect.bottom,
        },
        color,
    );
}

/// Session marker: filled for a running session, hollow for one that only
/// exists in the transcript history.
fn paint_dot(hdc: HDC, left: i32, row: RECT, color: Color, filled: bool, dpi: u32) {
    let size = scaled(DOT_SIZE, dpi);
    let top = (row.top + row.bottom) / 2 - size / 2;
    unsafe {
        if filled {
            let brush = CreateSolidBrush(COLORREF(color.to_colorref()));
            let region = CreateRoundRectRgn(left, top, left + size + 1, top + size + 1, size, size);
            let _ = FillRgn(hdc, region, brush);
            let _ = DeleteObject(region);
            let _ = DeleteObject(brush);
        } else {
            let pen = CreatePen(PS_SOLID, 1, COLORREF(color.to_colorref()));
            let old_pen = SelectObject(hdc, pen);
            let null_brush = GetStockObject(NULL_BRUSH);
            let old_brush = SelectObject(hdc, null_brush);
            let _ = Ellipse(hdc, left, top, left + size, top + size);
            SelectObject(hdc, old_pen);
            SelectObject(hdc, old_brush);
            let _ = DeleteObject(pen);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects::{HistorySession, LiveRow};
    use std::time::SystemTime;

    fn node(name: &str, live: usize, history: usize) -> ProjectNode {
        ProjectNode {
            path: PathBuf::from(format!("C:\\git\\{name}")),
            name: name.to_string(),
            live: (0..live)
                .map(|i| LiveRow {
                    id: i as SessionId + 1,
                    label: format!("live {i}"),
                    dormant: false,
                })
                .collect(),
            history: (0..history)
                .map(|i| HistorySession {
                    session_id: format!("id-{i}"),
                    title: format!("history {i}"),
                    last_modified: SystemTime::UNIX_EPOCH,
                })
                .collect(),
        }
    }

    #[test]
    fn the_plus_glyph_is_symmetric_at_every_dpi() {
        let row = RECT {
            left: 0,
            top: 0,
            right: 220,
            bottom: PROJECT_ROW_H,
        };
        for dpi in [96, 120, 144, 168, 192, 240] {
            let r = plus_rect(&row, dpi);
            let w = r.right - r.left;
            let h = r.bottom - r.top;
            assert_eq!(w, h, "dpi {dpi}: not square");
            // Even extents put the centre on a pixel boundary, which is what
            // lets the four arms come out the same length.
            assert_eq!(w % 2, 0, "dpi {dpi}: odd width {w}");
            let cx = (r.left + r.right) / 2;
            let cy = (r.top + r.bottom) / 2;
            assert_eq!(cx - r.left, r.right - cx, "dpi {dpi}: horizontal arms");
            assert_eq!(cy - r.top, r.bottom - cy, "dpi {dpi}: vertical arms");
        }
    }

    fn attention(id: SessionId) -> AttentionRow {
        AttentionRow {
            id,
            label: format!("session {id}"),
            reason: "input needed".to_string(),
        }
    }

    /// Nav over a snapshot, with no filter and nothing hovered.
    fn nav_state<'a>(
        tree: &'a [ProjectNode],
        attention: &'a [AttentionRow],
        expanded: &'a HashSet<PathBuf>,
        scroll_y: i32,
    ) -> NavState<'a> {
        NavState {
            tree,
            attention,
            expanded,
            scroll_y,
            hovered: None,
            search: NavSearch::default(),
        }
    }

    /// A 220-wide tile, the sidebar's width.
    fn tile() -> RECT {
        RECT {
            left: 0,
            top: 0,
            right: 220,
            bottom: 400,
        }
    }

    /// Design-pixel y of the first row's middle: past the pinned header,
    /// past the top padding.
    fn first_row_y() -> i32 {
        HEADER_H + PAD_TOP + PROJECT_ROW_H / 2
    }

    /// The section is a sibling of the projects: header first, its rows next,
    /// then the tree — and nothing at all when nothing is waiting.
    #[test]
    fn the_attention_section_leads_and_disappears_when_empty() {
        let tree = vec![node("a", 1, 0)];
        let sessions = Sessions::new();
        let expanded = HashSet::new();

        let none = rows(&nav_state(&tree, &[], &expanded, 0), &sessions);
        assert!(matches!(none[0].kind, RowKind::Project { .. }));

        let flagged = vec![attention(1), attention(2)];
        let with = rows(&nav_state(&tree, &flagged, &expanded, 0), &sessions);
        assert!(matches!(
            with[0].kind,
            RowKind::AttentionHeader { count: 2 }
        ));
        assert!(matches!(with[1].kind, RowKind::Attention { id: 1, .. }));
        assert!(matches!(with[2].kind, RowKind::Attention { id: 2, .. }));
        assert!(matches!(with[3].kind, RowKind::Project { index: 0, .. }));
    }

    /// A flagged session is clickable from the section and focuses the same
    /// terminal its row under the project would, but hit-tests as its own
    /// target so only one row lights up on hover.
    #[test]
    fn attention_rows_focus_their_session() {
        let bounds = tile();
        let tree = vec![node("a", 1, 0)];
        let flagged = vec![attention(1)];
        let expanded = HashSet::new();
        let nav = nav_state(&tree, &flagged, &expanded, 0);
        let sessions = Sessions::new();
        // The section's own header is inert.
        assert_eq!(
            target_at(40, first_row_y(), bounds, 96, &nav, &sessions),
            None
        );

        let row_y = first_row_y() + PROJECT_ROW_H / 2 + SESSION_ROW_H / 2;
        assert_eq!(
            target_at(40, row_y, bounds, 96, &nav, &sessions),
            Some(NavTarget::Attention(1))
        );
        assert!(matches!(
            action_for(NavTarget::Attention(1), &tree),
            Some(TileAction::FocusSession(1))
        ));
    }

    /// The bug this fixes: scroll down the tree and the flagged sessions
    /// used to leave with it. They hold the top of the body instead.
    #[test]
    fn the_attention_section_stays_put_while_the_tree_scrolls() {
        let bounds = tile();
        let body = RECT {
            top: HEADER_H,
            ..bounds
        };
        let tree = vec![node("a", 0, 12)];
        let flagged = vec![attention(1)];
        let expanded: HashSet<PathBuf> = tree.iter().map(|n| n.path.clone()).collect();
        let sessions = Sessions::new();

        // Scrolled well past where the section would otherwise have gone.
        let nav = nav_state(&tree, &flagged, &expanded, PROJECT_ROW_H * 4);
        let rows = rows(&nav, &sessions);
        let pinned = pinned_run(&rows, &body, 96);
        assert_eq!(pinned, 2, "header plus its one row");

        // Its rows sit where they do at rest, whatever the scroll offset.
        let resting = nav_state(&tree, &flagged, &expanded, 0);
        for i in 0..pinned {
            assert_eq!(
                row_rect(&body, 96, &rows, nav.scroll_y, i, pinned).top,
                row_rect(&body, 96, &rows, resting.scroll_y, i, pinned).top,
                "pinned row {i} moved"
            );
        }
        // And it is still the thing you click up there.
        let row_y = HEADER_H + PAD_TOP + PROJECT_ROW_H + SESSION_ROW_H / 2;
        assert_eq!(
            target_at(40, row_y, bounds, 96, &nav, &sessions),
            Some(NavTarget::Attention(1))
        );
    }

    /// Rows that have slid under the band belong to the band, not to them.
    #[test]
    fn a_row_scrolled_under_the_band_is_not_clickable_there() {
        let bounds = tile();
        let tree = vec![node("a", 0, 12)];
        let flagged = vec![attention(1)];
        let expanded: HashSet<PathBuf> = tree.iter().map(|n| n.path.clone()).collect();
        let sessions = Sessions::new();
        let nav = nav_state(&tree, &flagged, &expanded, PROJECT_ROW_H * 4);

        // A y inside the band never resolves to a scrolling row — at most
        // to the pinned one that owns that strip.
        for y in (HEADER_H + PAD_TOP)..(HEADER_H + PAD_TOP + PROJECT_ROW_H + SESSION_ROW_H) {
            match target_at(40, y, bounds, 96, &nav, &sessions) {
                None | Some(NavTarget::Attention(_)) => {}
                other => panic!("y {y} reached {other:?} under the pinned band"),
            }
        }
    }

    /// A tile that is nearly all attention rows would leave no way to reach
    /// the projects, so the section goes back to scrolling.
    #[test]
    fn a_section_that_would_bury_the_tree_is_not_pinned() {
        let short = RECT {
            left: 0,
            top: 0,
            right: 220,
            bottom: 90,
        };
        let tree = vec![node("a", 0, 4)];
        let flagged: Vec<AttentionRow> = (1..=4).map(attention).collect();
        let expanded = HashSet::new();
        let sessions = Sessions::new();
        let nav = nav_state(&tree, &flagged, &expanded, 0);
        let rows = rows(&nav, &sessions);
        assert_eq!(pinned_run(&rows, &short, 96), 0);

        // With room, the same section pins.
        let roomy = RECT {
            bottom: 600,
            ..short
        };
        assert_eq!(pinned_run(&rows, &roomy, 96), 5);
    }

    #[test]
    fn collapsed_projects_contribute_one_row_each() {
        let tree = vec![node("a", 1, 2), node("b", 0, 3)];
        let expanded = HashSet::new();
        let rows = rows(&nav_state(&tree, &[], &expanded, 0), &Sessions::new());
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn expanding_reveals_live_sessions_before_history() {
        let tree = vec![node("a", 1, 2)];
        let expanded: HashSet<PathBuf> = tree.iter().map(|n| n.path.clone()).collect();
        let rows = rows(&nav_state(&tree, &[], &expanded, 0), &Sessions::new());
        assert_eq!(rows.len(), 4);
        assert!(matches!(rows[0].kind, RowKind::Project { .. }));
        assert!(matches!(rows[1].kind, RowKind::Live { .. }));
        assert!(matches!(rows[2].kind, RowKind::History { index: 0, .. }));
        assert!(matches!(rows[3].kind, RowKind::History { index: 1, .. }));
    }

    /// A filter opens what it matched: leaving the hits collapsed behind a
    /// project row would hide the very rows the user searched for.
    #[test]
    fn a_filter_expands_every_project_it_left_standing() {
        let tree = vec![node("a", 1, 2)];
        let expanded = HashSet::new();
        let mut nav = nav_state(&tree, &[], &expanded, 0);
        assert_eq!(rows(&nav, &Sessions::new()).len(), 1);
        nav.search = NavSearch {
            open: true,
            query: "history",
            anim: 1.0,
        };
        assert_eq!(rows(&nav, &Sessions::new()).len(), 4);
    }

    #[test]
    fn hit_testing_maps_rows_to_targets() {
        let bounds = tile();
        let tree = vec![node("a", 1, 1)];
        let expanded: HashSet<PathBuf> = tree.iter().map(|n| n.path.clone()).collect();
        let nav = nav_state(&tree, &[], &expanded, 0);
        let sessions = Sessions::new();

        // Project row, well left of the `+`.
        let project_y = first_row_y();
        assert_eq!(
            target_at(40, project_y, bounds, 96, &nav, &sessions),
            Some(NavTarget::Project(0))
        );
        // Same row, on the `+`.
        assert_eq!(
            target_at(bounds.right - PAD_X - 2, project_y, bounds, 96, &nav, &sessions),
            Some(NavTarget::NewSession(0))
        );
        // First child is the live session, second the history entry.
        let live_y = project_y + PROJECT_ROW_H / 2 + SESSION_ROW_H / 2;
        assert_eq!(
            target_at(40, live_y, bounds, 96, &nav, &sessions),
            Some(NavTarget::Live(1))
        );
        let history_y = live_y + SESSION_ROW_H;
        assert_eq!(
            target_at(40, history_y, bounds, 96, &nav, &sessions),
            Some(NavTarget::History(0, 0))
        );
        // Past the last row.
        assert_eq!(target_at(40, 390, bounds, 96, &nav, &sessions), None);
    }

    /// The header's controls, right to left, and the dead space beside them.
    #[test]
    fn the_header_owns_its_band() {
        let bounds = tile();
        let tree = vec![node("a", 0, 0)];
        let expanded = HashSet::new();
        let mut nav = nav_state(&tree, &[], &expanded, 0);
        let sessions = Sessions::new();

        let badge = badge_rect(&bounds, 96);
        let icon = search_icon_rect(&bounds, 96);
        let mid = |r: &RECT| ((r.left + r.right) / 2, (r.top + r.bottom) / 2);

        let (bx, by) = mid(&badge);
        assert_eq!(
            target_at(bx, by, bounds, 96, &nav, &sessions),
            Some(NavTarget::NewProject)
        );
        let (ix, iy) = mid(&icon);
        assert_eq!(
            target_at(ix, iy, bounds, 96, &nav, &sessions),
            Some(NavTarget::SearchToggle)
        );
        // The title side of the band is not a control, and must not fall
        // through to the row scrolled under it.
        assert_eq!(target_at(20, by, bounds, 96, &nav, &sessions), None);

        // Open, the box claims the space it slid into.
        nav.search = NavSearch {
            open: true,
            query: "",
            anim: 1.0,
        };
        let field = search_field_rect(&bounds, 96, 1.0).expect("field at full width");
        let (fx, fy) = mid(&field);
        assert_eq!(
            target_at(fx, fy, bounds, 96, &nav, &sessions),
            Some(NavTarget::SearchField)
        );
        assert!(action_for(NavTarget::SearchField, &tree).is_none());
    }

    /// Half-open, the box is narrower and gives the space back as it goes.
    #[test]
    fn the_filter_box_slides_from_the_magnifier() {
        let bounds = tile();
        let open = search_field_rect(&bounds, 96, 1.0).expect("open");
        let half = search_field_rect(&bounds, 96, 0.5).expect("half");
        assert_eq!(open.right, half.right);
        assert!(half.left > open.left);
        assert!(open.right <= search_icon_rect(&bounds, 96).left);
        assert!(search_field_rect(&bounds, 96, 0.0).is_none());
    }

    #[test]
    fn scrolling_shifts_hit_testing_by_the_offset() {
        let bounds = tile();
        let tree = vec![node("a", 0, 1)];
        let expanded: HashSet<PathBuf> = tree.iter().map(|n| n.path.clone()).collect();
        let nav = nav_state(&tree, &[], &expanded, PROJECT_ROW_H);
        let sessions = Sessions::new();
        // With the project row scrolled off the top, its former y now
        // belongs to the history row beneath it.
        assert_eq!(
            target_at(40, first_row_y(), bounds, 96, &nav, &sessions),
            Some(NavTarget::History(0, 0))
        );
    }

    #[test]
    fn clamp_scroll_pins_to_zero_when_content_fits() {
        let bounds = tile();
        let tree = vec![node("a", 0, 1)];
        let expanded = HashSet::new();
        let nav = nav_state(&tree, &[], &expanded, 0);
        let sessions = Sessions::new();
        assert_eq!(clamp_scroll(bounds, 96, &nav, &sessions, 500), 0);
    }

    #[test]
    fn clamp_scroll_stops_at_the_last_row() {
        let bounds = RECT {
            bottom: 60,
            ..tile()
        };
        let tree = vec![node("a", 0, 0), node("b", 0, 0), node("c", 0, 0)];
        let expanded = HashSet::new();
        let nav = nav_state(&tree, &[], &expanded, 0);
        let sessions = Sessions::new();
        // The header is pinned, so only what's left of the tile scrolls.
        let expected = content_height(96, &nav, &sessions) - (60 - HEADER_H);
        assert_eq!(clamp_scroll(bounds, 96, &nav, &sessions, 9_999), expected);
    }

    #[test]
    fn actions_carry_the_project_path_and_session_id() {
        let tree = vec![node("a", 0, 1)];
        assert!(matches!(
            action_for(NavTarget::NewSession(0), &tree),
            Some(TileAction::NewSessionIn(p)) if p == tree[0].path
        ));
        assert!(matches!(
            action_for(NavTarget::History(0, 0), &tree),
            Some(TileAction::ResumeSession { cwd, session_id })
                if cwd == tree[0].path && session_id == "id-0"
        ));
    }

    #[test]
    fn stale_indices_produce_no_action() {
        let tree = vec![node("a", 0, 0)];
        assert!(action_for(NavTarget::NewSession(7), &tree).is_none());
        assert!(action_for(NavTarget::History(0, 3), &tree).is_none());
    }
}
