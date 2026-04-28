use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::native_interop::{self, Color};
use crate::session_view::SessionView;

const TRIGGER_CLASS: &str = "ClaudeCodeUsageMonitorTrigger";
const PANEL_CLASS: &str = "ClaudeCodeUsageMonitorPanel";

/// Square edge length of the orange trigger button, design pixels at 96 DPI.
pub const TRIGGER_SIZE: i32 = 28;
/// Gap between the trigger button and the main widget, design pixels at 96 DPI.
pub const TRIGGER_GAP: i32 = 6;
const TRIGGER_RADIUS: i32 = 6;

const PANEL_W: i32 = 900;
const PANEL_H: i32 = 600;
const CHROME_BUTTON_SIZE: i32 = 7;
const CHROME_BUTTON_MARGIN: i32 = 10;
const CHROME_BUTTON_GAP: i32 = 8;
const CHROME_STROKE: i32 = 2;

const CAPTION_STRIP: i32 = 26;
const PANEL_PAD_X: i32 = 12;
const PANEL_PAD_TOP: i32 = 30;
const PANEL_PAD_BOTTOM: i32 = 12;

const ORANGE_HEX: &str = "#D97757";
const CLAUDE_GREY_HEX: &str = "#262624";

const SHELL_CMD: &str = "cmd.exe";

pub const WM_APP_TERM_OUTPUT: u32 = WM_APP + 100;

static PANEL_HWND: Mutex<isize> = Mutex::new(0);
static SESSIONS: Mutex<Vec<SessionView>> = Mutex::new(Vec::new());
/// Index of the session that should receive keyboard input.
static FOCUSED_SESSION: Mutex<Option<usize>> = Mutex::new(None);
/// Index of the session that owns an in-progress mouse drag (selection).
static DRAGGING_SESSION: Mutex<Option<usize>> = Mutex::new(None);

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

pub fn open_panel() {
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

        // Initial session: a single full-area "claude" session.
        let mut session = SessionView::new(hwnd, WM_APP_TERM_OUTPUT, "claude", SHELL_CMD);
        let dpi = GetDpiForWindow(hwnd).max(96);
        let area = current_sessions_area(hwnd);
        session.set_bounds(area, dpi);
        {
            let mut sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
            sessions.push(session);
            *FOCUSED_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = Some(0);
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

/// Button visuals, ordered right-to-left as [close, maximize, minimize].
/// Each button is `CHROME_BUTTON_SIZE` square in design pixels — the small
/// glyph the user actually sees.
fn chrome_button_rects(client: &RECT, dpi: u32) -> [RECT; 3] {
    let scale = dpi as f64 / 96.0;
    let size = (CHROME_BUTTON_SIZE as f64 * scale).round() as i32;
    let margin = (CHROME_BUTTON_MARGIN as f64 * scale).round() as i32;
    let gap = (CHROME_BUTTON_GAP as f64 * scale).round() as i32;
    let mut right_edge = client.right - margin;
    let top = margin;
    let mut rects = [RECT::default(); 3];
    for r in rects.iter_mut() {
        *r = RECT {
            left: right_edge - size,
            top,
            right: right_edge,
            bottom: top + size,
        };
        right_edge -= size + gap;
    }
    rects
}

/// Hit-test rects — inflated versions of the visual rects so the clickable
/// area is comfortable. Adjacent hit rects abut (no gap, no overlap), which
/// makes the clicks forgiving without risking the wrong button.
fn chrome_button_hit_rects(client: &RECT, dpi: u32) -> [RECT; 3] {
    let scale = dpi as f64 / 96.0;
    let inflate = ((CHROME_BUTTON_GAP as f64 / 2.0) * scale).round() as i32;
    let mut rects = chrome_button_rects(client, dpi);
    for r in rects.iter_mut() {
        r.left -= inflate;
        r.top -= inflate;
        r.right += inflate;
        r.bottom += inflate;
    }
    rects
}

fn close_button_rect(client: &RECT, dpi: u32) -> RECT {
    chrome_button_rects(client, dpi)[0]
}

fn maximize_button_rect(client: &RECT, dpi: u32) -> RECT {
    chrome_button_rects(client, dpi)[1]
}

fn minimize_button_rect(client: &RECT, dpi: u32) -> RECT {
    chrome_button_rects(client, dpi)[2]
}

fn point_in(rect: &RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

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

/// The rect inside the panel client area that hosts session views — below
/// the caption / close button strip and inset for breathing room.
fn current_sessions_area(hwnd: HWND) -> RECT {
    unsafe {
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        let dpi = GetDpiForWindow(hwnd).max(96);
        let scale = dpi as f64 / 96.0;
        let pad_x = (PANEL_PAD_X as f64 * scale).round() as i32;
        let pad_top = (PANEL_PAD_TOP as f64 * scale).round() as i32;
        let pad_bot = (PANEL_PAD_BOTTOM as f64 * scale).round() as i32;
        RECT {
            left: client.left + pad_x,
            top: client.top + pad_top,
            right: client.right - pad_x,
            bottom: client.bottom - pad_bot,
        }
    }
}

fn relayout_sessions(hwnd: HWND) {
    let area = current_sessions_area(hwnd);
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    let mut sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    let count = sessions.len() as i32;
    if count == 0 {
        return;
    }
    // Layout: simple horizontal split. Multiple sessions share the area
    // equally side by side.
    let w_each = (area.right - area.left) / count;
    for (i, s) in sessions.iter_mut().enumerate() {
        let i = i as i32;
        let bounds = RECT {
            left: area.left + i * w_each,
            top: area.top,
            right: if i + 1 == count {
                area.right
            } else {
                area.left + (i + 1) * w_each
            },
            bottom: area.bottom,
        };
        s.set_bounds(bounds, dpi);
    }
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
            // The chrome buttons must take clicks themselves rather than
            // letting the caption strip swallow them as a drag. Use the
            // inflated hit rects so the clickable area is comfortable.
            for r in chrome_button_hit_rects(&client, dpi).iter() {
                if point_in(r, pt.x, pt.y) {
                    return LRESULT(HTCLIENT as isize);
                }
            }
            let cap = caption_strip(&client, dpi);
            if point_in(&cap, pt.x, pt.y) {
                return LRESULT(HTCAPTION as isize);
            }
            LRESULT(HTCLIENT as isize)
        }
        WM_SETCURSOR => {
            // Hand cursor when hovering any chrome button; otherwise let the
            // window class's IDC_IBEAM stand for the terminal area.
            let hit = (lparam.0 & 0xFFFF) as u16;
            if hit == HTCLIENT as u16 {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = ScreenToClient(hwnd, &mut pt);
                let mut client = RECT::default();
                let _ = GetClientRect(hwnd, &mut client);
                let dpi = GetDpiForWindow(hwnd).max(96);
                for r in chrome_button_hit_rects(&client, dpi).iter() {
                    if point_in(r, pt.x, pt.y) {
                        let cursor =
                            LoadCursorW(HINSTANCE::default(), IDC_HAND).unwrap_or_default();
                        SetCursor(cursor);
                        return LRESULT(1);
                    }
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            let _ = SetFocus(hwnd);
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut client = RECT::default();
            let _ = GetClientRect(hwnd, &mut client);
            let dpi = GetDpiForWindow(hwnd).max(96);
            let [close_hit, maximize_hit, minimize_hit] = chrome_button_hit_rects(&client, dpi);
            if point_in(&close_hit, x, y) {
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            if point_in(&maximize_hit, x, y) {
                let cmd = if IsZoomed(hwnd).as_bool() {
                    SW_RESTORE
                } else {
                    SW_MAXIMIZE
                };
                let _ = ShowWindow(hwnd, cmd);
                return LRESULT(0);
            }
            if point_in(&minimize_hit, x, y) {
                let _ = ShowWindow(hwnd, SW_MINIMIZE);
                return LRESULT(0);
            }
            // Route to whichever session contains the click.
            let sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
            for (i, s) in sessions.iter().enumerate() {
                if point_in(&s.bounds(), x, y) {
                    *FOCUSED_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = Some(i);
                    *DRAGGING_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = Some(i);
                    s.terminal().handle_mouse_down(x, y);
                    break;
                }
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let dragging = *DRAGGING_SESSION.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(idx) = dragging {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(s) = sessions.get(idx) {
                    s.terminal().handle_mouse_move(x, y);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let dragging = DRAGGING_SESSION
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(idx) = dragging {
                let sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(s) = sessions.get(idx) {
                    s.terminal().handle_mouse_up();
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

            let width = client.right - client.left;
            let height = client.bottom - client.top;
            let mem_dc = CreateCompatibleDC(hdc);
            let mem_bmp = CreateCompatibleBitmap(hdc, width, height);
            let old_bmp = SelectObject(mem_dc, mem_bmp);

            paint_panel_chrome_bg(mem_dc, &client);
            {
                let sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
                for s in sessions.iter() {
                    s.paint(mem_dc, dpi);
                }
            }
            paint_chrome_buttons(mem_dc, &client, dpi, hwnd);

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
            let focused = *FOCUSED_SESSION.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(idx) = focused {
                let sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(s) = sessions.get(idx) {
                    s.terminal().handle_char(code);
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let vk = wparam.0 as u32;
            let focused = *FOCUSED_SESSION.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(idx) = focused {
                let sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(s) = sessions.get(idx) {
                    if s.terminal().handle_key_down(vk) {
                        return LRESULT(0);
                    }
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        m if m == WM_APP_TERM_OUTPUT => {
            // A session emitted output; clear paint-pending on every session
            // (cheap if already false) and invalidate the panel.
            let sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
            for s in sessions.iter() {
                s.terminal().clear_paint_pending();
            }
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_SIZE => {
            relayout_sessions(hwnd);
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_DESTROY => {
            *PANEL_HWND.lock().unwrap_or_else(|e| e.into_inner()) = 0;
            SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).clear();
            *FOCUSED_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = None;
            *DRAGGING_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = None;
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn paint_panel_chrome_bg(hdc: HDC, client: &RECT) {
    let claude_grey = Color::from_hex(CLAUDE_GREY_HEX);
    unsafe {
        let bg_brush = CreateSolidBrush(COLORREF(claude_grey.to_colorref()));
        FillRect(hdc, client, bg_brush);
        let _ = DeleteObject(bg_brush);
    }
}

fn paint_chrome_buttons(hdc: HDC, client: &RECT, dpi: u32, hwnd: HWND) {
    let orange = Color::from_hex(ORANGE_HEX);
    let scale = dpi as f64 / 96.0;
    let pen_w = (CHROME_STROKE as f64 * scale).round().max(1.0) as i32;
    let close = close_button_rect(client, dpi);
    let maximize = maximize_button_rect(client, dpi);
    let minimize = minimize_button_rect(client, dpi);

    unsafe {
        let pen = CreatePen(PS_SOLID, pen_w, COLORREF(orange.to_colorref()));
        let old_pen = SelectObject(hdc, pen);
        let null_brush = GetStockObject(NULL_BRUSH);
        let old_brush = SelectObject(hdc, null_brush);

        // Close: orange "X".
        let _ = MoveToEx(hdc, close.left, close.top, None);
        let _ = LineTo(hdc, close.right, close.bottom);
        let _ = MoveToEx(hdc, close.right, close.top, None);
        let _ = LineTo(hdc, close.left, close.bottom);

        // Maximize / restore: when restored, draw a square outline. When the
        // window is maximized, draw two overlapping squares (the typical
        // "restore" affordance).
        if IsZoomed(hwnd).as_bool() {
            // Two overlapping squares offset by ~2 px each direction.
            let off = pen_w.max(2);
            let inner = RECT {
                left: maximize.left,
                top: maximize.top + off,
                right: maximize.right - off,
                bottom: maximize.bottom,
            };
            let outer = RECT {
                left: maximize.left + off,
                top: maximize.top,
                right: maximize.right,
                bottom: maximize.bottom - off,
            };
            let _ = Rectangle(hdc, inner.left, inner.top, inner.right, inner.bottom);
            let _ = Rectangle(hdc, outer.left, outer.top, outer.right, outer.bottom);
        } else {
            let _ = Rectangle(
                hdc,
                maximize.left,
                maximize.top,
                maximize.right,
                maximize.bottom,
            );
        }

        // Minimize: a horizontal line near the bottom of the cell.
        let _ = MoveToEx(hdc, minimize.left, minimize.bottom, None);
        let _ = LineTo(hdc, minimize.right, minimize.bottom);

        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(pen);
    }
}
