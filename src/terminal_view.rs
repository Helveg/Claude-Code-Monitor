//! Reusable terminal view. A `TerminalView` owns one PTY-backed `Terminal`
//! and knows how to paint itself, route mouse/keyboard input, copy/paste
//! its selection, and report when its grid needs redrawing. It does not own
//! a Win32 HWND of its own — instead the host window passes events in via
//! the public methods, and paints by handing the view a region of an HDC.
//! That keeps multiple terminals on a single window cheap.
//!
//! The view exposes its own bounds (in the host's client coordinates) and
//! its own paint-pending flag. The host can ask each view it hosts whether
//! a repaint is needed and clear the flag before invalidating.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VK_DELETE, VK_DOWN, VK_END, VK_F1, VK_F2, VK_F3,
    VK_F4, VK_F5, VK_F6, VK_F7, VK_F8, VK_F9, VK_F10, VK_F11, VK_F12, VK_HOME, VK_INSERT, VK_LEFT,
    VK_NEXT, VK_PRIOR, VK_RIGHT, VK_SHIFT, VK_TAB, VK_UP,
};

use crate::native_interop::{self, Color};
use crate::terminal::{Cell, Terminal};

pub const FONT_POINT_SIZE: i32 = 11;
const TERMINAL_PADDING: i32 = 8;

const CLAUDE_GREY_HEX: &str = "#262624";
const DEFAULT_FG_HEX: &str = "#E8E8E8";

#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub anchor_row: u16,
    pub anchor_col: u16,
    pub head_row: u16,
    pub head_col: u16,
}

impl Selection {
    fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        let a = (self.anchor_row, self.anchor_col);
        let h = (self.head_row, self.head_col);
        if a <= h {
            (a, h)
        } else {
            (h, a)
        }
    }

    fn contains(&self, row: u16, col: u16) -> bool {
        let (start, end) = self.ordered();
        let pos = (row, col);
        pos >= start && pos < end
    }
}

/// Options passed to [`render_grid_region`] for partial / scaled grid renders
/// (cards previews use this with a smaller font and a row tail).
pub struct RenderOptions {
    pub dpi: u32,
    /// Font size in points. Lower values = smaller cells = more rows fit.
    pub font_pt: i32,
    /// Range of grid rows to draw, mapped to bounds top-down. Clamped to
    /// what fits in `bounds` and to `grid.rows`.
    pub rows: std::ops::Range<u16>,
    /// Selection highlight to apply to matching cells (rows are in grid
    /// coordinates, same as `rows`).
    pub selection: Option<Selection>,
    /// When true and the cursor's row is in the rendered range, draw the
    /// inverted-cell cursor.
    pub show_cursor: bool,
}

pub struct TerminalView {
    /// Pixel rect in the host window's client coords.
    bounds: RECT,
    terminal: Option<Terminal>,
    selection: Mutex<Option<Selection>>,
    mouse_dragging: AtomicBool,
    host_hwnd: HWND,
    notify_msg: u32,
    cmd: String,
    cwd: Option<std::path::PathBuf>,
}

// HWND is the only !Send field; access is gated by the panel's mutex and the
// Win32 message-passing APIs we use from the worker thread are thread-safe.
unsafe impl Send for TerminalView {}
unsafe impl Sync for TerminalView {}

impl TerminalView {
    /// Create the view. The PTY isn't spawned until `set_bounds` is called
    /// with a non-empty rect — the bounds determine cell metrics and grid
    /// dimensions.
    pub fn new(
        host_hwnd: HWND,
        notify_msg: u32,
        cmd: impl Into<String>,
        cwd: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            bounds: RECT::default(),
            terminal: None,
            selection: Mutex::new(None),
            mouse_dragging: AtomicBool::new(false),
            host_hwnd,
            notify_msg,
            cmd: cmd.into(),
            cwd,
        }
    }

    #[allow(dead_code)]
    pub fn bounds(&self) -> RECT {
        self.bounds
    }

    /// Move/resize the view in the host's client coords. Spawns the PTY on
    /// the first call; subsequent calls resize the existing PTY.
    pub fn set_bounds(&mut self, bounds: RECT, dpi: u32) {
        self.bounds = bounds;
        let Some((cols, rows)) = self.term_dimensions(dpi) else {
            return;
        };
        match &mut self.terminal {
            Some(t) => t.resize(cols, rows),
            None => match Terminal::spawn(
                cols,
                rows,
                &self.cmd,
                self.cwd.as_deref(),
                self.host_hwnd,
                self.notify_msg,
            ) {
                Ok(t) => self.terminal = Some(t),
                Err(err) => crate::diagnose::log(format!("terminal spawn failed: {err}")),
            },
        }
    }

    /// True if (x, y) — in host client coords — is inside this view's bounds.
    #[allow(dead_code)]
    pub fn hit_test(&self, x: i32, y: i32) -> bool {
        x >= self.bounds.left
            && x < self.bounds.right
            && y >= self.bounds.top
            && y < self.bounds.bottom
    }

    /// Returns the paint-pending flag of the underlying terminal so the host
    /// can clear it before invalidating.
    pub fn clear_paint_pending(&self) {
        if let Some(t) = self.terminal.as_ref() {
            t.paint_pending().store(false, Ordering::Release);
        }
    }

    /// HWND that hosts this view — used by tiles that need to call the
    /// `host_hwnd`-keyed APIs (font metrics, invalidate).
    pub fn host_hwnd(&self) -> HWND {
        self.host_hwnd
    }

    /// Clone of the terminal's `Arc<Mutex<Grid>>` for read-only access from
    /// other tiles (e.g. the cards-grid live preview).
    pub fn grid_arc(
        &self,
    ) -> Option<std::sync::Arc<std::sync::Mutex<crate::terminal::Grid>>> {
        self.terminal.as_ref().map(|t| t.grid.clone())
    }

    pub fn last_output_ms(&self) -> u64 {
        self.terminal.as_ref().map(|t| t.last_output_ms()).unwrap_or(0)
    }

    pub fn last_input_ms(&self) -> u64 {
        self.terminal.as_ref().map(|t| t.last_input_ms()).unwrap_or(0)
    }

    /// Paint into the host's HDC. The view paints inside its `bounds`.
    pub fn paint(&self, hdc: HDC, dpi: u32) {
        let area = self.term_area(dpi);

        // Always paint the full panel bg first — even if the terminal hasn't
        // spawned yet, we want a clean rect.
        let bg_default = ansi_default_bg();
        unsafe {
            let panel_bg_brush = CreateSolidBrush(COLORREF(bg_default.to_colorref()));
            FillRect(hdc, &self.bounds, panel_bg_brush);
            let _ = DeleteObject(panel_bg_brush);
            let _ = SetBkMode(hdc, TRANSPARENT);
        }

        let Some(terminal) = self.terminal.as_ref() else {
            return;
        };
        let grid_arc = terminal.grid.clone();
        let grid = match grid_arc.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };

        let selection = self
            .selection
            .lock()
            .ok()
            .and_then(|g| *g);
        let opts = RenderOptions {
            dpi,
            font_pt: FONT_POINT_SIZE,
            rows: 0..grid.rows,
            selection,
            show_cursor: true,
        };
        render_grid_region(hdc, area, &grid, self.host_hwnd, &opts);
    }

    pub fn handle_mouse_down(&self, x: i32, y: i32) {
        if let Some((row, col)) = self.cell_at(x, y) {
            {
                let mut sel = self.selection.lock().unwrap_or_else(|e| e.into_inner());
                *sel = Some(Selection {
                    anchor_row: row,
                    anchor_col: col,
                    head_row: row,
                    head_col: col,
                });
            }
            self.mouse_dragging.store(true, Ordering::Release);
            unsafe {
                SetCapture(self.host_hwnd);
            }
            unsafe {
                let _ = InvalidateRect(self.host_hwnd, None, false);
            }
        }
    }

    pub fn handle_mouse_move(&self, x: i32, y: i32) {
        if !self.mouse_dragging.load(Ordering::Acquire) {
            return;
        }
        if let Some((row, col)) = self.cell_at(x, y) {
            let mut sel = self.selection.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(s) = sel.as_mut() {
                if s.head_row != row || s.head_col != col {
                    s.head_row = row;
                    s.head_col = col;
                    drop(sel);
                    unsafe {
                        let _ = InvalidateRect(self.host_hwnd, None, false);
                    }
                }
            }
        }
    }

    pub fn handle_mouse_up(&self) {
        if self.mouse_dragging.swap(false, Ordering::AcqRel) {
            unsafe {
                let _ = ReleaseCapture();
            }
            // Collapse zero-area selections so they don't render.
            let mut sel = self.selection.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(s) = sel.as_ref() {
                if s.anchor_row == s.head_row && s.anchor_col == s.head_col {
                    *sel = None;
                    drop(sel);
                    unsafe {
                        let _ = InvalidateRect(self.host_hwnd, None, false);
                    }
                }
            }
        }
    }

    /// Forward a WM_CHAR code to the PTY, with platform-specific remappings.
    /// Returns true if handled (selection-aware Ctrl+C copy, Ctrl+V paste,
    /// or Backspace remap).
    pub fn handle_char(&self, code: u32) -> bool {
        // Ctrl+C: copy selection if one is active; otherwise let the host
        // forward 0x03 as the interrupt to the PTY.
        if code == 0x03 {
            let sel_copy = {
                let g = self.selection.lock().unwrap_or_else(|e| e.into_inner());
                *g
            };
            if let Some(sel) = sel_copy {
                if let Some(text) = self.terminal.as_ref().and_then(|t| {
                    t.grid
                        .lock()
                        .ok()
                        .map(|g| selection_text(&g, &sel))
                }) {
                    unsafe {
                        copy_text_to_clipboard(self.host_hwnd, &text);
                    }
                }
                *self.selection.lock().unwrap_or_else(|e| e.into_inner()) = None;
                unsafe {
                    let _ = InvalidateRect(self.host_hwnd, None, false);
                }
                return true;
            }
        }

        // Ctrl+V: paste clipboard text.
        if code == 0x16 {
            if let Some(text) = unsafe { paste_clipboard_text(self.host_hwnd) } {
                if let Some(t) = self.terminal.as_ref() {
                    t.write_input(text.as_bytes());
                }
            }
            return true;
        }

        // Backspace remap. Windows is inverted from Unix:
        //   plain Backspace → 0x08 ; Ctrl+Backspace → 0x7f
        // Unix-style apps use 0x7f for delete-char and 0x08 for delete-word.
        if code == 0x08 {
            if let Some(t) = self.terminal.as_ref() {
                t.write_input(&[0x7f]);
            }
            return true;
        }
        if code == 0x7f {
            if let Some(t) = self.terminal.as_ref() {
                t.write_input(&[0x08]);
            }
            return true;
        }

        if let Some(ch) = char::from_u32(code) {
            let mut buf = [0u8; 4];
            let s = ch.encode_utf8(&mut buf);
            if let Some(t) = self.terminal.as_ref() {
                t.write_input(s.as_bytes());
            }
        }
        true
    }

    /// Encode and forward a WM_KEYDOWN virtual-key. Returns true when the key
    /// was handled (a non-empty escape sequence was sent).
    pub fn handle_key_down(&self, vk: u32) -> bool {
        let bytes = encode_key(vk);
        if bytes.is_empty() {
            return false;
        }
        if let Some(t) = self.terminal.as_ref() {
            t.write_input(&bytes);
        }
        true
    }

    #[allow(dead_code)]
    pub fn drop_terminal(&mut self) {
        self.terminal = None;
    }

    fn term_area(&self, dpi: u32) -> RECT {
        let scale = dpi as f64 / 96.0;
        let pad = (TERMINAL_PADDING as f64 * scale).round() as i32;
        RECT {
            left: self.bounds.left + pad,
            top: self.bounds.top + pad,
            right: self.bounds.right - pad,
            bottom: self.bounds.bottom - pad,
        }
    }

    fn term_dimensions(&self, dpi: u32) -> Option<(u16, u16)> {
        unsafe {
            let area = self.term_area(dpi);
            let font = create_term_font(dpi, FONT_POINT_SIZE);
            if font.is_invalid() {
                return None;
            }
            let (cw, ch) = font_cell_metrics(self.host_hwnd, font);
            let _ = DeleteObject(font);
            if cw <= 0 || ch <= 0 {
                return None;
            }
            let cols = ((area.right - area.left) / cw).max(20) as u16;
            let rows = ((area.bottom - area.top) / ch).max(5) as u16;
            Some((cols, rows))
        }
    }

    fn cell_at(&self, x: i32, y: i32) -> Option<(u16, u16)> {
        unsafe {
            let dpi = GetDpiForWindow(self.host_hwnd).max(96);
            let area = self.term_area(dpi);
            if !point_in(&area, x, y) {
                return None;
            }
            let font = create_term_font(dpi, FONT_POINT_SIZE);
            if font.is_invalid() {
                return None;
            }
            let (cw, ch) = font_cell_metrics(self.host_hwnd, font);
            let _ = DeleteObject(font);
            if cw <= 0 || ch <= 0 {
                return None;
            }
            let col = ((x - area.left) / cw).max(0) as u16;
            let row = ((y - area.top) / ch).max(0) as u16;
            Some((row, col))
        }
    }
}

fn point_in(rect: &RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

/// Render a slice of `grid` into `bounds` with the given font size + options.
/// Used both by `TerminalView::paint` (full grid render) and by the live
/// preview tiles (subset render at smaller font). Caller is responsible for
/// painting the background of `bounds` before calling — this function only
/// paints non-default cell backgrounds, glyphs, and (optionally) the cursor.
pub fn render_grid_region(
    hdc: HDC,
    bounds: RECT,
    grid: &crate::terminal::Grid,
    host_hwnd: HWND,
    opts: &RenderOptions,
) {
    let main_font = unsafe { create_term_font(opts.dpi, opts.font_pt) };
    let symbol_font = unsafe { create_symbol_font(opts.dpi, opts.font_pt) };
    let old_font = unsafe { SelectObject(hdc, main_font) };
    let (cell_w, cell_h) = unsafe { font_cell_metrics(host_hwnd, main_font) };
    if cell_w <= 0 || cell_h <= 0 {
        unsafe {
            SelectObject(hdc, old_font);
            let _ = DeleteObject(main_font);
            let _ = DeleteObject(symbol_font);
        }
        return;
    }

    unsafe {
        let _ = SetBkMode(hdc, TRANSPARENT);
    }

    // Clamp the rendered rows to what the bounds can fit.
    let bounds_rows = ((bounds.bottom - bounds.top) / cell_h).max(0) as u16;
    let bounds_cols = ((bounds.right - bounds.left) / cell_w).max(0) as u16;
    let row_start = opts.rows.start.min(grid.rows);
    let row_end = opts.rows.end.min(grid.rows).min(row_start + bounds_rows);
    let visible_cols = grid.cols.min(bounds_cols);

    let bg_default = ansi_default_bg();
    let selection = opts.selection;
    let cell_render_colors = |cell: &Cell, row: u16, col: u16| -> (Color, Color) {
        let (fg, bg) = cell_colors(cell);
        if selection.map(|s| s.contains(row, col)).unwrap_or(false) {
            (bg, fg)
        } else {
            (fg, bg)
        }
    };

    // Pre-compute glyph fallback decisions for the rendered region in one
    // batched GetGlyphIndicesW call.
    let render_rows = row_end.saturating_sub(row_start);
    let total_cells = render_rows as usize * visible_cols as usize;
    let mut row_chars: Vec<u16> = Vec::with_capacity(total_cells);
    for row in row_start..row_end {
        for col in 0..visible_cols {
            let cell = grid.cell(row, col);
            let mut buf = [0u16; 2];
            let s = cell.ch.encode_utf16(&mut buf);
            row_chars.push(s[0]);
        }
    }
    let mut glyph_indices = vec![0u16; total_cells];
    if total_cells > 0 {
        unsafe {
            let _ = GetGlyphIndicesW(
                hdc,
                PCWSTR::from_raw(row_chars.as_ptr()),
                row_chars.len() as i32,
                glyph_indices.as_mut_ptr(),
                GGI_MARK_NONEXISTING_GLYPHS,
            );
        }
    }
    let needs_symbol = |row: u16, col: u16| -> bool {
        let i = (row - row_start) as usize * visible_cols as usize + col as usize;
        glyph_indices.get(i).copied().unwrap_or(0) == 0xFFFF
    };

    let pixel_y = |row: u16| -> i32 { bounds.top + (row - row_start) as i32 * cell_h };

    // Pass 1: backgrounds — coalesce horizontal runs.
    for row in row_start..row_end {
        let y = pixel_y(row);
        let mut col = 0u16;
        while col < visible_cols {
            let cell = grid.cell(row, col);
            let (_, bg) = cell_render_colors(&cell, row, col);
            let mut run_end = col + 1;
            while run_end < visible_cols {
                let next = grid.cell(row, run_end);
                let (_, next_bg) = cell_render_colors(&next, row, run_end);
                if next_bg.r != bg.r || next_bg.g != bg.g || next_bg.b != bg.b {
                    break;
                }
                run_end += 1;
            }
            if bg.r != bg_default.r || bg.g != bg_default.g || bg.b != bg_default.b {
                let r = RECT {
                    left: bounds.left + col as i32 * cell_w,
                    top: y,
                    right: bounds.left + run_end as i32 * cell_w,
                    bottom: y + cell_h,
                };
                let brush = unsafe { CreateSolidBrush(COLORREF(bg.to_colorref())) };
                unsafe {
                    FillRect(hdc, &r, brush);
                    let _ = DeleteObject(brush);
                }
            }
            col = run_end;
        }
    }

    // Pass 2: glyphs — group runs by (fg, font choice).
    for row in row_start..row_end {
        let y = pixel_y(row);
        let mut col = 0u16;
        while col < visible_cols {
            let cell = grid.cell(row, col);
            let (fg, _) = cell_render_colors(&cell, row, col);
            let want_symbol = needs_symbol(row, col);
            let mut text_buf: Vec<u16> = Vec::new();
            let mut dx_buf: Vec<i32> = Vec::new();
            let mut run_end = col;
            while run_end < visible_cols {
                let c = grid.cell(row, run_end);
                let (next_fg, _) = cell_render_colors(&c, row, run_end);
                if next_fg.r != fg.r || next_fg.g != fg.g || next_fg.b != fg.b {
                    break;
                }
                if needs_symbol(row, run_end) != want_symbol {
                    break;
                }
                let mut buf = [0u16; 2];
                let s = c.ch.encode_utf16(&mut buf);
                let s_len = s.len();
                text_buf.extend_from_slice(s);
                for i in 0..s_len {
                    dx_buf.push(if i + 1 == s_len { cell_w } else { 0 });
                }
                run_end += 1;
            }
            if !text_buf.is_empty() {
                unsafe {
                    SelectObject(hdc, if want_symbol { symbol_font } else { main_font });
                    let _ = SetTextColor(hdc, COLORREF(fg.to_colorref()));
                }
                let x = bounds.left + col as i32 * cell_w;
                unsafe {
                    let _ = ExtTextOutW(
                        hdc,
                        x,
                        y,
                        ETO_OPTIONS(0),
                        None,
                        PCWSTR::from_raw(text_buf.as_ptr()),
                        text_buf.len() as u32,
                        Some(dx_buf.as_ptr()),
                    );
                }
            }
            col = run_end;
        }
    }

    // Cursor: invert the cell under the cursor if it's in the rendered range.
    if opts.show_cursor
        && grid.cursor_visible
        && grid.cursor_row >= row_start
        && grid.cursor_row < row_end
        && grid.cursor_col < visible_cols
    {
        let cell = grid.cell(grid.cursor_row, grid.cursor_col);
        let (fg, _bg) = cell_colors(&cell);
        let y = pixel_y(grid.cursor_row);
        let r = RECT {
            left: bounds.left + grid.cursor_col as i32 * cell_w,
            top: y,
            right: bounds.left + (grid.cursor_col as i32 + 1) * cell_w,
            bottom: y + cell_h,
        };
        let brush = unsafe { CreateSolidBrush(COLORREF(fg.to_colorref())) };
        unsafe {
            FillRect(hdc, &r, brush);
            let _ = DeleteObject(brush);
            let _ = SetTextColor(hdc, COLORREF(bg_default.to_colorref()));
        }
        let mut buf = [0u16; 2];
        let s = cell.ch.encode_utf16(&mut buf);
        let cursor_font = if needs_symbol(grid.cursor_row, grid.cursor_col) {
            symbol_font
        } else {
            main_font
        };
        unsafe {
            SelectObject(hdc, cursor_font);
        }
        let dx: Vec<i32> = (0..s.len())
            .map(|i| if i + 1 == s.len() { cell_w } else { 0 })
            .collect();
        unsafe {
            let _ = ExtTextOutW(
                hdc,
                r.left,
                r.top,
                ETO_OPTIONS(0),
                None,
                PCWSTR::from_raw(s.as_ptr()),
                s.len() as u32,
                Some(dx.as_ptr()),
            );
        }
    }

    unsafe {
        SelectObject(hdc, old_font);
        let _ = DeleteObject(main_font);
        let _ = DeleteObject(symbol_font);
    }
}

fn ansi_default_fg() -> Color {
    Color::from_hex(DEFAULT_FG_HEX)
}

fn ansi_default_bg() -> Color {
    Color::from_hex(CLAUDE_GREY_HEX)
}

fn cell_colors(cell: &Cell) -> (Color, Color) {
    let fg_default = ansi_default_fg();
    let bg_default = ansi_default_bg();
    let fg = match cell.attrs.fg {
        Some(c) => {
            let (r, g, b) = c.to_rgb((fg_default.r, fg_default.g, fg_default.b));
            Color::new(r, g, b)
        }
        None => fg_default,
    };
    let bg = match cell.attrs.bg {
        Some(c) => {
            let (r, g, b) = c.to_rgb((bg_default.r, bg_default.g, bg_default.b));
            Color::new(r, g, b)
        }
        None => bg_default,
    };
    if cell.attrs.reverse {
        (bg, fg)
    } else {
        (fg, bg)
    }
}

unsafe fn create_term_font(dpi: u32, font_pt: i32) -> HFONT {
    let height = -(font_pt * dpi as i32 / 72);
    let candidates = ["Cascadia Code", "Cascadia Mono", "Consolas", "Courier New"];
    for name in candidates {
        let face = native_interop::wide_str(name);
        let f = CreateFontW(
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
        if !f.is_invalid() {
            return f;
        }
    }
    HFONT::default()
}

unsafe fn create_symbol_font(dpi: u32, font_pt: i32) -> HFONT {
    let height = -(font_pt * dpi as i32 / 72);
    let face = native_interop::wide_str("Segoe UI Symbol");
    CreateFontW(
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
        (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
        PCWSTR::from_raw(face.as_ptr()),
    )
}

unsafe fn font_cell_metrics(hwnd: HWND, font: HFONT) -> (i32, i32) {
    let dc = GetDC(hwnd);
    let old = SelectObject(dc, font);
    let mut tm = TEXTMETRICW::default();
    let _ = GetTextMetricsW(dc, &mut tm);
    let mut size = SIZE::default();
    let m: [u16; 1] = [b'M' as u16];
    let _ = GetTextExtentPoint32W(dc, &m, &mut size);
    SelectObject(dc, old);
    ReleaseDC(hwnd, dc);
    let cw = if size.cx > 0 {
        size.cx
    } else {
        tm.tmAveCharWidth.max(1)
    };
    let ch = (tm.tmHeight + tm.tmExternalLeading).max(1);
    (cw, ch)
}

fn selection_text(grid: &crate::terminal::Grid, sel: &Selection) -> String {
    let ((srow, scol), (erow, ecol)) = sel.ordered();
    let mut out = String::new();
    let cols = grid.cols;
    for row in srow..=erow {
        let start_col = if row == srow { scol } else { 0 };
        let end_col = if row == erow { ecol } else { cols };
        let mut line = String::new();
        for col in start_col..end_col {
            line.push(grid.cell(row, col).ch);
        }
        let trimmed = line.trim_end_matches(' ').to_string();
        out.push_str(&trimmed);
        if row != erow {
            out.push_str("\r\n");
        }
    }
    out
}

unsafe fn copy_text_to_clipboard(hwnd: HWND, text: &str) {
    if text.is_empty() {
        return;
    }
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * 2;
    let h_mem = match GlobalAlloc(GMEM_MOVEABLE, bytes) {
        Ok(h) if !h.is_invalid() => h,
        _ => return,
    };
    let dst = GlobalLock(h_mem) as *mut u16;
    if dst.is_null() {
        return;
    }
    std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
    let _ = GlobalUnlock(h_mem);

    if OpenClipboard(hwnd).is_err() {
        return;
    }
    let _ = EmptyClipboard();
    let h_handle = HANDLE(h_mem.0);
    let _ = SetClipboardData(CF_UNICODETEXT.0 as u32, h_handle);
    let _ = CloseClipboard();
}

unsafe fn paste_clipboard_text(hwnd: HWND) -> Option<String> {
    if OpenClipboard(hwnd).is_err() {
        return None;
    }
    let result = (|| -> Option<String> {
        let h = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
        if h.is_invalid() {
            return None;
        }
        let h_global = windows::Win32::Foundation::HGLOBAL(h.0);
        let ptr = GlobalLock(h_global) as *const u16;
        if ptr.is_null() {
            return None;
        }
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
            if len > 1024 * 1024 {
                break;
            }
        }
        let slice = std::slice::from_raw_parts(ptr, len);
        let s = String::from_utf16_lossy(slice);
        let _ = GlobalUnlock(h_global);
        Some(s)
    })();
    let _ = CloseClipboard();
    result
}

/// Encode keystrokes that don't have a printable WM_CHAR equivalent. Plain
/// Enter, Tab, Escape, and Backspace are intentionally excluded — TranslateMessage
/// posts a WM_CHAR for them, which the host routes through `handle_char`.
/// Shift+Tab is the exception: TranslateMessage skips it.
fn encode_key(vk: u32) -> Vec<u8> {
    let shift_held = unsafe { (GetKeyState(VK_SHIFT.0 as i32) as i16) < 0 };
    match vk {
        v if v == VK_TAB.0 as u32 && shift_held => b"\x1b[Z".to_vec(),
        v if v == VK_UP.0 as u32 => b"\x1b[A".to_vec(),
        v if v == VK_DOWN.0 as u32 => b"\x1b[B".to_vec(),
        v if v == VK_RIGHT.0 as u32 => b"\x1b[C".to_vec(),
        v if v == VK_LEFT.0 as u32 => b"\x1b[D".to_vec(),
        v if v == VK_HOME.0 as u32 => b"\x1b[H".to_vec(),
        v if v == VK_END.0 as u32 => b"\x1b[F".to_vec(),
        v if v == VK_PRIOR.0 as u32 => b"\x1b[5~".to_vec(),
        v if v == VK_NEXT.0 as u32 => b"\x1b[6~".to_vec(),
        v if v == VK_INSERT.0 as u32 => b"\x1b[2~".to_vec(),
        v if v == VK_DELETE.0 as u32 => b"\x1b[3~".to_vec(),
        v if v == VK_F1.0 as u32 => b"\x1bOP".to_vec(),
        v if v == VK_F2.0 as u32 => b"\x1bOQ".to_vec(),
        v if v == VK_F3.0 as u32 => b"\x1bOR".to_vec(),
        v if v == VK_F4.0 as u32 => b"\x1bOS".to_vec(),
        v if v == VK_F5.0 as u32 => b"\x1b[15~".to_vec(),
        v if v == VK_F6.0 as u32 => b"\x1b[17~".to_vec(),
        v if v == VK_F7.0 as u32 => b"\x1b[18~".to_vec(),
        v if v == VK_F8.0 as u32 => b"\x1b[19~".to_vec(),
        v if v == VK_F9.0 as u32 => b"\x1b[20~".to_vec(),
        v if v == VK_F10.0 as u32 => b"\x1b[21~".to_vec(),
        v if v == VK_F11.0 as u32 => b"\x1b[23~".to_vec(),
        v if v == VK_F12.0 as u32 => b"\x1b[24~".to_vec(),
        _ => Vec::new(),
    }
}
