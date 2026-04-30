//! Bottom-area card grid. Each session renders as a card with a header
//! (name + status dot) and a live mini-terminal preview underneath. Cards
//! flow into rows; if more cards exist than fit, scrolling lands in a
//! later refinement.
//!
//! Click a card → focus that session in the main terminal.
//!
//! In the SidebarGrid view, the grid additionally surfaces "orphan" claude
//! projects: directories under `~/.claude/projects/` that don't have a live
//! terminal pointed at them. Orphan cards are read-only and show the most
//! recent message summary instead of a live preview.

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;

use crate::claude_store::{self, ClaudeSession};
use crate::dashboard::{CursorHint, TileAction};
use crate::diagnose;
use crate::native_interop::{self, Color};
use crate::sessions::{Session, SessionId, SessionStatus, Sessions};
use crate::terminal_view::{render_grid_region, RenderOptions};

/// Design pixels at 96 DPI.
/// Minimum / preferred card width — used as a target when computing the
/// number of columns that will fit. Cards stretch wider than this when the
/// remaining horizontal space allows, so the grid always fills the tile.
const CARD_W: i32 = 340;
const CARD_H: i32 = 180;
const CARD_GAP: i32 = 12;
const CARD_PADDING: i32 = 10;
/// Width of the scrollbar gutter on the right side of the cards grid.
const SCROLLBAR_W: i32 = 10;
/// Minimum scrollbar thumb height — keeps the thumb grabbable even when
/// content is much taller than the tile.
const SCROLLBAR_MIN_THUMB_H: i32 = 30;
/// Header height accommodating two text lines: name + status subtitle.
const HEADER_H: i32 = 40;
const SUBTITLE_H: i32 = 16;
const PREVIEW_FONT_PT: i32 = 9;
const STATUS_DOT_SIZE: i32 = 7;
const STATUS_DOT_GAP: i32 = 8;
const NAME_FONT_PT: i32 = 10;
const BORDER_RADIUS: i32 = 8;

const PANEL_BG_HEX: &str = "#262624";
const CARD_BG_HEX: &str = "#1F1F1D";
const CARD_BG_FOCUSED_HEX: &str = "#2C2C29";
const CARD_BG_ORPHAN_HEX: &str = "#1B1B19";
const CARD_BORDER_HEX: &str = "#3A3A38";
const NAME_FG_HEX: &str = "#E8E8E8";
const NAME_FG_ORPHAN_HEX: &str = "#A8A29E";
const META_FG_HEX: &str = "#7C766F";
const STATUS_IDLE_HEX: &str = "#5A5F58";
const STATUS_THINKING_HEX: &str = "#5BD16B";
const STATUS_NEEDS_HEX: &str = "#E07A5F";
const SCROLLBAR_TRACK_HEX: &str = "#1B1B19";
const SCROLLBAR_THUMB_HEX: &str = "#3A3A38";

const META_FONT_PT: i32 = 8;

/// Layout snapshot for one paint of the cards grid: column count + the
/// stretched per-card width / height / gap, plus the inner rect (the tile
/// minus the scrollbar gutter) and the y-pixel range covered by every card
/// at the chosen `card_count`. Computed once per paint and reused for
/// hit-testing.
#[derive(Clone, Copy)]
struct GridLayout {
    inner: RECT,
    card_w: i32,
    card_h: i32,
    gap: i32,
    cols: i32,
    #[allow(dead_code)]
    rows: i32,
    /// Total content height: rows * (card_h + gap) + gap. Used for scrollbar
    /// thumb sizing and scroll clamping.
    content_h: i32,
}

fn compute_grid_layout(bounds: &RECT, dpi: u32, card_count: usize) -> GridLayout {
    let scale = dpi as f64 / 96.0;
    let target_card_w = (CARD_W as f64 * scale).round() as i32;
    let card_h = (CARD_H as f64 * scale).round() as i32;
    let gap = (CARD_GAP as f64 * scale).round() as i32;
    let scrollbar_w = (SCROLLBAR_W as f64 * scale).round() as i32;
    // Reserve the scrollbar gutter unconditionally so the column math is
    // stable even when content fits — the gutter just stays empty in that
    // case.
    let inner = RECT {
        left: bounds.left,
        top: bounds.top,
        right: (bounds.right - scrollbar_w).max(bounds.left),
        bottom: bounds.bottom,
    };
    let total_w = (inner.right - inner.left).max(target_card_w + 2 * gap);
    // Number of columns sized using the *target* card width. Cards then
    // stretch to consume any leftover horizontal space.
    let cols = ((total_w - gap) / (target_card_w + gap)).max(1);
    let card_w = ((total_w - (cols + 1) * gap) / cols).max(target_card_w);
    let rows = if card_count == 0 {
        0
    } else {
        ((card_count as i32 + cols - 1) / cols).max(1)
    };
    let content_h = if rows == 0 {
        0
    } else {
        rows * (card_h + gap) + gap
    };
    GridLayout {
        inner,
        card_w,
        card_h,
        gap,
        cols,
        rows,
        content_h,
    }
}

/// Card rect at index `i` in the layout-virtual (pre-scroll) coordinate
/// system: the tile's `inner` rect, with cards laid out top-down. The
/// caller subtracts `scroll_y` from the `top`/`bottom` to get the actual
/// on-screen position.
fn card_rect(layout: &GridLayout, i: usize) -> RECT {
    let i = i as i32;
    let col = i % layout.cols;
    let row = i / layout.cols;
    let x = layout.inner.left + layout.gap + col * (layout.card_w + layout.gap);
    let y = layout.inner.top + layout.gap + row * (layout.card_h + layout.gap);
    RECT {
        left: x,
        top: y,
        right: x + layout.card_w,
        bottom: y + layout.card_h,
    }
}

/// Maximum legal scroll for the given layout — clamps `scroll_y` to
/// `[0, max_scroll]`. Returns 0 when content fits.
fn max_scroll(layout: &GridLayout) -> i32 {
    let visible_h = layout.inner.bottom - layout.inner.top;
    (layout.content_h - visible_h).max(0)
}

/// Compute the scrollbar's track + thumb rect inside `bounds`. Returns
/// `None` when content fits (no scrollbar needed).
fn scrollbar_rects(
    bounds: &RECT,
    layout: &GridLayout,
    dpi: u32,
    scroll_y: i32,
) -> Option<(RECT, RECT)> {
    let visible_h = layout.inner.bottom - layout.inner.top;
    if layout.content_h <= visible_h {
        return None;
    }
    let scale = dpi as f64 / 96.0;
    let scrollbar_w = (SCROLLBAR_W as f64 * scale).round() as i32;
    let track = RECT {
        left: bounds.right - scrollbar_w,
        top: bounds.top + layout.gap,
        right: bounds.right,
        bottom: bounds.bottom - layout.gap,
    };
    let track_h = (track.bottom - track.top).max(1);
    let min_thumb = (SCROLLBAR_MIN_THUMB_H as f64 * scale).round() as i32;
    let raw_thumb = (visible_h as i64 * track_h as i64 / layout.content_h as i64) as i32;
    let thumb_h = raw_thumb.max(min_thumb).min(track_h);
    let max_s = max_scroll(layout).max(1);
    let thumb_top = track.top + ((scroll_y as i64 * (track_h - thumb_h) as i64) / max_s as i64) as i32;
    let thumb = RECT {
        left: track.left,
        top: thumb_top,
        right: track.right,
        bottom: thumb_top + thumb_h,
    };
    Some((track, thumb))
}

/// One card in the grid. Live cards mirror a running session and may have
/// a matching claude-store record (used to surface a message-count badge);
/// orphan cards are read-only history entries — one per jsonl file under
/// `~/.claude/projects/`.
enum CardEntry<'a> {
    Live {
        session: &'a Session,
        history: Option<ClaudeSession>,
    },
    Orphan {
        record: ClaudeSession,
    },
}

/// Compute the ordered list of cards to render for the current view. Live
/// sessions come first (in session-list order); orphan jsonls come after
/// when `include_orphans` is set, sorted newest-first.
///
/// We deliberately do *not* exclude jsonls whose cwd matches a live PTY
/// session — we can't reliably correlate a live PTY to a specific jsonl,
/// and hiding a whole project's history just because the user opened a new
/// session in the same directory was the wrong default.
///
/// We *do* suppress orphans whose `session_id` matches a live session's
/// `resumed_from_session_id`: when the user clicks an orphan to resume it,
/// the jsonl behind that orphan is now backing the new live PTY, so
/// showing both as separate cards would just be a duplicate.
fn build_cards<'a>(sessions: &'a Sessions, include_orphans: bool) -> Vec<CardEntry<'a>> {
    let store = claude_store::global();
    let mut cards: Vec<CardEntry> = Vec::new();
    let resumed: std::collections::HashSet<String> = sessions
        .iter()
        .filter_map(|s| s.resumed_from_session_id.clone())
        .collect();
    for s in sessions.iter() {
        let history = s.cwd.as_deref().and_then(|c| store.latest_for_cwd(c));
        cards.push(CardEntry::Live { session: s, history });
    }
    if include_orphans {
        for record in store.snapshot() {
            if !record.session_id.is_empty() && resumed.contains(&record.session_id) {
                continue;
            }
            cards.push(CardEntry::Orphan { record });
        }
    }
    cards
}

pub fn paint(
    hdc: HDC,
    bounds: RECT,
    dpi: u32,
    sessions: &Sessions,
    focused: Option<SessionId>,
    include_orphans: bool,
    scroll_y: i32,
) {
    let panel_bg = Color::from_hex(PANEL_BG_HEX);
    unsafe {
        let bg = CreateSolidBrush(COLORREF(panel_bg.to_colorref()));
        FillRect(hdc, &bounds, bg);
        let _ = DeleteObject(bg);
        let _ = SetBkMode(hdc, TRANSPARENT);
    }

    let scale = dpi as f64 / 96.0;
    let radius = (BORDER_RADIUS as f64 * scale).round().max(2.0) as i32;

    let cards = build_cards(sessions, include_orphans);
    let layout = compute_grid_layout(&bounds, dpi, cards.len());
    let scroll_y = scroll_y.clamp(0, max_scroll(&layout));

    // Clip card rendering to the inner area so partially-visible rows at
    // the top/bottom of the viewport draw cleanly without overflowing into
    // adjacent tiles.
    let saved = unsafe { SaveDC(hdc) };
    unsafe {
        let _ = IntersectClipRect(
            hdc,
            layout.inner.left,
            layout.inner.top,
            layout.inner.right,
            layout.inner.bottom,
        );
    }

    let name_font = create_font(NAME_FONT_PT, dpi, FW_MEDIUM.0 as i32);
    let meta_font = create_font(META_FONT_PT, dpi, FW_NORMAL.0 as i32);

    for (i, entry) in cards.iter().enumerate() {
        let virt = card_rect(&layout, i);
        let card = RECT {
            left: virt.left,
            top: virt.top - scroll_y,
            right: virt.right,
            bottom: virt.bottom - scroll_y,
        };
        // Skip cards entirely above or below the visible band.
        if card.bottom < layout.inner.top {
            continue;
        }
        if card.top >= layout.inner.bottom {
            break;
        }
        paint_card(hdc, card, dpi, scale, radius, name_font, meta_font, focused, entry);
    }

    unsafe {
        let _ = DeleteObject(name_font);
        let _ = DeleteObject(meta_font);
        let _ = RestoreDC(hdc, saved);
    }

    paint_scrollbar(hdc, &bounds, &layout, dpi, scroll_y);
}

fn paint_scrollbar(hdc: HDC, bounds: &RECT, layout: &GridLayout, dpi: u32, scroll_y: i32) {
    let Some((track, thumb)) = scrollbar_rects(bounds, layout, dpi, scroll_y) else {
        return;
    };
    let track_color = Color::from_hex(SCROLLBAR_TRACK_HEX);
    let thumb_color = Color::from_hex(SCROLLBAR_THUMB_HEX);
    unsafe {
        let track_brush = CreateSolidBrush(COLORREF(track_color.to_colorref()));
        FillRect(hdc, &track, track_brush);
        let _ = DeleteObject(track_brush);
        let thumb_brush = CreateSolidBrush(COLORREF(thumb_color.to_colorref()));
        FillRect(hdc, &thumb, thumb_brush);
        let _ = DeleteObject(thumb_brush);
    }
}

fn create_font(point_size: i32, dpi: u32, weight: i32) -> HFONT {
    let height = -(point_size * dpi as i32 / 72);
    let face = native_interop::wide_str("Segoe UI");
    unsafe {
        CreateFontW(
            height,
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

fn paint_card(
    hdc: HDC,
    card: RECT,
    dpi: u32,
    scale: f64,
    radius: i32,
    name_font: HFONT,
    meta_font: HFONT,
    focused: Option<SessionId>,
    entry: &CardEntry,
) {
    let header_h = (HEADER_H as f64 * scale).round() as i32;
    let subtitle_h = (SUBTITLE_H as f64 * scale).round() as i32;
    let name_h = header_h - subtitle_h;
    let pad = (CARD_PADDING as f64 * scale).round() as i32;
    let dot_size = (STATUS_DOT_SIZE as f64 * scale).round() as i32;
    let dot_gap = (STATUS_DOT_GAP as f64 * scale).round() as i32;

    let now = std::time::SystemTime::now();
    let is_focused = matches!(entry, CardEntry::Live { session, .. } if focused == Some(session.id));
    // Faded styling is reserved for orphans whose jsonl hasn't been
    // touched in a long time. An "ongoing" jsonl session — one that's
    // still actively being written to — gets first-class card chrome.
    let is_faded = matches!(
        entry,
        CardEntry::Orphan { record } if record.activity_at(now) == claude_store::SessionActivity::Stale
    );

    let bg_color = if is_faded {
        Color::from_hex(CARD_BG_ORPHAN_HEX)
    } else if is_focused {
        Color::from_hex(CARD_BG_FOCUSED_HEX)
    } else {
        Color::from_hex(CARD_BG_HEX)
    };
    let card_border = Color::from_hex(CARD_BORDER_HEX);

    unsafe {
        let brush = CreateSolidBrush(COLORREF(bg_color.to_colorref()));
        let rgn = CreateRoundRectRgn(
            card.left,
            card.top,
            card.right + 1,
            card.bottom + 1,
            radius * 2,
            radius * 2,
        );
        let _ = FillRgn(hdc, rgn, brush);
        let _ = DeleteObject(rgn);
        let _ = DeleteObject(brush);
    }
    unsafe {
        let pen = CreatePen(PS_SOLID, 1, COLORREF(card_border.to_colorref()));
        let old_pen = SelectObject(hdc, pen);
        let null_brush = GetStockObject(NULL_BRUSH);
        let old_brush = SelectObject(hdc, null_brush);
        let _ = RoundRect(
            hdc,
            card.left,
            card.top,
            card.right,
            card.bottom,
            radius * 2,
            radius * 2,
        );
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(pen);
    }

    // Header strip: status dot + name on the top line, status label on
    // the bottom line as a subtitle. Live and orphan cards both carry a
    // dot — the live card's color comes from its computed `SessionStatus`,
    // the orphan's from the jsonl's recency on disk. Stale orphans omit
    // the dot entirely.
    let header_top = card.top + pad / 2;
    let name_top = header_top;
    let name_bottom = name_top + name_h;
    let subtitle_top = name_bottom;
    let mut label_x = card.left + pad;
    let dot_y = name_top + (name_h - dot_size) / 2;
    let dot_color: Option<Color> = match entry {
        CardEntry::Live { session, .. } => Some(match session.status {
            SessionStatus::Idle => Color::from_hex(STATUS_IDLE_HEX),
            SessionStatus::Thinking => Color::from_hex(STATUS_THINKING_HEX),
            SessionStatus::NeedsAttention => Color::from_hex(STATUS_NEEDS_HEX),
        }),
        CardEntry::Orphan { record } => match record.activity_at(now) {
            claude_store::SessionActivity::Thinking => {
                Some(Color::from_hex(STATUS_THINKING_HEX))
            }
            claude_store::SessionActivity::NeedsAttention => {
                Some(Color::from_hex(STATUS_NEEDS_HEX))
            }
            claude_store::SessionActivity::Idle => Some(Color::from_hex(STATUS_IDLE_HEX)),
            claude_store::SessionActivity::Stale => None,
        },
    };
    if let Some(color) = dot_color {
        unsafe {
            let brush = CreateSolidBrush(COLORREF(color.to_colorref()));
            let rgn = CreateRoundRectRgn(
                label_x,
                dot_y,
                label_x + dot_size + 1,
                dot_y + dot_size + 1,
                dot_size,
                dot_size,
            );
            let _ = FillRgn(hdc, rgn, brush);
            let _ = DeleteObject(rgn);
            let _ = DeleteObject(brush);
        }
        label_x += dot_size + dot_gap;
    }

    let orphan_title;
    let (name_text, subtitle_text, history) = match entry {
        CardEntry::Live { session, history } => {
            let label: &str = if session.status_label.is_empty() {
                "idle"
            } else {
                session.status_label.as_str()
            };
            (session.name.as_str(), label, history.as_ref())
        }
        CardEntry::Orphan { record } => {
            orphan_title = orphan_card_title(record);
            let label = match record.activity_at(now) {
                claude_store::SessionActivity::Thinking => "thinking",
                claude_store::SessionActivity::NeedsAttention => "assistant",
                claude_store::SessionActivity::Idle => match record.last_speaker {
                    claude_store::LastSpeaker::Assistant => "assistant",
                    claude_store::LastSpeaker::Human => "human",
                    claude_store::LastSpeaker::ToolResult => "tool",
                    claude_store::LastSpeaker::Unknown => "idle",
                },
                claude_store::SessionActivity::Stale => "stale",
            };
            (orphan_title.as_str(), label, Some(record))
        }
    };

    // Right-side badge: "N msgs" — for live cards, the message count from
    // the most recent saved session in the same cwd; for orphan cards, the
    // session's own count. Drawn on the name row only.
    let mut name_right = card.right - pad;
    if let Some(p) = history {
        let badge_text = format!("{} msgs", p.message_count);
        let mut badge_wide: Vec<u16> = badge_text.encode_utf16().collect();
        let old_font = unsafe { SelectObject(hdc, meta_font) };
        let mut size = SIZE::default();
        unsafe {
            let _ = GetTextExtentPoint32W(hdc, &badge_wide, &mut size);
            let _ = SetTextColor(hdc, COLORREF(Color::from_hex(META_FG_HEX).to_colorref()));
            let mut badge_rect = RECT {
                left: name_right - size.cx,
                top: name_top,
                right: name_right,
                bottom: name_bottom,
            };
            let _ = DrawTextW(
                hdc,
                &mut badge_wide,
                &mut badge_rect,
                DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(hdc, old_font);
        }
        name_right -= size.cx + dot_gap;
    }

    let old_font = unsafe { SelectObject(hdc, name_font) };
    let mut label_rect = RECT {
        left: label_x,
        top: name_top,
        right: name_right,
        bottom: name_bottom,
    };
    let mut label_wide: Vec<u16> = name_text.encode_utf16().collect();
    let name_color = if is_faded {
        Color::from_hex(NAME_FG_ORPHAN_HEX)
    } else {
        Color::from_hex(NAME_FG_HEX)
    };
    unsafe {
        let _ = SetTextColor(hdc, COLORREF(name_color.to_colorref()));
        let _ = DrawTextW(
            hdc,
            &mut label_wide,
            &mut label_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
        SelectObject(hdc, old_font);
    }

    // Subtitle row: status label, dimmer / smaller, indented under the name
    // (skipping the dot column to keep the alignment with the title).
    if !subtitle_text.is_empty() {
        let old_font = unsafe { SelectObject(hdc, meta_font) };
        let mut subtitle_rect = RECT {
            left: card.left + pad,
            top: subtitle_top,
            right: card.right - pad,
            bottom: subtitle_top + subtitle_h,
        };
        let mut subtitle_wide: Vec<u16> = subtitle_text.encode_utf16().collect();
        unsafe {
            let _ = SetTextColor(
                hdc,
                COLORREF(Color::from_hex(META_FG_HEX).to_colorref()),
            );
            let _ = DrawTextW(
                hdc,
                &mut subtitle_wide,
                &mut subtitle_rect,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
            SelectObject(hdc, old_font);
        }
    }

    // Body: live preview, or for orphans the last-message summary.
    let body = RECT {
        left: card.left + pad,
        top: card.top + pad + header_h,
        right: card.right - pad,
        bottom: card.bottom - pad,
    };
    if body.bottom <= body.top {
        return;
    }
    match entry {
        CardEntry::Live { session, .. } => {
            paint_live_preview(hdc, body, dpi, session);
        }
        CardEntry::Orphan { record } => {
            paint_orphan_body(hdc, body, record, meta_font);
        }
    }
}

/// Render the truncated last-message summary for an orphan session card.
/// Read-only — no terminal preview because there's no live PTY.
fn paint_orphan_body(hdc: HDC, bounds: RECT, record: &ClaudeSession, meta_font: HFONT) {
    if record.last_message_summary.is_empty() {
        return;
    }
    let old_font = unsafe { SelectObject(hdc, meta_font) };
    let mut wide: Vec<u16> = record.last_message_summary.encode_utf16().collect();
    let mut rect = bounds;
    unsafe {
        let _ = SetTextColor(
            hdc,
            COLORREF(Color::from_hex(META_FG_HEX).to_colorref()),
        );
        let _ = DrawTextW(
            hdc,
            &mut wide,
            &mut rect,
            DT_LEFT | DT_TOP | DT_WORDBREAK | DT_NOPREFIX | DT_END_ELLIPSIS | DT_EDITCONTROL,
        );
        SelectObject(hdc, old_font);
    }
}

/// Card title for an orphan session: project basename + short session id,
/// e.g. `Claude-Code-Monitor · 13d4ffd2`.
fn orphan_card_title(record: &ClaudeSession) -> String {
    let project = record
        .project_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let short_id: String = record.session_id.chars().take(8).collect();
    if project.is_empty() && short_id.is_empty() {
        "session".to_string()
    } else if short_id.is_empty() {
        project.to_string()
    } else if project.is_empty() {
        short_id
    } else {
        format!("{project} · {short_id}")
    }
}

/// Maximum rows between the two horizontal dividers of claude's input box.
/// In practice the input is 1–3 rows tall, plus the borders themselves.
const INPUT_CHROME_MAX_HEIGHT: u16 = 5;
/// Fallback skip used when no input-box divider pair is found —
/// approximates claude's typical input chrome height.
const CLAUDE_INPUT_CHROME_FALLBACK: u16 = 4;

/// True if the row is dominated by horizontal box-drawing characters,
/// i.e. probably an input-box divider line.
fn row_is_horizontal_divider(grid: &crate::terminal::Grid, row: u16) -> bool {
    if row >= grid.rows || grid.cols == 0 {
        return false;
    }
    let mut count: u16 = 0;
    for col in 0..grid.cols {
        let c = grid.cell(row, col).ch;
        // Cover the common Unicode line characters claude uses for borders.
        match c {
            '─' | '━' | '╌' | '╍' | '═' | '┄' | '┅' | '┈' | '┉' => count += 1,
            _ => {}
        }
    }
    count >= grid.cols / 2
}

/// Find the top edge of claude's input chrome — i.e., the row above which
/// the actual conversation lives. claude renders its input as a box bordered
/// by two horizontal dividers; we scan the whole grid for divider rows and
/// return the top divider of the bottom-most divider pair within
/// `INPUT_CHROME_MAX_HEIGHT` of each other. If only a single divider exists
/// (e.g. partially scrolled), use that. `None` means no chrome detected.
fn find_input_chrome_top(grid: &crate::terminal::Grid) -> Option<u16> {
    let dividers: Vec<u16> = (0..grid.rows)
        .filter(|&r| row_is_horizontal_divider(grid, r))
        .collect();
    for pair in dividers.windows(2).rev() {
        if pair[1] - pair[0] <= INPUT_CHROME_MAX_HEIGHT {
            return Some(pair[0]);
        }
    }
    dividers.last().copied()
}

fn paint_live_preview(
    hdc: HDC,
    bounds: RECT,
    dpi: u32,
    session: &crate::sessions::Session,
) {
    let host_hwnd = session.session_view.terminal().host_hwnd();
    let Some(grid_arc) = session.session_view.terminal().grid_arc() else {
        return;
    };
    let grid = match grid_arc.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };

    let (cw, ch) = measure_preview_cell(host_hwnd, dpi);
    if cw <= 0 || ch <= 0 {
        return;
    }
    let visible_rows = ((bounds.bottom - bounds.top) / ch).max(0) as u16;
    if visible_rows == 0 {
        return;
    }

    // Find the input chrome's top edge (topmost horizontal-divider row in
    // the bottom slice). The preview renders the rows above that line —
    // i.e., claude's actual conversation output, not its input box.
    // Falls back to a fixed-rows skip when no divider is detected (e.g.
    // a non-claude session in the same terminal).
    let end_row = find_input_chrome_top(&grid)
        .or_else(|| grid.rows.checked_sub(CLAUDE_INPUT_CHROME_FALLBACK))
        .unwrap_or(grid.rows);
    let row_start = end_row.saturating_sub(visible_rows);

    maybe_log_preview_diagnostics(session, &grid, end_row, row_start, visible_rows);

    let opts = RenderOptions {
        dpi,
        font_pt: PREVIEW_FONT_PT,
        rows: row_start..end_row,
        selection: None,
        show_cursor: false,
    };
    render_grid_region(hdc, bounds, &grid, host_hwnd, &opts);
}

/// Periodically dump the bottom of each session's grid to the diagnostic log
/// so we can see what claude is actually rendering in the small preview area
/// and tune the input-chrome detection heuristic. No-op unless `--diagnose`
/// is on. Throttled per-session to avoid flooding.
fn maybe_log_preview_diagnostics(
    session: &crate::sessions::Session,
    grid: &crate::terminal::Grid,
    end_row: u16,
    row_start: u16,
    visible_rows: u16,
) {
    if !diagnose::is_enabled() {
        return;
    }
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    static LAST_LOG_MS: OnceLock<Mutex<HashMap<SessionId, u64>>> = OnceLock::new();
    let map = LAST_LOG_MS.get_or_init(|| Mutex::new(HashMap::new()));
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    {
        let mut m = match map.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let last = m.get(&session.id).copied().unwrap_or(0);
        if now_ms.saturating_sub(last) < 3000 {
            return;
        }
        m.insert(session.id, now_ms);
    }

    diagnose::log(format!(
        "preview[{}] rows={} cols={} cursor=({},{}) divider_top={:?} end_row={} row_start={} visible={}",
        session.name,
        grid.rows,
        grid.cols,
        grid.cursor_row,
        grid.cursor_col,
        find_input_chrome_top(grid),
        end_row,
        row_start,
        visible_rows,
    ));

    let scan_start = grid.rows.saturating_sub(15);
    let cap = grid.cols.min(80);
    for row in scan_start..grid.rows {
        let mut line = String::with_capacity(cap as usize);
        for col in 0..cap {
            let c = grid.cell(row, col).ch;
            if c == '\0' {
                line.push(' ');
            } else if c.is_control() {
                line.push('?');
            } else {
                line.push(c);
            }
        }
        let trimmed = line.trim_end();
        let div = row_is_horizontal_divider(grid, row) as u8;
        diagnose::log(format!("  r{row:>2} div={div} |{trimmed}|"));
    }
}

unsafe fn measure_preview_cell_inner(host_hwnd: HWND, dpi: u32) -> (i32, i32) {
    let height = -(PREVIEW_FONT_PT * dpi as i32 / 72);
    let candidates = ["Cascadia Code", "Cascadia Mono", "Consolas", "Courier New"];
    let mut font = HFONT::default();
    for name in candidates {
        let face = native_interop::wide_str(name);
        font = CreateFontW(
            height,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            (FIXED_PITCH.0 | FF_MODERN.0) as u32,
            PCWSTR::from_raw(face.as_ptr()),
        );
        if !font.is_invalid() {
            break;
        }
    }
    if font.is_invalid() {
        return (0, 0);
    }
    let dc = GetDC(host_hwnd);
    let old = SelectObject(dc, font);
    let mut tm = TEXTMETRICW::default();
    let _ = GetTextMetricsW(dc, &mut tm);
    let mut size = SIZE::default();
    let m: [u16; 1] = [b'M' as u16];
    let _ = GetTextExtentPoint32W(dc, &m, &mut size);
    SelectObject(dc, old);
    ReleaseDC(host_hwnd, dc);
    let _ = DeleteObject(font);
    let cw = if size.cx > 0 {
        size.cx
    } else {
        tm.tmAveCharWidth.max(1)
    };
    let ch = (tm.tmHeight + tm.tmExternalLeading).max(1);
    (cw, ch)
}

fn measure_preview_cell(host_hwnd: HWND, dpi: u32) -> (i32, i32) {
    unsafe { measure_preview_cell_inner(host_hwnd, dpi) }
}

/// Hand cursor over any clickable card (live, or an orphan with a known
/// `session_id` that we can `claude --resume` against), or over the
/// scrollbar thumb. Arrow elsewhere.
pub fn cursor_at(
    x: i32,
    y: i32,
    bounds: RECT,
    dpi: u32,
    sessions: &Sessions,
    include_orphans: bool,
    scroll_y: i32,
) -> CursorHint {
    let cards = build_cards(sessions, include_orphans);
    let layout = compute_grid_layout(&bounds, dpi, cards.len());
    let scroll_y = scroll_y.clamp(0, max_scroll(&layout));
    if let Some((_, thumb)) = scrollbar_rects(&bounds, &layout, dpi, scroll_y) {
        if point_in(&thumb, x, y) {
            return CursorHint::Hand;
        }
    }
    if !point_in(&layout.inner, x, y) {
        return CursorHint::Arrow;
    }
    for (i, entry) in cards.iter().enumerate() {
        let virt = card_rect(&layout, i);
        let card = RECT {
            left: virt.left,
            top: virt.top - scroll_y,
            right: virt.right,
            bottom: virt.bottom - scroll_y,
        };
        if card.bottom < layout.inner.top {
            continue;
        }
        if card.top >= layout.inner.bottom {
            break;
        }
        if point_in(&card, x, y) {
            return match entry {
                CardEntry::Live { .. } => CursorHint::Hand,
                CardEntry::Orphan { record } if !record.session_id.is_empty() => {
                    CursorHint::Hand
                }
                CardEntry::Orphan { .. } => CursorHint::Arrow,
            };
        }
    }
    CursorHint::Arrow
}

/// Outcome of clicking inside the cards tile. The panel layer interprets
/// this — card clicks become tile actions, scrollbar interactions update
/// the panel's `cards_scroll_y` directly.
pub enum CardsClick {
    Card(TileAction),
    /// User pressed on the scrollbar thumb — start a drag at this offset
    /// (mouse y minus thumb top) within the given tile bounds.
    ScrollThumbGrab {
        grab_offset: i32,
        bounds: RECT,
        content_h: i32,
    },
    /// User clicked the scrollbar track outside the thumb — page-jump
    /// `scroll_y` toward the click and consume.
    ScrollPageJump { delta: i32 },
    None,
}

pub fn handle_lbutton_down(
    x: i32,
    y: i32,
    bounds: RECT,
    dpi: u32,
    sessions: &Sessions,
    include_orphans: bool,
    scroll_y: i32,
) -> Option<TileAction> {
    match handle_lbutton_down_ex(x, y, bounds, dpi, sessions, include_orphans, scroll_y) {
        CardsClick::Card(action) => Some(action),
        _ => None,
    }
}

/// Extended click handler used by the panel directly (so it can react to
/// scrollbar interactions in addition to card actions).
pub fn handle_lbutton_down_ex(
    x: i32,
    y: i32,
    bounds: RECT,
    dpi: u32,
    sessions: &Sessions,
    include_orphans: bool,
    scroll_y: i32,
) -> CardsClick {
    let cards = build_cards(sessions, include_orphans);
    let layout = compute_grid_layout(&bounds, dpi, cards.len());
    let scroll_y = scroll_y.clamp(0, max_scroll(&layout));
    if let Some((track, thumb)) = scrollbar_rects(&bounds, &layout, dpi, scroll_y) {
        if point_in(&thumb, x, y) {
            return CardsClick::ScrollThumbGrab {
                grab_offset: y - thumb.top,
                bounds,
                content_h: layout.content_h,
            };
        }
        if point_in(&track, x, y) {
            // Page-jump in the direction of the click.
            let visible_h = layout.inner.bottom - layout.inner.top;
            let delta = if y < thumb.top { -visible_h } else { visible_h };
            return CardsClick::ScrollPageJump { delta };
        }
    }
    if !point_in(&layout.inner, x, y) {
        return CardsClick::None;
    }
    for (i, entry) in cards.iter().enumerate() {
        let virt = card_rect(&layout, i);
        let card = RECT {
            left: virt.left,
            top: virt.top - scroll_y,
            right: virt.right,
            bottom: virt.bottom - scroll_y,
        };
        if card.bottom < layout.inner.top {
            continue;
        }
        if card.top >= layout.inner.bottom {
            break;
        }
        if !point_in(&card, x, y) {
            continue;
        }
        return match entry {
            CardEntry::Live { session, .. } => {
                CardsClick::Card(TileAction::FocusSession(session.id))
            }
            CardEntry::Orphan { record } if !record.session_id.is_empty() => {
                CardsClick::Card(TileAction::ResumeSession {
                    session_id: record.session_id.clone(),
                    cwd: record.project_path.clone(),
                    name: orphan_card_title(record),
                })
            }
            CardEntry::Orphan { .. } => CardsClick::None,
        };
    }
    CardsClick::None
}

fn point_in(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// Convert a mouse-y position to the corresponding `scroll_y` while a
/// scrollbar drag is in progress. `grab_offset` is the y-distance between
/// the thumb's top and the mouse when the drag started.
pub fn scroll_y_from_drag(
    bounds: RECT,
    dpi: u32,
    content_h: i32,
    mouse_y: i32,
    grab_offset: i32,
) -> i32 {
    let scale = dpi as f64 / 96.0;
    let scrollbar_w = (SCROLLBAR_W as f64 * scale).round() as i32;
    let gap = (CARD_GAP as f64 * scale).round() as i32;
    let _ = scrollbar_w;
    let track_top = bounds.top + gap;
    let track_bottom = bounds.bottom - gap;
    let track_h = (track_bottom - track_top).max(1);
    let visible_h = bounds.bottom - bounds.top;
    if content_h <= visible_h {
        return 0;
    }
    let min_thumb = (SCROLLBAR_MIN_THUMB_H as f64 * scale).round() as i32;
    let raw_thumb = (visible_h as i64 * track_h as i64 / content_h as i64) as i32;
    let thumb_h = raw_thumb.max(min_thumb).min(track_h);
    let max_s = (content_h - visible_h).max(1);
    let thumb_top = (mouse_y - grab_offset).clamp(track_top, track_bottom - thumb_h);
    ((thumb_top - track_top) as i64 * max_s as i64 / (track_h - thumb_h).max(1) as i64) as i32
}

/// Clamp a scroll value against the cards grid layout for the current
/// session list + tile bounds. Lets the panel apply `delta` from
/// mousewheel events without re-deriving the layout itself.
pub fn clamp_scroll(
    bounds: RECT,
    dpi: u32,
    sessions: &Sessions,
    include_orphans: bool,
    scroll_y: i32,
) -> i32 {
    let cards = build_cards(sessions, include_orphans);
    let layout = compute_grid_layout(&bounds, dpi, cards.len());
    scroll_y.clamp(0, max_scroll(&layout))
}
