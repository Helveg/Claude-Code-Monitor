//! A session view: a `TerminalView` wrapped in a border with a floating
//! name label inset into the top-left of the border. Multiple sessions can
//! sit side by side in a host window — each maintains its own bounds and
//! routes input to its inner terminal.

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;

use crate::native_interop::{self, Color};
use crate::terminal_view::TerminalView;

const BORDER_COLOR_HEX: &str = "#3A3A38";
const BORDER_FOCUSED_HEX: &str = "#D97757";
const PANEL_BG_HEX: &str = "#262624";
const LABEL_FG_HEX: &str = "#A8A29E";
const PROJECT_FG_HEX: &str = "#7C766F";
/// Title width that has to survive before the project earns a place on the
/// frame. In design pixels — a title cut to two characters tells you less
/// than the project it belongs to, so below this the project is dropped.
const MIN_TITLE_W: i32 = 56;
/// Design pixels.
const BORDER_THICKNESS: i32 = 1;
const BORDER_RADIUS: i32 = 6;
const LABEL_INSET_X: i32 = 14;
const LABEL_PAD_X: i32 = 6;
const LABEL_FONT_POINT_SIZE: i32 = 9;
const TERMINAL_INSET: i32 = 1;
const STATUS_DOT_SIZE: i32 = 6;
const STATUS_DOT_GAP: i32 = 5;
/// Close cross inset into the top-right of the frame, mirroring the name
/// label at the other corner.
const CLOSE_SIZE: i32 = 7;
const CLOSE_INSET_X: i32 = 14;
/// The cross is a small target; the clickable area around it is not.
const CLOSE_HIT_INFLATE: i32 = 6;
const CLOSE_FG_HEX: &str = "#8B857E";
const CLOSE_HOVER_HEX: &str = "#D97757";

/// How a session's chrome should be drawn for the slot it currently sits in.
/// The grid view hosts many sessions at once, so it marks the one keyboard
/// input goes to and dots each name with the session's status; the dashboard's
/// single terminal needs neither.
#[derive(Clone, Copy, Default)]
pub struct ChromeStyle {
    /// Draw the border in the accent colour — "keystrokes land here".
    pub focused: bool,
    /// Status dot drawn just before the name label.
    pub status: Option<Color>,
    /// Attention pulse, `0.0..=1.0`, driven by the panel's animation timer.
    /// The border blends that far towards white, which reads across a grid
    /// of terminals from the other side of the room.
    pub pulse: Option<f64>,
    /// Draw the close cross on the frame. The grid gives every cell one;
    /// the dashboard's single slot has no use for it.
    pub closable: bool,
    pub close_hovered: bool,
}

const PULSE_COLOR_HEX: &str = "#FFFFFF";

/// Linear blend, `t` clamped to `0.0..=1.0`.
fn blend(from: Color, to: Color, t: f64) -> Color {
    let t = t.clamp(0.0, 1.0);
    let mix = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * t).round() as u8;
    Color::new(mix(from.r, to.r), mix(from.g, to.g), mix(from.b, to.b))
}

pub struct SessionView {
    bounds: RECT,
    name: String,
    /// Directory name of the session's cwd, drawn at the far end of the
    /// frame. A restored session is labelled with its conversation title —
    /// a sentence that says nothing about where it lives — so the project
    /// is what makes the frame readable at a glance.
    project: String,
    terminal: TerminalView,
}

unsafe impl Send for SessionView {}
unsafe impl Sync for SessionView {}

/// Last component of the session's working directory, which is what the
/// rest of the UI calls the project.
fn project_label(cwd: Option<&std::path::Path>) -> String {
    cwd.and_then(|c| c.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Split the frame's top edge between the title (from `text_x`, rightwards)
/// and the project (right-aligned at `content_right`).
///
/// Returns `(title_right, project_left)`; a `None` project means it was
/// dropped. The title is what tells two cells apart, so it holds the space:
/// the project only appears when at least `min_title` pixels of title
/// survive, and it is never given more than a third of the run — a project
/// cut short is still recognizable, where a title cut short isn't.
fn label_runs(
    text_x: i32,
    content_right: i32,
    project_w: i32,
    gap: i32,
    min_title: i32,
) -> (i32, Option<i32>) {
    let run = content_right - text_x;
    if project_w <= 0 || run <= 0 {
        return (content_right, None);
    }
    let project_w = project_w.min(run / 3);
    let project_left = content_right - project_w;
    let title_right = project_left - gap;
    if title_right - text_x < min_title {
        return (content_right, None);
    }
    (title_right, Some(project_left))
}

impl SessionView {
    pub fn new(
        host_hwnd: HWND,
        notify_msg: u32,
        name: impl Into<String>,
        cmd: impl Into<String>,
        cwd: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            bounds: RECT::default(),
            name: name.into(),
            project: project_label(cwd.as_deref()),
            terminal: TerminalView::new(host_hwnd, notify_msg, cmd, cwd),
        }
    }

    /// A session that is laid out but not running: restored from the last
    /// time the manager was open, waiting for the user to resume it. `cmd`
    /// is what it will run when they do.
    pub fn new_dormant(
        host_hwnd: HWND,
        notify_msg: u32,
        name: impl Into<String>,
        cmd: impl Into<String>,
        cwd: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            bounds: RECT::default(),
            name: name.into(),
            project: project_label(cwd.as_deref()),
            terminal: TerminalView::new_held(host_hwnd, notify_msg, cmd, cwd),
        }
    }

    pub fn is_dormant(&self) -> bool {
        self.terminal.is_held()
    }

    /// Re-label the frame. Returns `true` when the text actually changed, so
    /// the caller knows whether a repaint is owed.
    pub fn set_label(&mut self, name: String) -> bool {
        if self.name == name {
            return false;
        }
        self.name = name;
        true
    }

    /// Wake a dormant session. The PTY starts on the relayout that follows.
    pub fn wake(&mut self) {
        self.terminal.release_spawn();
    }

    pub fn set_bounds(&mut self, bounds: RECT, dpi: u32, font_pt: i32) {
        self.bounds = bounds;
        let inner = self.inner_terminal_rect(dpi);
        self.terminal.set_bounds(inner, dpi, font_pt);
    }

    #[allow(dead_code)]
    pub fn bounds(&self) -> RECT {
        self.bounds
    }

    pub fn terminal(&self) -> &TerminalView {
        &self.terminal
    }

    /// Visual rect of the frame's close cross. Both extents are an even
    /// number of pixels so the two diagonals cross exactly on a pixel
    /// boundary — with an odd extent the crossing lands half a pixel off
    /// and the four arms come out visibly unequal.
    fn close_rect(&self, dpi: u32) -> RECT {
        let scale = dpi as f64 / 96.0;
        let half = ((CLOSE_SIZE as f64 * scale) / 2.0).round().max(2.0) as i32;
        let inset = (CLOSE_INSET_X as f64 * scale).round() as i32;
        let right = self.bounds.right - inset;
        let top = self.bounds.top - half;
        RECT {
            left: right - half * 2,
            top,
            right,
            bottom: top + half * 2,
        }
    }

    /// True if `(x, y)` lands on the frame's close cross — a generous
    /// margin around the glyph, since it's only a few pixels of stroke.
    pub fn hits_close(&self, x: i32, y: i32, dpi: u32) -> bool {
        let by = (CLOSE_HIT_INFLATE as f64 * dpi as f64 / 96.0).round() as i32;
        let r = self.close_rect(dpi);
        x >= r.left - by && x < r.right + by && y >= r.top - by && y < r.bottom + by
    }

    /// Rect inside the border where the terminal lives.
    fn inner_terminal_rect(&self, dpi: u32) -> RECT {
        let scale = dpi as f64 / 96.0;
        let inset = ((BORDER_THICKNESS + TERMINAL_INSET) as f64 * scale).round() as i32;
        RECT {
            left: self.bounds.left + inset,
            top: self.bounds.top + inset,
            right: self.bounds.right - inset,
            bottom: self.bounds.bottom - inset,
        }
    }

    pub fn paint(&self, hdc: HDC, dpi: u32, style: ChromeStyle) {
        // Paint the inner terminal first; the border draws on top so it
        // cleanly clips against whatever the terminal renders at the edges.
        self.terminal.paint(hdc, dpi);
        unsafe { self.paint_chrome(hdc, dpi, style) };
    }

    unsafe fn paint_chrome(&self, hdc: HDC, dpi: u32, style: ChromeStyle) {
        let scale = dpi as f64 / 96.0;
        let radius = (BORDER_RADIUS as f64 * scale).round().max(1.0) as i32;
        // A pulsing border also runs thicker: at 1 px the colour swing alone
        // is easy to miss in a grid full of terminals.
        let thickness_design = BORDER_THICKNESS * if style.pulse.is_some() { 2 } else { 1 };
        let thickness = (thickness_design as f64 * scale).round().max(1.0) as i32;
        let label_inset = (LABEL_INSET_X as f64 * scale).round() as i32;
        let label_pad = (LABEL_PAD_X as f64 * scale).round() as i32;
        let dot_size = (STATUS_DOT_SIZE as f64 * scale).round().max(2.0) as i32;
        let dot_gap = (STATUS_DOT_GAP as f64 * scale).round() as i32;

        let base_border = if style.focused {
            Color::from_hex(BORDER_FOCUSED_HEX)
        } else {
            Color::from_hex(BORDER_COLOR_HEX)
        };
        let border_color = match style.pulse {
            Some(level) => blend(base_border, Color::from_hex(PULSE_COLOR_HEX), level),
            None => base_border,
        };
        let panel_bg = Color::from_hex(PANEL_BG_HEX);
        let label_fg = Color::from_hex(LABEL_FG_HEX);

        // Border: rounded rect outline drawn with a pen.
        let pen = CreatePen(PS_SOLID, thickness, COLORREF(border_color.to_colorref()));
        let old_pen = SelectObject(hdc, pen);
        let null_brush = GetStockObject(NULL_BRUSH);
        let old_brush = SelectObject(hdc, null_brush);
        let _ = RoundRect(
            hdc,
            self.bounds.left,
            self.bounds.top,
            self.bounds.right,
            self.bounds.bottom,
            radius * 2,
            radius * 2,
        );
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(pen);

        // Close cross, floating in the border at top-right the way the name
        // label floats at top-left. Same trick: punch the border out behind
        // it so the line breaks around the glyph.
        if style.closable {
            let close = self.close_rect(dpi);
            let bg_brush = CreateSolidBrush(COLORREF(panel_bg.to_colorref()));
            FillRect(
                hdc,
                &RECT {
                    left: close.left - label_pad,
                    top: self.bounds.top - thickness,
                    right: close.right + label_pad,
                    bottom: self.bounds.top + thickness,
                },
                bg_brush,
            );
            let _ = DeleteObject(bg_brush);

            let color = if style.close_hovered {
                Color::from_hex(CLOSE_HOVER_HEX)
            } else {
                Color::from_hex(CLOSE_FG_HEX)
            };
            let stroke = (dpi as f64 / 96.0).round().max(1.0) as i32;
            let close_pen = CreatePen(PS_SOLID, stroke, COLORREF(color.to_colorref()));
            let old = SelectObject(hdc, close_pen);
            // LineTo leaves its final point unpainted, so each diagonal is
            // drawn one step past the corner it ends on. Both then cover
            // the same pixel box and the two strokes come out the same
            // length, crossing in the middle.
            let _ = MoveToEx(hdc, close.left, close.top, None);
            let _ = LineTo(hdc, close.right, close.bottom);
            let _ = MoveToEx(hdc, close.right - 1, close.top, None);
            let _ = LineTo(hdc, close.left - 1, close.bottom);
            SelectObject(hdc, old);
            let _ = DeleteObject(close_pen);
        }

        // Label: small text floating in the border at top-left. Punch a panel-
        // bg rect behind it so the border line breaks for the label.
        if self.name.is_empty() {
            return;
        }
        let label_height = -(LABEL_FONT_POINT_SIZE * dpi as i32 / 72);
        let face = native_interop::wide_str("Segoe UI");
        let label_font = CreateFontW(
            label_height,
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
        );
        let old_label_font = SelectObject(hdc, label_font);

        let label_wide: Vec<u16> = self.name.encode_utf16().collect();
        let mut text_size = SIZE::default();
        let _ = GetTextExtentPoint32W(hdc, &label_wide, &mut text_size);

        let mut tm = TEXTMETRICW::default();
        let _ = GetTextMetricsW(hdc, &mut tm);

        let label_x = self.bounds.left + label_inset;
        // The dot, when present, takes the head of the label run and pushes
        // the text right by its own width.
        let dot_run = style.status.map_or(0, |_| dot_size + dot_gap);
        let text_x = label_x + dot_run;
        // Center the label vertically on the top border edge.
        let label_y = self.bounds.top - (tm.tmAscent + tm.tmDescent) / 2;

        // The label run ends at the close cross, or at the frame's other
        // corner when there isn't one. A session's name is its conversation
        // title, which is a whole sentence — without a limit here it runs
        // straight off the frame and across whatever sits beside it.
        let content_right = if style.closable {
            self.close_rect(dpi).left - label_pad
        } else {
            self.bounds.right - label_inset
        };
        if content_right <= text_x {
            SelectObject(hdc, old_label_font);
            let _ = DeleteObject(label_font);
            return;
        }

        // The project shares the top edge, right-aligned — but only when it
        // says something the title doesn't. A session started from the nav
        // is named after its directory already.
        let project = Some(self.project.as_str())
            .filter(|p| !p.is_empty() && *p != self.name.as_str());
        let project_wide: Vec<u16> = project
            .map(|p| p.encode_utf16().collect())
            .unwrap_or_default();
        let mut project_size = SIZE::default();
        if !project_wide.is_empty() {
            let _ = GetTextExtentPoint32W(hdc, &project_wide, &mut project_size);
        }
        let (text_right, project_left) = label_runs(
            text_x,
            content_right,
            project_size.cx,
            label_pad * 2,
            (MIN_TITLE_W as f64 * scale).round() as i32,
        );
        let drawn_w = text_size.cx.min(text_right - text_x);

        // Erase the slice of border that lies behind the label so the line
        // visually breaks around it — as far as the text actually reaches,
        // not as far as it would have without the limit.
        let bg_rect = RECT {
            left: label_x - label_pad,
            top: self.bounds.top - thickness,
            right: text_x + drawn_w + label_pad,
            bottom: self.bounds.top + thickness,
        };
        let bg_brush = CreateSolidBrush(COLORREF(panel_bg.to_colorref()));
        FillRect(hdc, &bg_rect, bg_brush);
        let _ = DeleteObject(bg_brush);

        if let Some(status) = style.status {
            let dot_top = self.bounds.top - dot_size / 2;
            let brush = CreateSolidBrush(COLORREF(status.to_colorref()));
            let rgn = CreateRoundRectRgn(
                label_x,
                dot_top,
                label_x + dot_size + 1,
                dot_top + dot_size + 1,
                dot_size,
                dot_size,
            );
            let _ = FillRgn(hdc, rgn, brush);
            let _ = DeleteObject(rgn);
            let _ = DeleteObject(brush);
        }

        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(label_fg.to_colorref()));
        // DrawText rather than ExtTextOut: it ellipsizes to the rect, which
        // is the whole point of the limit above. The rect's top is where
        // ExtTextOut's y anchored — the top of the character cell — so the
        // text sits exactly where it did before.
        let text_bottom = label_y + tm.tmAscent + tm.tmDescent;
        let mut label_wide = label_wide;
        let mut label_rect = RECT {
            left: text_x,
            top: label_y,
            right: text_right,
            bottom: text_bottom,
        };
        let _ = DrawTextW(
            hdc,
            &mut label_wide,
            &mut label_rect,
            DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );

        // Project at the far end, dimmer than the title so the two read as
        // detail and context rather than competing labels.
        if let Some(project_left) = project_left {
            let project_bg = RECT {
                left: project_left - label_pad,
                top: self.bounds.top - thickness,
                right: content_right + label_pad,
                bottom: self.bounds.top + thickness,
            };
            let brush = CreateSolidBrush(COLORREF(panel_bg.to_colorref()));
            FillRect(hdc, &project_bg, brush);
            let _ = DeleteObject(brush);

            let _ = SetTextColor(hdc, COLORREF(Color::from_hex(PROJECT_FG_HEX).to_colorref()));
            let mut project_wide = project_wide;
            let mut project_rect = RECT {
                left: project_left,
                top: label_y,
                right: content_right,
                bottom: text_bottom,
            };
            let _ = DrawTextW(
                hdc,
                &mut project_wide,
                &mut project_rect,
                DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
        }

        SelectObject(hdc, old_label_font);
        let _ = DeleteObject(label_font);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The project is context, the title is what tells two cells apart —
    /// so a frame too narrow for both keeps the title.
    #[test]
    fn a_narrow_frame_drops_the_project_rather_than_the_title() {
        // Roomy: both run, title takes what the project leaves.
        let (title_right, project_left) = label_runs(20, 400, 90, 12, 56);
        assert_eq!(project_left, Some(310));
        assert_eq!(title_right, 298);

        // Tight: the title would be squeezed under the floor, so the
        // project goes and the title gets the whole run.
        let (title_right, project_left) = label_runs(20, 120, 90, 12, 56);
        assert_eq!(project_left, None);
        assert_eq!(title_right, 120);
    }

    /// A long directory name can't take the frame over from the title.
    #[test]
    fn the_project_never_takes_more_than_a_third_of_the_run() {
        let (title_right, project_left) = label_runs(0, 300, 280, 12, 56);
        assert_eq!(project_left, Some(200), "project capped at a third");
        assert_eq!(title_right, 188);
    }

    #[test]
    fn a_session_with_no_project_gives_the_title_everything() {
        assert_eq!(label_runs(20, 400, 0, 12, 56), (400, None));
    }

    /// The cross is two strokes over one box; if that box isn't square with
    /// an even extent, the arms come out unequal and the whole glyph looks
    /// tipped over.
    #[test]
    fn the_close_cross_is_square_and_even_at_every_dpi() {
        for dpi in [96, 120, 144, 168, 192, 240] {
            let mut view = SessionView::new_dormant(HWND::default(), 0, "x", "cmd", None);
            view.set_bounds(
                RECT {
                    left: 0,
                    top: 0,
                    right: 400,
                    bottom: 300,
                },
                dpi,
                9,
            );
            let r = view.close_rect(dpi);
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            assert_eq!(w, h, "dpi {dpi}: not square");
            assert_eq!(w % 2, 0, "dpi {dpi}: odd extent {w}");
            // Centred on the frame's top edge, same distance either side.
            assert_eq!(0 - r.top, r.bottom - 0, "dpi {dpi}: off the border line");
        }
    }
}
