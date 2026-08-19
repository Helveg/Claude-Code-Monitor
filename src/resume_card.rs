//! The card a restored session shows instead of a terminal.
//!
//! ```text
//! ┌──────────────────────────────────┐
//! │  add a nav tile to the panel     │
//! │  Claude-Code-Monitor             │
//! │  2h ago · 78.2k / 200k context   │
//! │  ▓▓▓▓▓▓▓▓░░░░░░░░░░░░░░░░░░░░░   │
//! │        Resume      Close         │
//! └──────────────────────────────────┘
//! ```
//!
//! The manager remembers which sessions were open and puts them back on
//! restart, but does not run them again — a restored session is a
//! placeholder until the user says otherwise. This card is that
//! placeholder: enough of the conversation to recognize it, and the two
//! answers to "and now?".
//!
//! Facts come from claude's own transcript via [`crate::claude_store`], so
//! they stay accurate for a conversation that has since moved on. Anything
//! the store hasn't seen yet is simply left out rather than guessed at.
//!
//! [`paint`] and [`button_at`] derive their geometry from the same
//! [`card_rect`] / [`button_rects`] pair, so what is drawn is what is
//! clickable at every size.

use std::time::SystemTime;

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;

use crate::claude_store;
use crate::native_interop::{self, Color};
use crate::sessions::Session;

/// Design pixels at 96 DPI.
const CARD_W: i32 = 300;
const CARD_PAD: i32 = 16;
const TITLE_H: i32 = 20;
const LINE_H: i32 = 16;
const BAR_H: i32 = 4;
const BAR_GAP: i32 = 10;
const BUTTON_W: i32 = 78;
const BUTTON_H: i32 = 26;
const BUTTON_GAP: i32 = 12;
/// Smallest slot the card still draws in. Below this only the buttons fit,
/// and below *that* nothing is drawn at all.
const MIN_CARD_W: i32 = 120;
const MIN_CARD_H: i32 = 52;

const TITLE_FONT_PT: i32 = 11;
const META_FONT_PT: i32 = 8;
const BUTTON_FONT_PT: i32 = 9;

const CARD_BG_HEX: &str = "#1F1F1D";
const CARD_BORDER_HEX: &str = "#3A3A38";
const TITLE_FG_HEX: &str = "#E8E8E8";
const META_FG_HEX: &str = "#8B857E";
const BAR_TRACK_HEX: &str = "#333330";
const ORANGE_HEX: &str = "#D97757";
const BUTTON_FG_HEX: &str = "#C8C4BE";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardButton {
    /// Start claude again on this conversation, in place.
    Resume,
    /// Forget the session — it leaves the workspace and won't come back on
    /// the next restart. The transcript is untouched.
    Close,
}

/// Rect the card occupies inside a session's slot: centred, capped at
/// [`CARD_W`], and shrinking with the slot. `None` when the slot is too
/// small to say anything useful in.
fn card_rect(bounds: &RECT, dpi: u32) -> Option<RECT> {
    let scale = dpi as f64 / 96.0;
    let pad = (CARD_PAD as f64 * scale).round() as i32;
    let avail_w = bounds.right - bounds.left - pad;
    let avail_h = bounds.bottom - bounds.top - pad;
    if avail_w < (MIN_CARD_W as f64 * scale) as i32 || avail_h < (MIN_CARD_H as f64 * scale) as i32
    {
        return None;
    }
    let w = avail_w.min((CARD_W as f64 * scale).round() as i32);
    let h = avail_h.min(card_height(dpi));
    let cx = (bounds.left + bounds.right) / 2;
    let cy = (bounds.top + bounds.bottom) / 2;
    Some(RECT {
        left: cx - w / 2,
        top: cy - h / 2,
        right: cx + w / 2,
        bottom: cy + h / 2,
    })
}

/// Height the card wants when the slot allows it: everything stacked, plus
/// padding above and below.
fn card_height(dpi: u32) -> i32 {
    let scale = dpi as f64 / 96.0;
    let content = TITLE_H + LINE_H + LINE_H + BAR_GAP + BAR_H + BAR_GAP + BUTTON_H;
    ((content + 2 * CARD_PAD) as f64 * scale).round() as i32
}

/// `(Resume, Close)`, side by side along the card's bottom edge. `None`
/// when there is no card, or it is too narrow for two buttons.
fn button_rects(bounds: &RECT, dpi: u32) -> Option<(RECT, RECT)> {
    let scale = dpi as f64 / 96.0;
    let card = card_rect(bounds, dpi)?;
    let pad = (CARD_PAD as f64 * scale).round() as i32;
    let gap = (BUTTON_GAP as f64 * scale).round() as i32;
    let h = (BUTTON_H as f64 * scale).round() as i32;
    // Buttons share what the card can spare, never wider than their design
    // width — two comfortable targets beat two enormous ones.
    let usable = card.right - card.left - pad;
    let w = ((usable - gap) / 2).min((BUTTON_W as f64 * scale).round() as i32);
    if w <= 0 || card.bottom - card.top < h + pad {
        return None;
    }
    let cx = (card.left + card.right) / 2;
    let bottom = card.bottom - pad / 2;
    let top = bottom - h;
    Some((
        RECT {
            left: cx - gap / 2 - w,
            top,
            right: cx - gap / 2,
            bottom,
        },
        RECT {
            left: cx + gap / 2,
            top,
            right: cx + gap / 2 + w,
            bottom,
        },
    ))
}

/// Which button is under `(x, y)`, `bounds` being the session's slot.
pub fn button_at(x: i32, y: i32, bounds: RECT, dpi: u32) -> Option<CardButton> {
    let (resume, close) = button_rects(&bounds, dpi)?;
    if contains(&resume, x, y) {
        return Some(CardButton::Resume);
    }
    if contains(&close, x, y) {
        return Some(CardButton::Close);
    }
    None
}

fn contains(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// Draw the card over a dormant session's slot. `hovered` is the button
/// under the cursor, if this is the session the cursor is over.
pub fn paint(
    hdc: HDC,
    bounds: RECT,
    dpi: u32,
    session: &Session,
    hovered: Option<CardButton>,
) {
    let Some(card) = card_rect(&bounds, dpi) else {
        return;
    };
    let scale = dpi as f64 / 96.0;
    let pad = (CARD_PAD as f64 * scale).round() as i32;
    let radius = (8.0 * scale).round().max(2.0) as i32;

    unsafe {
        let bg = CreateSolidBrush(COLORREF(Color::from_hex(CARD_BG_HEX).to_colorref()));
        let rgn = CreateRoundRectRgn(
            card.left,
            card.top,
            card.right + 1,
            card.bottom + 1,
            radius * 2,
            radius * 2,
        );
        let _ = FillRgn(hdc, rgn, bg);
        let _ = DeleteObject(rgn);
        let _ = DeleteObject(bg);

        let pen = CreatePen(
            PS_SOLID,
            1,
            COLORREF(Color::from_hex(CARD_BORDER_HEX).to_colorref()),
        );
        let old_pen = SelectObject(hdc, pen);
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
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
        let _ = SetBkMode(hdc, TRANSPARENT);
    }

    let buttons = button_rects(&bounds, dpi);
    // Everything above the buttons is optional: a small cell keeps the two
    // answers and drops the prose.
    let text_bottom = buttons.map(|(r, _)| r.top).unwrap_or(card.bottom - pad / 2);
    let mut y = card.top + pad / 2;
    let left = card.left + pad / 2;
    let right = card.right - pad / 2;

    let record = claude_store::global().lookup_by_session_id(&session.session_id);

    let title_font = make_font(dpi, TITLE_FONT_PT, FW_MEDIUM.0 as i32);
    let meta_font = make_font(dpi, META_FONT_PT, FW_NORMAL.0 as i32);
    let button_font = make_font(dpi, BUTTON_FONT_PT, FW_MEDIUM.0 as i32);

    let title_h = (TITLE_H as f64 * scale).round() as i32;
    let line_h = (LINE_H as f64 * scale).round() as i32;

    if y + title_h <= text_bottom {
        let old = unsafe { SelectObject(hdc, title_font) };
        draw_text(
            hdc,
            &session.name,
            RECT {
                left,
                top: y,
                right,
                bottom: y + title_h,
            },
            Color::from_hex(TITLE_FG_HEX),
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
        unsafe { SelectObject(hdc, old) };
        y += title_h;
    }

    let old_meta = unsafe { SelectObject(hdc, meta_font) };
    let project = session
        .cwd
        .as_ref()
        .and_then(|c| c.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if !project.is_empty() && y + line_h <= text_bottom {
        draw_text(
            hdc,
            &project,
            RECT {
                left,
                top: y,
                right,
                bottom: y + line_h,
            },
            Color::from_hex(META_FG_HEX),
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
        y += line_h;
    }

    let meta = meta_line(record.as_ref());
    if !meta.is_empty() && y + line_h <= text_bottom {
        draw_text(
            hdc,
            &meta,
            RECT {
                left,
                top: y,
                right,
                bottom: y + line_h,
            },
            Color::from_hex(META_FG_HEX),
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
        y += line_h;
    }
    unsafe { SelectObject(hdc, old_meta) };

    // Context meter, when the transcript has told us how full the window is.
    let bar_gap = (BAR_GAP as f64 * scale).round() as i32;
    let bar_h = (BAR_H as f64 * scale).round().max(2.0) as i32;
    if let Some(fraction) = record.as_ref().and_then(context_fraction) {
        if y + bar_gap + bar_h <= text_bottom {
            let track = RECT {
                left,
                top: y + bar_gap,
                right,
                bottom: y + bar_gap + bar_h,
            };
            fill(hdc, &track, Color::from_hex(BAR_TRACK_HEX));
            let filled = ((track.right - track.left) as f64 * fraction).round() as i32;
            if filled > 0 {
                fill(
                    hdc,
                    &RECT {
                        right: track.left + filled,
                        ..track
                    },
                    Color::from_hex(ORANGE_HEX),
                );
            }
        }
    }

    if let Some((resume, close)) = buttons {
        let old = unsafe { SelectObject(hdc, button_font) };
        paint_button(
            hdc,
            &resume,
            "Resume",
            hovered == Some(CardButton::Resume),
            true,
            dpi,
        );
        paint_button(
            hdc,
            &close,
            "Close",
            hovered == Some(CardButton::Close),
            false,
            dpi,
        );
        unsafe { SelectObject(hdc, old) };
    }

    unsafe {
        let _ = DeleteObject(title_font);
        let _ = DeleteObject(meta_font);
        let _ = DeleteObject(button_font);
    }
}

/// "2h ago · 78.2k / 200k context" — whichever halves are known.
fn meta_line(record: Option<&claude_store::ClaudeSession>) -> String {
    let Some(record) = record else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    parts.push(relative_time(record.last_modified));
    if let Some(tokens) = record.context_tokens {
        parts.push(format!(
            "{} / {} context",
            compact_tokens(tokens),
            compact_tokens(record.context_limit())
        ));
    }
    parts.join(" \u{00b7} ")
}

/// How full the context window is, `0.0..=1.0`.
fn context_fraction(record: &claude_store::ClaudeSession) -> Option<f64> {
    let tokens = record.context_tokens?;
    let limit = record.context_limit().max(1);
    Some((tokens as f64 / limit as f64).clamp(0.0, 1.0))
}

/// `78_200` → `78.2k`, `1_000_000` → `1M`.
fn compact_tokens(tokens: u64) -> String {
    match tokens {
        0..=999 => tokens.to_string(),
        1_000..=999_999 => {
            let k = tokens as f64 / 1_000.0;
            if k < 10.0 {
                format!("{k:.1}k")
            } else {
                format!("{}k", k.round() as u64)
            }
        }
        _ => {
            let m = tokens as f64 / 1_000_000.0;
            if (m - m.round()).abs() < 0.05 {
                format!("{}M", m.round() as u64)
            } else {
                format!("{m:.1}M")
            }
        }
    }
}

/// Age of a timestamp in the coarsest unit that still says something.
fn relative_time(at: SystemTime) -> String {
    let Ok(elapsed) = SystemTime::now().duration_since(at) else {
        // Clock skew, or a file written a moment into the future.
        return "just now".to_string();
    };
    let secs = elapsed.as_secs();
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        86_400..=604_799 => format!("{}d ago", secs / 86_400),
        _ => format!("{}w ago", secs / 604_800),
    }
}

/// Outlined pill, filled with nothing — the panel's controls are strokes.
/// The primary one carries the accent colour so the pair reads as
/// "resume, or else".
fn paint_button(hdc: HDC, rect: &RECT, label: &str, hovered: bool, primary: bool, dpi: u32) {
    let color = if hovered || primary {
        Color::from_hex(ORANGE_HEX)
    } else {
        Color::from_hex(BUTTON_FG_HEX)
    };
    let unit = (dpi as f64 / 96.0).round().max(1.0) as i32;
    stroke_pill(hdc, rect, color, if hovered { unit * 2 } else { unit });
    draw_text_centered(hdc, label, rect, color);
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
/// `DT_CENTER | DT_VCENTER` — that centres the font's line box, whose
/// internal leading all sits above the glyphs, so the text comes out low.
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
        let glyphs_h = tm.tmAscent - tm.tmInternalLeading + tm.tmDescent;
        let x = (rect.left + rect.right - size.cx) / 2;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(w: i32, h: i32) -> RECT {
        RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        }
    }

    #[test]
    fn the_card_centres_itself_and_caps_its_width() {
        let bounds = slot(900, 600);
        let card = card_rect(&bounds, 96).expect("card");
        assert_eq!(card.right - card.left, CARD_W);
        assert_eq!((card.left + card.right) / 2, 450);
        assert_eq!((card.top + card.bottom) / 2, 300);
    }

    /// A grid cell at the minimum size still has to offer both answers.
    #[test]
    fn a_small_cell_keeps_its_buttons() {
        let bounds = slot(160, 120);
        let (resume, close) = button_rects(&bounds, 96).expect("buttons");
        assert!(resume.right <= close.left, "buttons overlap");
        let card = card_rect(&bounds, 96).expect("card");
        assert!(resume.left >= card.left && close.right <= card.right);
    }

    /// Whatever is drawn is what is clickable, at any DPI.
    #[test]
    fn hit_testing_follows_the_painted_buttons() {
        for dpi in [96, 144, 192] {
            let bounds = slot(600 * dpi as i32 / 96, 400 * dpi as i32 / 96);
            let (resume, close) = button_rects(&bounds, dpi).expect("buttons");
            let mid = |r: &RECT| ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
            let (rx, ry) = mid(&resume);
            assert_eq!(button_at(rx, ry, bounds, dpi), Some(CardButton::Resume));
            let (cx, cy) = mid(&close);
            assert_eq!(button_at(cx, cy, bounds, dpi), Some(CardButton::Close));
            // The gap between them belongs to neither.
            assert_eq!(button_at((resume.right + close.left) / 2, ry, bounds, dpi), None);
        }
    }

    #[test]
    fn a_slot_too_small_for_a_card_draws_nothing() {
        assert!(card_rect(&slot(80, 40), 96).is_none());
        assert!(button_at(20, 20, slot(80, 40), 96).is_none());
    }

    #[test]
    fn token_counts_read_at_a_glance() {
        assert_eq!(compact_tokens(842), "842");
        assert_eq!(compact_tokens(9_140), "9.1k");
        assert_eq!(compact_tokens(78_200), "78k");
        assert_eq!(compact_tokens(200_000), "200k");
        assert_eq!(compact_tokens(1_000_000), "1M");
    }

    #[test]
    fn relative_time_uses_the_coarsest_useful_unit() {
        let ago = |secs| relative_time(SystemTime::now() - std::time::Duration::from_secs(secs));
        assert_eq!(ago(5), "just now");
        assert_eq!(ago(120), "2m ago");
        assert_eq!(ago(7_200), "2h ago");
        assert_eq!(ago(172_800), "2d ago");
        assert_eq!(ago(1_209_600), "2w ago");
    }
}
