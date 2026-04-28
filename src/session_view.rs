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
const PANEL_BG_HEX: &str = "#262624";
const LABEL_FG_HEX: &str = "#A8A29E";
/// Design pixels.
const BORDER_THICKNESS: i32 = 1;
const BORDER_RADIUS: i32 = 6;
const LABEL_INSET_X: i32 = 14;
const LABEL_PAD_X: i32 = 6;
const LABEL_FONT_POINT_SIZE: i32 = 9;
const TERMINAL_INSET: i32 = 1;

pub struct SessionView {
    bounds: RECT,
    name: String,
    terminal: TerminalView,
}

unsafe impl Send for SessionView {}
unsafe impl Sync for SessionView {}

impl SessionView {
    pub fn new(host_hwnd: HWND, notify_msg: u32, name: impl Into<String>, cmd: impl Into<String>) -> Self {
        Self {
            bounds: RECT::default(),
            name: name.into(),
            terminal: TerminalView::new(host_hwnd, notify_msg, cmd),
        }
    }

    pub fn set_bounds(&mut self, bounds: RECT, dpi: u32) {
        self.bounds = bounds;
        let inner = self.inner_terminal_rect(dpi);
        self.terminal.set_bounds(inner, dpi);
    }

    pub fn bounds(&self) -> RECT {
        self.bounds
    }

    pub fn terminal(&self) -> &TerminalView {
        &self.terminal
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

    pub fn paint(&self, hdc: HDC, dpi: u32) {
        // Paint the inner terminal first; the border draws on top so it
        // cleanly clips against whatever the terminal renders at the edges.
        self.terminal.paint(hdc, dpi);
        unsafe { self.paint_chrome(hdc, dpi) };
    }

    unsafe fn paint_chrome(&self, hdc: HDC, dpi: u32) {
        let scale = dpi as f64 / 96.0;
        let radius = (BORDER_RADIUS as f64 * scale).round().max(1.0) as i32;
        let thickness = (BORDER_THICKNESS as f64 * scale).round().max(1.0) as i32;
        let label_inset = (LABEL_INSET_X as f64 * scale).round() as i32;
        let label_pad = (LABEL_PAD_X as f64 * scale).round() as i32;

        let border_color = Color::from_hex(BORDER_COLOR_HEX);
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
        // Center the label vertically on the top border edge.
        let label_y = self.bounds.top - (tm.tmAscent + tm.tmDescent) / 2;

        // Erase the slice of border that lies behind the label so the line
        // visually breaks around it.
        let bg_rect = RECT {
            left: label_x - label_pad,
            top: self.bounds.top - thickness,
            right: label_x + text_size.cx + label_pad,
            bottom: self.bounds.top + thickness,
        };
        let bg_brush = CreateSolidBrush(COLORREF(panel_bg.to_colorref()));
        FillRect(hdc, &bg_rect, bg_brush);
        let _ = DeleteObject(bg_brush);

        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(label_fg.to_colorref()));
        let _ = ExtTextOutW(
            hdc,
            label_x,
            label_y,
            ETO_OPTIONS(0),
            None,
            PCWSTR::from_raw(label_wide.as_ptr()),
            label_wide.len() as u32,
            None,
        );

        SelectObject(hdc, old_label_font);
        let _ = DeleteObject(label_font);
    }
}
