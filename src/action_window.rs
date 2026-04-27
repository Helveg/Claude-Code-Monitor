use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_DELETE, VK_DOWN, VK_END, VK_F1, VK_F2,
    VK_F3, VK_F4, VK_F5, VK_F6, VK_F7, VK_F8, VK_F9, VK_F10, VK_F11, VK_F12, VK_HOME, VK_INSERT,
    VK_LEFT, VK_NEXT, VK_PRIOR, VK_RIGHT, VK_SHIFT, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::native_interop::{self, Color};
use crate::terminal::{Cell, Terminal};

const TRIGGER_CLASS: &str = "ClaudeCodeUsageMonitorTrigger";
const PANEL_CLASS: &str = "ClaudeCodeUsageMonitorPanel";

/// Square edge length of the orange trigger button, design pixels at 96 DPI.
pub const TRIGGER_SIZE: i32 = 28;
/// Gap between the trigger button and the main widget, design pixels at 96 DPI.
pub const TRIGGER_GAP: i32 = 6;
const TRIGGER_RADIUS: i32 = 6;

const PANEL_W: i32 = 900;
const PANEL_H: i32 = 600;
const CLOSE_SIZE: i32 = 7;
const CLOSE_MARGIN: i32 = 10;
const CLOSE_STROKE: i32 = 2;

const CAPTION_STRIP: i32 = 26; // top drag strip height
const TERM_PAD_X: i32 = 12;
const TERM_PAD_TOP: i32 = 30;
const TERM_PAD_BOTTOM: i32 = 12;
const FONT_POINT_SIZE: i32 = 11;

const ORANGE_HEX: &str = "#D97757";
const CLAUDE_GREY_HEX: &str = "#262624";
const DEFAULT_FG_HEX: &str = "#E8E8E8";

const SHELL_CMD: &str = "cmd.exe";

pub const WM_APP_TERM_OUTPUT: u32 = WM_APP + 100;

/// Set by the terminal worker thread when output has arrived since the last
/// paint. Cleared by the panel's WM_APP_TERM_OUTPUT handler before painting.
/// Used to collapse repaint storms during heavy output bursts.
pub static TERM_PAINT_PENDING: AtomicBool = AtomicBool::new(false);

static PANEL_HWND: Mutex<isize> = Mutex::new(0);
static TERMINAL: Mutex<Option<Terminal>> = Mutex::new(None);
static SELECTION: Mutex<Option<Selection>> = Mutex::new(None);
static MOUSE_DRAGGING: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug)]
struct Selection {
    anchor_row: u16,
    anchor_col: u16,
    head_row: u16,
    head_col: u16,
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

pub fn register_classes(hinstance: HINSTANCE) {
    unsafe {
        let trigger_class = native_interop::wide_str(TRIGGER_CLASS);
        let trigger_wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(trigger_wnd_proc),
            hInstance: hinstance,
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_HAND).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(trigger_class.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassExW(&trigger_wc);

        let panel_class = native_interop::wide_str(PANEL_CLASS);
        let panel_wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(panel_wnd_proc),
            hInstance: hinstance,
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_IBEAM).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(panel_class.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassExW(&panel_wc);
    }
}

pub fn create_trigger(hinstance: HINSTANCE) -> Option<HWND> {
    unsafe {
        let class_name = native_interop::wide_str(TRIGGER_CLASS);
        let title = native_interop::wide_str("");
        match CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WS_POPUP,
            0,
            0,
            32,
            32,
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        ) {
            Ok(h) if !h.is_invalid() => Some(h),
            _ => None,
        }
    }
}

pub fn paint_trigger(hwnd: HWND, width: i32, height: i32) {
    if width <= 0 || height <= 0 {
        return;
    }
    let orange = Color::from_hex(ORANGE_HEX);
    let radius = (TRIGGER_RADIUS as f64 * width as f64 / TRIGGER_SIZE as f64).round() as i32;

    unsafe {
        let screen_dc = GetDC(hwnd);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let mem_dc = CreateCompatibleDC(screen_dc);
        let dib = CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
            .unwrap_or_default();
        if dib.is_invalid() || bits.is_null() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(hwnd, screen_dc);
            return;
        }
        let old_bmp = SelectObject(mem_dc, dib);
        let pixel_count = (width * height) as usize;
        std::ptr::write_bytes(bits as *mut u32, 0, pixel_count);

        let brush = CreateSolidBrush(COLORREF(orange.to_colorref()));
        let rgn = CreateRoundRectRgn(0, 0, width + 1, height + 1, radius * 2, radius * 2);
        let _ = FillRgn(mem_dc, rgn, brush);
        let _ = DeleteObject(rgn);
        let _ = DeleteObject(brush);

        let pixel_data = std::slice::from_raw_parts_mut(bits as *mut u32, pixel_count);
        for px in pixel_data.iter_mut() {
            let rgb = *px & 0x00FF_FFFF;
            if rgb != 0 {
                *px = 0xFF00_0000 | rgb;
            }
        }

        let pt_src = POINT { x: 0, y: 0 };
        let sz = SIZE {
            cx: width,
            cy: height,
        };
        let blend = BLENDFUNCTION {
            BlendOp: 0,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: 1,
        };
        let _ = UpdateLayeredWindow(
            hwnd,
            screen_dc,
            None,
            Some(&sz),
            mem_dc,
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(hwnd, screen_dc);
    }
}

unsafe extern "system" fn trigger_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_LBUTTONUP => {
            open_panel();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn open_panel() {
    {
        let existing = *PANEL_HWND.lock().unwrap_or_else(|e| e.into_inner());
        if existing != 0 {
            let hwnd = HWND(existing as *mut _);
            unsafe {
                if IsWindow(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_SHOW);
                    let _ = SetForegroundWindow(hwnd);
                    let _ = SetFocus(hwnd);
                    return;
                }
            }
        }
    }

    unsafe {
        let module = match GetModuleHandleW(PCWSTR::null()) {
            Ok(h) => h,
            Err(_) => return,
        };
        let hinstance = HINSTANCE(module.0);
        let class_name = native_interop::wide_str(PANEL_CLASS);
        let title = native_interop::wide_str("");

        let scale = system_dpi_scale();
        let w = (PANEL_W as f64 * scale).round() as i32;
        let h = (PANEL_H as f64 * scale).round() as i32;
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);
        let x = (screen_w - w) / 2;
        let y = (screen_h - h) / 2;

        let hwnd = match CreateWindowExW(
            WS_EX_APPWINDOW,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WS_POPUP,
            x,
            y,
            w,
            h,
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        ) {
            Ok(h) => h,
            Err(_) => return,
        };

        *PANEL_HWND.lock().unwrap_or_else(|e| e.into_inner()) = hwnd.0 as isize;

        // Spawn terminal sized to current client area.
        if let Some((cols, rows)) = current_term_dimensions(hwnd) {
            match Terminal::spawn(cols, rows, SHELL_CMD, hwnd) {
                Ok(term) => {
                    *TERMINAL.lock().unwrap_or_else(|e| e.into_inner()) = Some(term);
                }
                Err(err) => {
                    crate::diagnose::log(format!("terminal spawn failed: {err}"));
                }
            }
        }

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(hwnd);
    }
}

fn system_dpi_scale() -> f64 {
    unsafe {
        let dc = GetDC(HWND::default());
        if dc.is_invalid() {
            return 1.0;
        }
        let dpi_x = GetDeviceCaps(dc, LOGPIXELSX);
        ReleaseDC(HWND::default(), dc);
        if dpi_x <= 0 {
            1.0
        } else {
            dpi_x as f64 / 96.0
        }
    }
}

fn close_button_rect(client: &RECT, dpi: u32) -> RECT {
    let scale = dpi as f64 / 96.0;
    let size = (CLOSE_SIZE as f64 * scale).round() as i32;
    let margin = (CLOSE_MARGIN as f64 * scale).round() as i32;
    RECT {
        left: client.right - margin - size,
        top: margin,
        right: client.right - margin,
        bottom: margin + size,
    }
}

fn point_in(rect: &RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

/// Compute terminal drawing area within the panel client rect.
fn term_area(client: &RECT, dpi: u32) -> RECT {
    let scale = dpi as f64 / 96.0;
    let pad_x = (TERM_PAD_X as f64 * scale).round() as i32;
    let pad_top = (TERM_PAD_TOP as f64 * scale).round() as i32;
    let pad_bot = (TERM_PAD_BOTTOM as f64 * scale).round() as i32;
    RECT {
        left: client.left + pad_x,
        top: client.top + pad_top,
        right: client.right - pad_x,
        bottom: client.bottom - pad_bot,
    }
}

/// Caption strip at top — drag area, excluding the close button.
fn caption_strip(client: &RECT, dpi: u32) -> RECT {
    let scale = dpi as f64 / 96.0;
    let height = (CAPTION_STRIP as f64 * scale).round() as i32;
    RECT {
        left: client.left,
        top: client.top,
        right: client.right,
        bottom: client.top + height,
    }
}

/// Create a monospace font scaled for the given DPI. Caller owns the HFONT.
unsafe fn create_term_font(dpi: u32) -> HFONT {
    let height = -(FONT_POINT_SIZE * dpi as i32 / 72);
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

/// Fallback font for glyphs the main monospace font doesn't carry —
/// claude's UI uses Misc-Technical / Geometric-Shapes characters that
/// not every Cascadia install ships. Segoe UI Symbol covers most of them.
unsafe fn create_symbol_font(dpi: u32) -> HFONT {
    let height = -(FONT_POINT_SIZE * dpi as i32 / 72);
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

/// Returns (cell_width_px, cell_height_px) for a given font.
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

/// Map a client-area pixel coordinate to a (row, col) cell in the terminal
/// grid, or `None` if outside the terminal area.
fn cell_at(hwnd: HWND, x: i32, y: i32) -> Option<(u16, u16)> {
    unsafe {
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        let dpi = GetDpiForWindow(hwnd).max(96);
        let area = term_area(&client, dpi);
        if !point_in(&area, x, y) {
            return None;
        }
        let font = create_term_font(dpi);
        if font.is_invalid() {
            return None;
        }
        let (cw, ch) = font_cell_metrics(hwnd, font);
        let _ = DeleteObject(font);
        if cw <= 0 || ch <= 0 {
            return None;
        }
        let col = ((x - area.left) / cw).max(0) as u16;
        let row = ((y - area.top) / ch).max(0) as u16;
        Some((row, col))
    }
}

/// Build the selected text from the grid using line-wrap selection: first row
/// from anchor.col to end of row; intermediate rows are full lines; last row
/// from 0 to head.col (exclusive).
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
        // Trim trailing spaces from each row (typical terminal copy behavior).
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
        // Find null terminator
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

/// Compute the (cols, rows) the terminal should use for the current panel size.
fn current_term_dimensions(hwnd: HWND) -> Option<(u16, u16)> {
    unsafe {
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        let dpi = GetDpiForWindow(hwnd).max(96);
        let area = term_area(&client, dpi);
        let font = create_term_font(dpi);
        if font.is_invalid() {
            return None;
        }
        let (cw, ch) = font_cell_metrics(hwnd, font);
        let _ = DeleteObject(font);
        if cw <= 0 || ch <= 0 {
            return None;
        }
        let cols = ((area.right - area.left) / cw).max(20) as u16;
        let rows = ((area.bottom - area.top) / ch).max(5) as u16;
        Some((cols, rows))
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

unsafe fn draw_terminal(hdc: HDC, hwnd: HWND, client: &RECT, dpi: u32) {
    let area = term_area(client, dpi);
    let main_font = create_term_font(dpi);
    let symbol_font = create_symbol_font(dpi);
    let old_font = SelectObject(hdc, main_font);
    let (cell_w, cell_h) = font_cell_metrics(hwnd, main_font);
    if cell_w <= 0 || cell_h <= 0 {
        SelectObject(hdc, old_font);
        let _ = DeleteObject(main_font);
        let _ = DeleteObject(symbol_font);
        return;
    }

    let _ = SetBkMode(hdc, TRANSPARENT);

    let term_guard = TERMINAL.lock().unwrap_or_else(|e| e.into_inner());
    let term = match term_guard.as_ref() {
        Some(t) => t,
        None => {
            SelectObject(hdc, old_font);
            let _ = DeleteObject(main_font);
            let _ = DeleteObject(symbol_font);
            return;
        }
    };
    let grid_arc = term.grid.clone();
    drop(term_guard);

    let grid = match grid_arc.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };

    let bg_default = ansi_default_bg();
    let area_w_cells = ((area.right - area.left) / cell_w).max(0) as u16;
    let area_h_cells = ((area.bottom - area.top) / cell_h).max(0) as u16;
    let visible_cols = grid.cols.min(area_w_cells);
    let visible_rows = grid.rows.min(area_h_cells);

    let selection = SELECTION
        .lock()
        .ok()
        .and_then(|g| *g);
    let cell_render_colors = |cell: &Cell, row: u16, col: u16| -> (Color, Color) {
        let (fg, bg) = cell_colors(cell);
        if selection.map(|s| s.contains(row, col)).unwrap_or(false) {
            (bg, fg)
        } else {
            (fg, bg)
        }
    };

    // Pre-compute which cells need the symbol-font fallback. Done in a single
    // GetGlyphIndicesW call against the main font; if the index for a cell
    // is 0xFFFF the main font lacks that glyph and we render that cell with
    // the symbol font instead.
    let total_cells = visible_rows as usize * visible_cols as usize;
    let mut row_chars: Vec<u16> = Vec::with_capacity(total_cells);
    for row in 0..visible_rows {
        for col in 0..visible_cols {
            let cell = grid.cell(row, col);
            let mut buf = [0u16; 2];
            let s = cell.ch.encode_utf16(&mut buf);
            // Use the first u16; surrogate pairs (rare) get approximated.
            row_chars.push(s[0]);
        }
    }
    let mut glyph_indices = vec![0u16; total_cells];
    if total_cells > 0 {
        let _ = GetGlyphIndicesW(
            hdc,
            PCWSTR::from_raw(row_chars.as_ptr()),
            row_chars.len() as i32,
            glyph_indices.as_mut_ptr(),
            GGI_MARK_NONEXISTING_GLYPHS,
        );
    }
    let needs_symbol = |row: u16, col: u16| -> bool {
        let idx = row as usize * visible_cols as usize + col as usize;
        glyph_indices.get(idx).copied().unwrap_or(0) == 0xFFFF
    };

    // Pass 1: backgrounds. Coalesce horizontal runs of identical bg.
    for row in 0..visible_rows {
        let y = area.top + row as i32 * cell_h;
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
                    left: area.left + col as i32 * cell_w,
                    top: y,
                    right: area.left + run_end as i32 * cell_w,
                    bottom: y + cell_h,
                };
                let brush = CreateSolidBrush(COLORREF(bg.to_colorref()));
                FillRect(hdc, &r, brush);
                let _ = DeleteObject(brush);
            }
            col = run_end;
        }
    }

    // Pass 2: glyphs. Group runs by (fg color, font choice) so we can emit a
    // single ExtTextOutW per run while still routing missing-glyph cells
    // through the symbol font. lpdx forces every char to advance exactly
    // `cell_w` so proportional fallback fonts (Segoe UI Symbol) still align
    // to the monospace grid.
    for row in 0..visible_rows {
        let y = area.top + row as i32 * cell_h;
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
                // For surrogate pairs the final unit takes the cell's
                // advance; the leading high surrogate advances 0 so the
                // pair renders as a single glyph in one cell.
                for i in 0..s_len {
                    dx_buf.push(if i + 1 == s_len { cell_w } else { 0 });
                }
                run_end += 1;
            }
            if !text_buf.is_empty() {
                SelectObject(hdc, if want_symbol { symbol_font } else { main_font });
                let _ = SetTextColor(hdc, COLORREF(fg.to_colorref()));
                let x = area.left + col as i32 * cell_w;
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
            col = run_end;
        }
    }

    // Cursor: invert the cell under the cursor.
    if grid.cursor_visible
        && grid.cursor_row < visible_rows
        && grid.cursor_col < visible_cols
    {
        let cell = grid.cell(grid.cursor_row, grid.cursor_col);
        let (fg, _bg) = cell_colors(&cell);
        let r = RECT {
            left: area.left + grid.cursor_col as i32 * cell_w,
            top: area.top + grid.cursor_row as i32 * cell_h,
            right: area.left + (grid.cursor_col as i32 + 1) * cell_w,
            bottom: area.top + (grid.cursor_row as i32 + 1) * cell_h,
        };
        let brush = CreateSolidBrush(COLORREF(fg.to_colorref()));
        FillRect(hdc, &r, brush);
        let _ = DeleteObject(brush);
        let bg_color = ansi_default_bg();
        let _ = SetTextColor(hdc, COLORREF(bg_color.to_colorref()));
        let mut buf = [0u16; 2];
        let s = cell.ch.encode_utf16(&mut buf);
        let cursor_font = if needs_symbol(grid.cursor_row, grid.cursor_col) {
            symbol_font
        } else {
            main_font
        };
        SelectObject(hdc, cursor_font);
        let dx: Vec<i32> = (0..s.len())
            .map(|i| if i + 1 == s.len() { cell_w } else { 0 })
            .collect();
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

    SelectObject(hdc, old_font);
    let _ = DeleteObject(main_font);
    let _ = DeleteObject(symbol_font);
}

unsafe extern "system" fn panel_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCHITTEST => {
            let xs = (lparam.0 & 0xFFFF) as i16 as i32;
            let ys = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut pt = POINT { x: xs, y: ys };
            let _ = ScreenToClient(hwnd, &mut pt);
            let mut client = RECT::default();
            let _ = GetClientRect(hwnd, &mut client);
            let dpi = GetDpiForWindow(hwnd).max(96);
            let close = close_button_rect(&client, dpi);
            if point_in(&close, pt.x, pt.y) {
                return LRESULT(HTCLIENT as isize);
            }
            let cap = caption_strip(&client, dpi);
            if point_in(&cap, pt.x, pt.y) {
                return LRESULT(HTCAPTION as isize);
            }
            LRESULT(HTCLIENT as isize)
        }
        WM_LBUTTONDOWN => {
            let _ = SetFocus(hwnd);
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut client = RECT::default();
            let _ = GetClientRect(hwnd, &mut client);
            let dpi = GetDpiForWindow(hwnd).max(96);
            let close = close_button_rect(&client, dpi);
            if point_in(&close, x, y) {
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            // Begin a selection at the clicked cell.
            if let Some((row, col)) = cell_at(hwnd, x, y) {
                {
                    let mut sel = SELECTION.lock().unwrap_or_else(|e| e.into_inner());
                    *sel = Some(Selection {
                        anchor_row: row,
                        anchor_col: col,
                        head_row: row,
                        head_col: col,
                    });
                }
                MOUSE_DRAGGING.store(true, Ordering::Release);
                SetCapture(hwnd);
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if MOUSE_DRAGGING.load(Ordering::Acquire) {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                if let Some((row, col)) = cell_at(hwnd, x, y) {
                    let mut sel = SELECTION.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(s) = sel.as_mut() {
                        if s.head_row != row || s.head_col != col {
                            s.head_row = row;
                            s.head_col = col;
                            drop(sel);
                            let _ = InvalidateRect(hwnd, None, false);
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if MOUSE_DRAGGING.swap(false, Ordering::AcqRel) {
                let _ = ReleaseCapture();
                // Collapse zero-area selections so they don't render.
                let mut sel = SELECTION.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(s) = sel.as_ref() {
                    if s.anchor_row == s.head_row && s.anchor_col == s.head_col {
                        *sel = None;
                        drop(sel);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
            }
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut client = RECT::default();
            let _ = GetClientRect(hwnd, &mut client);
            let dpi = GetDpiForWindow(hwnd).max(96);

            // Double-buffer to avoid flicker.
            let width = client.right - client.left;
            let height = client.bottom - client.top;
            let mem_dc = CreateCompatibleDC(hdc);
            let mem_bmp = CreateCompatibleBitmap(hdc, width, height);
            let old_bmp = SelectObject(mem_dc, mem_bmp);

            paint_panel_chrome(mem_dc, &client, dpi);
            draw_terminal(mem_dc, hwnd, &client, dpi);

            let _ = BitBlt(hdc, 0, 0, width, height, mem_dc, 0, 0, SRCCOPY);

            SelectObject(mem_dc, old_bmp);
            let _ = DeleteObject(mem_bmp);
            let _ = DeleteDC(mem_dc);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_CHAR => {
            let code = wparam.0 as u32;

            // Ctrl+C: copy current selection to the clipboard if one exists,
            // then clear the selection. Without a selection, fall through and
            // forward 0x03 (ETX) so the running program receives an interrupt.
            if code == 0x03 {
                let sel_copy = {
                    let g = SELECTION.lock().unwrap_or_else(|e| e.into_inner());
                    *g
                };
                if let Some(sel) = sel_copy {
                    let text = {
                        let term_guard =
                            TERMINAL.lock().unwrap_or_else(|e| e.into_inner());
                        term_guard.as_ref().and_then(|term| {
                            term.grid.lock().ok().map(|g| selection_text(&g, &sel))
                        })
                    };
                    if let Some(t) = text {
                        copy_text_to_clipboard(hwnd, &t);
                    }
                    *SELECTION.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    let _ = InvalidateRect(hwnd, None, false);
                    return LRESULT(0);
                }
            }

            // Ctrl+V: paste clipboard text (if any) as input to the PTY.
            if code == 0x16 {
                if let Some(text) = paste_clipboard_text(hwnd) {
                    let term_guard = TERMINAL.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(term) = term_guard.as_ref() {
                        term.write_input(text.as_bytes());
                    }
                }
                return LRESULT(0);
            }

            // Backspace remap. Windows' WM_CHAR codes are inverted relative
            // to the Unix convention TUIs expect:
            //   plain Backspace → 0x08 ; Ctrl+Backspace → 0x7f
            // Unix-style apps use 0x7f for "delete char" and 0x08 for
            // "delete word", so we swap the two.
            if code == 0x08 {
                let term_guard = TERMINAL.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(term) = term_guard.as_ref() {
                    term.write_input(&[0x7f]);
                }
                return LRESULT(0);
            }
            if code == 0x7f {
                let term_guard = TERMINAL.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(term) = term_guard.as_ref() {
                    term.write_input(&[0x08]);
                }
                return LRESULT(0);
            }

            // Re-encode the UTF-16 code unit as UTF-8.
            if let Some(ch) = char::from_u32(code) {
                let mut buf = [0u8; 4];
                let s = ch.encode_utf8(&mut buf);
                let bytes = s.as_bytes().to_vec();
                let term_guard = TERMINAL.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(term) = term_guard.as_ref() {
                    term.write_input(&bytes);
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let vk = wparam.0 as u32;
            let bytes = encode_key(vk);
            if !bytes.is_empty() {
                let term_guard = TERMINAL.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(term) = term_guard.as_ref() {
                    term.write_input(&bytes);
                }
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        m if m == WM_APP_TERM_OUTPUT => {
            // Clear the pending flag BEFORE invalidating so any output
            // produced during the upcoming paint can post a fresh request.
            TERM_PAINT_PENDING.store(false, Ordering::Release);
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_SIZE => {
            if let Some((cols, rows)) = current_term_dimensions(hwnd) {
                let mut term_guard = TERMINAL.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(term) = term_guard.as_mut() {
                    term.resize(cols, rows);
                }
            }
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_DESTROY => {
            *PANEL_HWND.lock().unwrap_or_else(|e| e.into_inner()) = 0;
            *TERMINAL.lock().unwrap_or_else(|e| e.into_inner()) = None;
            *SELECTION.lock().unwrap_or_else(|e| e.into_inner()) = None;
            MOUSE_DRAGGING.store(false, Ordering::Release);
            TERM_PAINT_PENDING.store(false, Ordering::Release);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn paint_panel_chrome(hdc: HDC, client: &RECT, dpi: u32) {
    let claude_grey = Color::from_hex(CLAUDE_GREY_HEX);
    let orange = Color::from_hex(ORANGE_HEX);

    unsafe {
        let bg_brush = CreateSolidBrush(COLORREF(claude_grey.to_colorref()));
        FillRect(hdc, client, bg_brush);
        let _ = DeleteObject(bg_brush);

        let close = close_button_rect(client, dpi);
        let scale = dpi as f64 / 96.0;
        let pen_w = (CLOSE_STROKE as f64 * scale).round().max(1.0) as i32;
        let pen = CreatePen(PS_SOLID, pen_w, COLORREF(orange.to_colorref()));
        let old_pen = SelectObject(hdc, pen);
        let _ = MoveToEx(hdc, close.left, close.top, None);
        let _ = LineTo(hdc, close.right, close.bottom);
        let _ = MoveToEx(hdc, close.right, close.top, None);
        let _ = LineTo(hdc, close.left, close.bottom);
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(pen);
    }
}

/// Encode keystrokes that don't have a printable WM_CHAR equivalent. Plain
/// Enter, Tab, Escape, and Backspace are intentionally excluded — TranslateMessage
/// posts a WM_CHAR for them, which handles the encoding cleanly. Shift+Tab is
/// the exception: TranslateMessage skips it, so we encode CBT here.
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
