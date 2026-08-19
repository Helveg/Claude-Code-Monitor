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
    GetKeyState, ReleaseCapture, SetCapture, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_F1, VK_F2,
    VK_F3, VK_F4, VK_F5, VK_F6, VK_F7, VK_F8, VK_F9, VK_F10, VK_F11, VK_F12, VK_HOME, VK_INSERT,
    VK_LEFT, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

use crate::native_interop::{self, Color};
use crate::terminal::{Cell, Terminal};

pub const FONT_POINT_SIZE: i32 = 11;
/// Range the user can zoom the terminals through. The floor is where the
/// glyphs stop being legible; the ceiling is where a grid cell stops
/// holding enough columns for claude's layout to survive.
pub const MIN_FONT_POINT_SIZE: i32 = 7;
pub const MAX_FONT_POINT_SIZE: i32 = 24;

/// Hold a point size inside the range the terminals can render.
pub fn clamp_font_pt(point_size: i32) -> i32 {
    point_size.clamp(MIN_FONT_POINT_SIZE, MAX_FONT_POINT_SIZE)
}
const TERMINAL_PADDING: i32 = 8;
/// Scrollback lines moved per wheel notch, matching the Windows default of
/// three lines per detent.
const WHEEL_SCROLL_LINES: i32 = 3;
/// Scrollback bar down the right edge of the terminal, in design pixels.
/// The gutter is reserved whether or not there is history to show: it comes
/// out of the column count, and a PTY that gains and loses a column as
/// scrollback accumulates would have claude reflowing its whole layout.
const SCROLLBAR_W: i32 = 8;
const SCROLLBAR_THUMB_INSET: i32 = 2;
/// Floor on the thumb, so a long session still leaves something grabbable.
const SCROLLBAR_MIN_THUMB_H: i32 = 24;
const SCROLLBAR_TRACK_HEX: &str = "#1B1B19";
const SCROLLBAR_THUMB_HEX: &str = "#4A4A46";
const SCROLLBAR_THUMB_ACTIVE_HEX: &str = "#6C6C64";

const CLAUDE_GREY_HEX: &str = "#262624";
const DEFAULT_FG_HEX: &str = "#E8E8E8";

/// A range of text the user has selected. Rows are absolute line indices in
/// the grid's [`viewport_origin`](crate::terminal::Grid::viewport_origin)
/// space, not screen rows, so the highlight tracks its text as the viewport
/// scrolls and as output pushes lines up.
#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub anchor_row: u64,
    pub anchor_col: u16,
    pub head_row: u64,
    pub head_col: u16,
}

impl Selection {
    fn ordered(&self) -> ((u64, u16), (u64, u16)) {
        let a = (self.anchor_row, self.anchor_col);
        let h = (self.head_row, self.head_col);
        if a <= h {
            (a, h)
        } else {
            (h, a)
        }
    }

    fn is_empty(&self) -> bool {
        let (start, end) = self.ordered();
        start == end
    }

    fn contains(&self, row: u64, col: u16) -> bool {
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
    /// Selection highlight to apply to matching cells. Its rows are absolute
    /// line indices, which the render maps onto `rows` through the grid's
    /// viewport origin.
    pub selection: Option<Selection>,
    /// When true and the cursor's row is in the rendered range, draw the
    /// inverted-cell cursor.
    pub show_cursor: bool,
}

pub struct TerminalView {
    /// Pixel rect in the host window's client coords.
    bounds: RECT,
    /// Point size the grid is rendered at. The PTY's cols/rows are derived
    /// from this font's cell metrics, so it travels with the bounds: a
    /// terminal squeezed into a small tile asks for a smaller font and keeps
    /// a usable number of columns.
    font_pt: i32,
    terminal: Option<Terminal>,
    selection: Mutex<Option<Selection>>,
    mouse_dragging: AtomicBool,
    /// Set while the user is dragging the scrollbar thumb, holding the
    /// y-distance from the thumb's top to where they grabbed it. Lives here
    /// rather than on the panel so the existing press → move → release
    /// routing carries it without a second drag protocol.
    scroll_drag: Mutex<Option<i32>>,
    host_hwnd: HWND,
    notify_msg: u32,
    cmd: String,
    cwd: Option<std::path::PathBuf>,
    /// While set, `set_bounds` records geometry but does not spawn. A
    /// restored session sits like this until the user resumes it, so the
    /// manager can lay a session out — cell, nav row and all — without a
    /// process behind it.
    spawn_held: bool,
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
            font_pt: FONT_POINT_SIZE,
            terminal: None,
            selection: Mutex::new(None),
            mouse_dragging: AtomicBool::new(false),
            scroll_drag: Mutex::new(None),
            host_hwnd,
            notify_msg,
            cmd: cmd.into(),
            cwd,
            spawn_held: false,
        }
    }

    /// Same, but held: nothing is spawned until [`release_spawn`] is called.
    pub fn new_held(
        host_hwnd: HWND,
        notify_msg: u32,
        cmd: impl Into<String>,
        cwd: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            spawn_held: true,
            ..Self::new(host_hwnd, notify_msg, cmd, cwd)
        }
    }

    pub fn is_held(&self) -> bool {
        self.spawn_held
    }

    /// Let the PTY start. The next `set_bounds` spawns it, which the panel
    /// runs on the relayout that follows.
    pub fn release_spawn(&mut self) {
        self.spawn_held = false;
    }

    #[allow(dead_code)]
    pub fn bounds(&self) -> RECT {
        self.bounds
    }

    /// Move/resize the view in the host's client coords, rendering at
    /// `font_pt`. Spawns the PTY on the first call; subsequent calls resize
    /// the existing PTY.
    pub fn set_bounds(&mut self, bounds: RECT, dpi: u32, font_pt: i32) {
        self.bounds = bounds;
        self.font_pt = font_pt;
        if self.spawn_held {
            return;
        }
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
            font_pt: self.font_pt,
            rows: 0..grid.rows,
            selection,
            show_cursor: true,
        };
        render_grid_region(hdc, area, &grid, self.host_hwnd, &opts);
        // The metrics come off the same lock this paint already holds.
        drop(grid);
        self.paint_scrollbar(hdc, dpi);
    }

    /// Track plus thumb, in the gutter [`term_area`] reserved. Nothing is
    /// drawn while there's no history — an empty gutter reads as "you are
    /// at the start", which is exactly the case.
    fn paint_scrollbar(&self, hdc: HDC, dpi: u32) {
        let Some(thumb) = self.scrollbar_thumb(dpi) else {
            return;
        };
        let dragging = self
            .scroll_drag
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        let track = self.scrollbar_track(dpi);
        let radius = ((thumb.right - thumb.left) / 2).max(1);
        unsafe {
            let track_brush = CreateSolidBrush(COLORREF(
                Color::from_hex(SCROLLBAR_TRACK_HEX).to_colorref(),
            ));
            FillRect(hdc, &track, track_brush);
            let _ = DeleteObject(track_brush);

            let color = if dragging {
                Color::from_hex(SCROLLBAR_THUMB_ACTIVE_HEX)
            } else {
                Color::from_hex(SCROLLBAR_THUMB_HEX)
            };
            let thumb_brush = CreateSolidBrush(COLORREF(color.to_colorref()));
            let rgn = CreateRoundRectRgn(
                thumb.left,
                thumb.top,
                thumb.right + 1,
                thumb.bottom + 1,
                radius * 2,
                radius * 2,
            );
            let _ = FillRgn(hdc, rgn, thumb_brush);
            let _ = DeleteObject(rgn);
            let _ = DeleteObject(thumb_brush);
        }
    }

    pub fn handle_mouse_down(&self, x: i32, y: i32) {
        if self.press_scrollbar(x, y) {
            return;
        }
        if let Some((row, col)) = self.cell_at(x, y) {
            {
                let line = self.viewport_origin() + row as u64;
                let mut sel = self.selection.lock().unwrap_or_else(|e| e.into_inner());
                *sel = Some(Selection {
                    anchor_row: line,
                    anchor_col: col,
                    head_row: line,
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

    /// Take a press on the scrollbar: grab the thumb, or page towards a
    /// click on the track. Returns true when the press belonged to it.
    fn press_scrollbar(&self, x: i32, y: i32) -> bool {
        let dpi = unsafe { GetDpiForWindow(self.host_hwnd).max(96) };
        if !self.over_scrollbar(x, y, dpi) {
            return false;
        }
        let Some(thumb) = self.scrollbar_thumb(dpi) else {
            return false;
        };
        if y >= thumb.top && y < thumb.bottom {
            *self.scroll_drag.lock().unwrap_or_else(|e| e.into_inner()) = Some(y - thumb.top);
            unsafe {
                SetCapture(self.host_hwnd);
                let _ = InvalidateRect(self.host_hwnd, None, false);
            }
        } else {
            // Above the thumb is further back in history.
            let page = self.visible_rows() as i32;
            self.scroll_view_by(if y < thumb.top { page } else { -page });
        }
        true
    }

    pub fn handle_mouse_move(&self, x: i32, y: i32) {
        let grabbed = *self.scroll_drag.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(grab_offset) = grabbed {
            let dpi = unsafe { GetDpiForWindow(self.host_hwnd).max(96) };
            self.scroll_to_thumb(y, grab_offset, dpi);
            return;
        }
        if !self.mouse_dragging.load(Ordering::Acquire) {
            return;
        }
        self.drag_head_to(x, y);
    }

    /// Move the selection's head to the cell under `(x, y)` — clamped into
    /// the grid, so dragging past an edge keeps extending instead of
    /// freezing the selection where the pointer left the terminal.
    fn drag_head_to(&self, x: i32, y: i32) {
        let Some((row, col)) = self.clamped_cell_at(x, y) else {
            return;
        };
        let line = self.viewport_origin() + row as u64;
        let mut sel = self.selection.lock().unwrap_or_else(|e| e.into_inner());
        let Some(s) = sel.as_mut() else {
            return;
        };
        if s.head_row == line && s.head_col == col {
            return;
        }
        s.head_row = line;
        s.head_col = col;
        drop(sel);
        unsafe {
            let _ = InvalidateRect(self.host_hwnd, None, false);
        }
    }

    /// Re-anchor the selection head after the viewport moved under a
    /// stationary pointer: wheeling mid-drag reveals new lines, and the
    /// selection should swallow them the way it would if the pointer had
    /// travelled there.
    fn extend_drag_after_scroll(&self) {
        if !self.mouse_dragging.load(Ordering::Acquire) {
            return;
        }
        let mut pt = POINT::default();
        unsafe {
            if GetCursorPos(&mut pt).is_err() || !ScreenToClient(self.host_hwnd, &mut pt).as_bool() {
                return;
            }
        }
        self.drag_head_to(pt.x, pt.y);
    }

    pub fn handle_mouse_up(&self) {
        let was_scrolling = self
            .scroll_drag
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .is_some();
        if was_scrolling {
            unsafe {
                let _ = ReleaseCapture();
                let _ = InvalidateRect(self.host_hwnd, None, false);
            }
        }
        if self.mouse_dragging.swap(false, Ordering::AcqRel) {
            unsafe {
                let _ = ReleaseCapture();
            }
            // Collapse zero-area selections so they don't render.
            let mut sel = self.selection.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(s) = sel.as_ref() {
                if s.is_empty() {
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
        // CR / LF arrive here as the TranslateMessage echo of a VK_RETURN that
        // `encode_key` already sent — with the shift state intact, which this
        // message has lost. Dropping them keeps Enter from being sent twice.
        if code == 0x0D || code == 0x0A {
            return true;
        }

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
            self.paste_clipboard();
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
            self.snap_to_live();
            if let Some(t) = self.terminal.as_ref() {
                t.write_input(s.as_bytes());
            }
        }
        true
    }

    /// Send the clipboard's text to the PTY as a single paste.
    pub fn paste_clipboard(&self) {
        let Some(t) = self.terminal.as_ref() else {
            return;
        };
        let Some(text) = (unsafe { paste_clipboard_text(self.host_hwnd) }) else {
            return;
        };
        let bracketed = t
            .grid
            .lock()
            .map(|g| g.bracketed_paste)
            .unwrap_or(false);
        let bytes = encode_paste(&text, bracketed);
        crate::diagnose::log(format!(
            "paste: {} chars -> {} bytes, bracketed={bracketed}",
            text.chars().count(),
            bytes.len(),
        ));
        self.snap_to_live();
        t.write_input(&bytes);
    }

    /// Encode and forward a WM_KEYDOWN virtual-key. `alt_held` is true for
    /// WM_SYSKEYDOWN, and prefixes the sequence with ESC — the meta encoding
    /// every Unix TUI reads as "Alt was down". Returns true when the key was
    /// handled (a non-empty escape sequence was sent).
    pub fn handle_key_down(&self, vk: u32, alt_held: bool) -> bool {
        let shift_held = unsafe { (GetKeyState(VK_SHIFT.0 as i32) as i16) < 0 };
        // Shift+PageUp / Shift+PageDown page the scrollback, the way every
        // other terminal does — the unshifted keys still go to the TUI.
        if shift_held && (vk == VK_PRIOR.0 as u32 || vk == VK_NEXT.0 as u32) {
            let page = self.visible_rows().saturating_sub(1).max(1) as i32;
            let dir = if vk == VK_PRIOR.0 as u32 { page } else { -page };
            self.scroll_view_by(dir);
            return true;
        }
        let bytes = encode_key(vk, alt_held);
        if bytes.is_empty() {
            return false;
        }
        self.snap_to_live();
        if let Some(t) = self.terminal.as_ref() {
            t.write_input(&bytes);
        }
        true
    }

    /// Forward a WM_SYSCHAR — a printable character typed with Alt down —
    /// as ESC + the character. Windows routes these through the menu loop
    /// instead of WM_CHAR, so without this path every Alt+letter binding a
    /// TUI defines is unreachable.
    pub fn handle_alt_char(&self, code: u32) -> bool {
        let Some(ch) = char::from_u32(code) else {
            return false;
        };
        let mut bytes = vec![0x1b];
        let mut buf = [0u8; 4];
        bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        self.snap_to_live();
        if let Some(t) = self.terminal.as_ref() {
            t.write_input(&bytes);
        }
        true
    }

    /// Rows the live screen currently shows — the page size for keyboard
    /// scrollback paging.
    fn visible_rows(&self) -> u16 {
        self.terminal
            .as_ref()
            .and_then(|t| match t.grid.lock() {
                Ok(g) => Some(g.rows),
                Err(p) => Some(p.into_inner().rows),
            })
            .unwrap_or(1)
    }

    /// Handle a vertical scroll-wheel notch. `notches` is the WHEEL_DELTA-
    /// scaled count (positive = wheel-up, negative = wheel-down); `x` and `y`
    /// are in host client coords.
    ///
    /// In the alt screen a full-screen TUI owns the viewport, so the wheel is
    /// forwarded as an xterm mouse event when the TUI asked for mouse
    /// tracking. In the primary buffer the wheel pages our own scrollback —
    /// that's where a session's history lives, and forwarding there would
    /// hand the notch to an app that has nothing to scroll.
    ///
    /// Returns true if the event was consumed. We deliberately never
    /// synthesize arrow keys for wheel events: that's what Claude Code's
    /// `arrow-burst` detector warns about, and it's lossy compared to real
    /// mouse events.
    pub fn handle_wheel(&self, notches: i32, x: i32, y: i32) -> bool {
        if notches == 0 {
            return false;
        }
        let Some(terminal) = self.terminal.as_ref() else {
            return false;
        };
        let (protocol, encoding, is_alt) = {
            let g = match terminal.grid.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            (g.mouse_protocol, g.mouse_encoding, g.is_alt())
        };
        if !is_alt {
            return self.scroll_view_by(notches * WHEEL_SCROLL_LINES);
        }
        if protocol == crate::terminal::MouseProtocol::None {
            return false;
        }
        // Wheel-up is button 64; wheel-down is 65 in xterm's encoding.
        let button = if notches > 0 { 64u32 } else { 65u32 };
        // Cell coords are 1-based in both encodings. Outside the grid → no
        // event (cursor was over padding or the title bar).
        let (col, row) = match self.cell_at(x, y) {
            Some((r, c)) => (c as u32 + 1, r as u32 + 1),
            None => return false,
        };
        let one = encode_mouse_event(button, col, row, encoding);
        // Multiple notches in one Windows message → multiple events. Cap
        // generously so a runaway wheel can't flood the PTY but a fast
        // flick still scrolls a screenful.
        let repeats = notches.unsigned_abs().min(10);
        let mut out: Vec<u8> = Vec::with_capacity(one.len() * repeats as usize);
        for _ in 0..repeats {
            out.extend_from_slice(&one);
        }
        terminal.write_input(&out);
        true
    }

    /// Move the scrollback viewport by `lines` (positive = back in history)
    /// and repaint. Returns true when the viewport actually moved — false at
    /// either end, so the caller can let the wheel fall through.
    fn scroll_view_by(&self, lines: i32) -> bool {
        let Some(terminal) = self.terminal.as_ref() else {
            return false;
        };
        let moved = {
            let mut g = match terminal.grid.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            g.scroll_view(lines)
        };
        if moved {
            self.extend_drag_after_scroll();
            unsafe {
                let _ = InvalidateRect(self.host_hwnd, None, false);
            }
        }
        moved
    }

    /// Snap back to the live screen. Typing anywhere in history should put
    /// the user back where their keystrokes land.
    fn snap_to_live(&self) {
        let Some(terminal) = self.terminal.as_ref() else {
            return;
        };
        let moved = {
            let mut g = match terminal.grid.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            g.scroll_view_to_bottom()
        };
        if moved {
            unsafe {
                let _ = InvalidateRect(self.host_hwnd, None, false);
            }
        }
    }

    #[allow(dead_code)]
    pub fn drop_terminal(&mut self) {
        self.terminal = None;
    }

    /// Everything inside the view's padding — the text plus the scrollbar
    /// gutter.
    fn content_area(&self, dpi: u32) -> RECT {
        let scale = dpi as f64 / 96.0;
        let pad = (TERMINAL_PADDING as f64 * scale).round() as i32;
        RECT {
            left: self.bounds.left + pad,
            top: self.bounds.top + pad,
            right: self.bounds.right - pad,
            bottom: self.bounds.bottom - pad,
        }
    }

    /// Where the grid is drawn, and what the PTY's dimensions come from.
    fn term_area(&self, dpi: u32) -> RECT {
        let content = self.content_area(dpi);
        RECT {
            right: (content.right - scaled(SCROLLBAR_W, dpi)).max(content.left),
            ..content
        }
    }

    /// The scrollbar's track: the reserved gutter, full height.
    fn scrollbar_track(&self, dpi: u32) -> RECT {
        let content = self.content_area(dpi);
        RECT {
            left: self.term_area(dpi).right,
            ..content
        }
    }

    /// What the thumb is sized and placed from: `(history lines, rows on
    /// screen, lines the viewport is scrolled back by)`. `None` when there
    /// is nothing to scroll — no history yet, or a full-screen TUI owning
    /// the viewport, in which case the gutter stays empty.
    fn scroll_metrics(&self) -> Option<(usize, usize, usize)> {
        let terminal = self.terminal.as_ref()?;
        let g = match terminal.grid.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if g.is_alt() {
            return None;
        }
        let history = g.scrollback_len();
        if history == 0 {
            return None;
        }
        Some((history, g.rows as usize, g.view_offset()))
    }

    /// Thumb rect inside the track. `None` whenever [`scroll_metrics`] is.
    fn scrollbar_thumb(&self, dpi: u32) -> Option<RECT> {
        let (history, rows, offset) = self.scroll_metrics()?;
        let track = self.scrollbar_track(dpi);
        let track_h = (track.bottom - track.top).max(1);
        let min_thumb = scaled(SCROLLBAR_MIN_THUMB_H, dpi).min(track_h);
        let (top, thumb_h) = thumb_geometry(track_h, min_thumb, history, rows, offset);
        let inset = scaled(SCROLLBAR_THUMB_INSET, dpi);
        Some(RECT {
            left: track.left + inset,
            top: track.top + top,
            right: (track.right - inset).max(track.left + inset + 1),
            bottom: track.top + top + thumb_h,
        })
    }

    /// True if `(x, y)` is over the scrollbar — the caller uses it to show a
    /// hand rather than the text cursor.
    pub fn over_scrollbar(&self, x: i32, y: i32, dpi: u32) -> bool {
        self.scroll_metrics().is_some() && point_in(&self.scrollbar_track(dpi), x, y)
    }

    /// Put the viewport where a thumb dragged to `mouse_y` says it should
    /// be. Returns true when it moved.
    fn scroll_to_thumb(&self, mouse_y: i32, grab_offset: i32, dpi: u32) -> bool {
        let Some((history, _, offset)) = self.scroll_metrics() else {
            return false;
        };
        let Some(thumb) = self.scrollbar_thumb(dpi) else {
            return false;
        };
        let track = self.scrollbar_track(dpi);
        let travel = (track.bottom - track.top - (thumb.bottom - thumb.top)).max(1);
        let top = (mouse_y - grab_offset - track.top).clamp(0, travel);
        let target = offset_from_thumb_top(top, travel, history);
        self.scroll_view_by(target as i32 - offset as i32)
    }

    fn term_dimensions(&self, dpi: u32) -> Option<(u16, u16)> {
        unsafe {
            let area = self.term_area(dpi);
            let font = create_term_font(dpi, self.font_pt);
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
        let dpi = unsafe { GetDpiForWindow(self.host_hwnd).max(96) };
        if !point_in(&self.term_area(dpi), x, y) {
            return None;
        }
        self.clamped_cell_at(x, y)
    }

    /// Same mapping as [`cell_at`](Self::cell_at) but for points outside the
    /// terminal too: the cell is clamped to the grid's last row / column.
    fn clamped_cell_at(&self, x: i32, y: i32) -> Option<(u16, u16)> {
        unsafe {
            let dpi = GetDpiForWindow(self.host_hwnd).max(96);
            let area = self.term_area(dpi);
            let font = create_term_font(dpi, self.font_pt);
            if font.is_invalid() {
                return None;
            }
            let (cw, ch) = font_cell_metrics(self.host_hwnd, font);
            let _ = DeleteObject(font);
            if cw <= 0 || ch <= 0 {
                return None;
            }
            let (cols, rows) = self.grid_dimensions()?;
            let col = ((x - area.left) / cw).clamp(0, cols as i32 - 1) as u16;
            let row = ((y - area.top) / ch).clamp(0, rows as i32 - 1) as u16;
            Some((row, col))
        }
    }

    /// The live grid's `(cols, rows)`, or `None` before the PTY spawns.
    fn grid_dimensions(&self) -> Option<(u16, u16)> {
        let terminal = self.terminal.as_ref()?;
        let g = match terminal.grid.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        Some((g.cols.max(1), g.rows.max(1)))
    }

    /// Absolute line index of the row at the top of the viewport. Selections
    /// are stored against it, so a scroll changes the origin and leaves the
    /// selected text alone. 0 before the PTY spawns.
    fn viewport_origin(&self) -> u64 {
        self.terminal
            .as_ref()
            .map(|t| match t.grid.lock() {
                Ok(g) => g.viewport_origin(),
                Err(p) => p.into_inner().viewport_origin(),
            })
            .unwrap_or(0)
    }
}

fn point_in(rect: &RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

fn scaled(design_px: i32, dpi: u32) -> i32 {
    (design_px as f64 * dpi as f64 / 96.0).round() as i32
}

/// Thumb `(top, height)` within a track of `track_h` pixels, for a session
/// with `history` lines above a screen of `rows`, scrolled back by
/// `offset` lines.
///
/// The scrollbar spans history *plus* the live screen, so the thumb is the
/// screen's share of the whole conversation. `offset` counts backwards from
/// the live screen, so it runs the opposite way to the thumb: caught up
/// (`offset == 0`) puts the thumb at the bottom.
fn thumb_geometry(
    track_h: i32,
    min_thumb: i32,
    history: usize,
    rows: usize,
    offset: usize,
) -> (i32, i32) {
    let total = (history + rows).max(1);
    let thumb_h = ((track_h as i64 * rows as i64 / total as i64) as i32)
        .max(min_thumb.min(track_h))
        .min(track_h);
    let travel = track_h - thumb_h;
    if history == 0 || travel <= 0 {
        return (travel.max(0), thumb_h);
    }
    let scrolled_from_top = history - offset.min(history);
    // Rounded, not truncated, in both directions: the drag converts back
    // through `offset_from_thumb_top`, and two floors compound into a thumb
    // that creeps away from the cursor.
    let top = div_round(travel as i64 * scrolled_from_top as i64, history as i64) as i32;
    (top.clamp(0, travel), thumb_h)
}

/// The inverse: view offset for a thumb dragged to `top` within `travel`
/// pixels of room.
fn offset_from_thumb_top(top: i32, travel: i32, history: usize) -> usize {
    if travel <= 0 {
        return 0;
    }
    let top = top.clamp(0, travel) as i64;
    let scrolled_from_top = div_round(top * history as i64, travel as i64) as usize;
    history - scrolled_from_top.min(history)
}

/// Integer divide, rounding to nearest. Both operands are non-negative here.
fn div_round(numerator: i64, denominator: i64) -> i64 {
    if denominator == 0 {
        return 0;
    }
    (numerator + denominator / 2) / denominator
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
    let mut fonts = FontSet::new(opts.dpi, opts.font_pt);
    // The plain face stays selected through the glyph-coverage probe below:
    // GetGlyphIndicesW reports on whatever font the DC holds, and the
    // main-vs-symbol decision has to be the unstyled face's.
    let main_font = unsafe { fonts.get(GlyphStyle::PLAIN) };
    let old_font = unsafe { SelectObject(hdc, main_font) };
    let (cell_w, cell_h) = unsafe { font_cell_metrics(host_hwnd, main_font) };
    if cell_w <= 0 || cell_h <= 0 {
        unsafe {
            SelectObject(hdc, old_font);
            fonts.destroy();
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
    let origin = grid.viewport_origin();
    let cell_render_colors = |cell: &Cell, row: u16, col: u16| -> (Color, Color) {
        let (fg, bg) = cell_colors(cell);
        if selection
            .map(|s| s.contains(origin + row as u64, col))
            .unwrap_or(false)
        {
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
            let cell = grid.display_cell(row, col);
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
            let cell = grid.display_cell(row, col);
            let (_, bg) = cell_render_colors(&cell, row, col);
            let mut run_end = col + 1;
            while run_end < visible_cols {
                let next = grid.display_cell(row, run_end);
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

    // Pass 2: glyphs — group runs by (fg, style).
    for row in row_start..row_end {
        let y = pixel_y(row);
        let mut col = 0u16;
        while col < visible_cols {
            let cell = grid.display_cell(row, col);
            let (fg, _) = cell_render_colors(&cell, row, col);
            let style = GlyphStyle::of(&cell, needs_symbol(row, col));
            let mut text_buf: Vec<u16> = Vec::new();
            let mut dx_buf: Vec<i32> = Vec::new();
            let mut run_end = col;
            while run_end < visible_cols {
                let c = grid.display_cell(row, run_end);
                let (next_fg, _) = cell_render_colors(&c, row, run_end);
                if next_fg.r != fg.r || next_fg.g != fg.g || next_fg.b != fg.b {
                    break;
                }
                if GlyphStyle::of(&c, needs_symbol(row, run_end)) != style {
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
                    let font = fonts.get(style);
                    SelectObject(hdc, font);
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
    // Scrolled back into history there is no live cursor on screen to draw.
    if opts.show_cursor
        && grid.view_offset() == 0
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
        unsafe {
            let cursor_font = fonts.get(GlyphStyle::of(
                &cell,
                needs_symbol(grid.cursor_row, grid.cursor_col),
            ));
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
        fonts.destroy();
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
    let (fg, bg) = if cell.attrs.reverse { (bg, fg) } else { (fg, bg) };
    // Faint has no GDI equivalent, so the drop in intensity has to be baked
    // into the color: pull the foreground halfway to whatever it sits on.
    if cell.attrs.dim {
        (blend(fg, bg, 50), bg)
    } else {
        (fg, bg)
    }
}

/// `pct` percent of the way from `from` to `to`.
fn blend(from: Color, to: Color, pct: u32) -> Color {
    let mix = |a: u8, b: u8| -> u8 {
        ((a as u32 * (100 - pct) + b as u32 * pct) / 100).min(255) as u8
    };
    Color::new(mix(from.r, to.r), mix(from.g, to.g), mix(from.b, to.b))
}

/// Everything about a cell that picks a font: the SGR glyph attributes plus
/// the main/symbol face decision. Runs of glyphs are batched per style, so
/// this doubles as half the run key in pass 2.
#[derive(Clone, Copy, PartialEq, Eq)]
struct GlyphStyle {
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
    /// The glyph is missing from the monospace face and falls back to the
    /// symbol font.
    symbol: bool,
}

impl GlyphStyle {
    const PLAIN: Self = Self {
        bold: false,
        italic: false,
        underline: false,
        strikethrough: false,
        symbol: false,
    };

    fn of(cell: &Cell, symbol: bool) -> Self {
        Self {
            bold: cell.attrs.bold,
            italic: cell.attrs.italic,
            underline: cell.attrs.underline,
            strikethrough: cell.attrs.strikethrough,
            symbol,
        }
    }
}

/// GDI fonts for the styles one render actually touches, built on demand.
/// There are 32 possible combinations but a screen normally uses two or
/// three, and creating them all up front would cost more than the paint.
struct FontSet {
    dpi: u32,
    font_pt: i32,
    entries: Vec<(GlyphStyle, HFONT)>,
}

impl FontSet {
    fn new(dpi: u32, font_pt: i32) -> Self {
        Self {
            dpi,
            font_pt,
            entries: Vec::new(),
        }
    }

    unsafe fn get(&mut self, style: GlyphStyle) -> HFONT {
        if let Some(&(_, f)) = self.entries.iter().find(|(s, _)| *s == style) {
            return f;
        }
        let f = create_styled_font(self.dpi, self.font_pt, style);
        self.entries.push((style, f));
        f
    }

    /// Must be called with none of these fonts selected into a DC.
    unsafe fn destroy(self) {
        for (_, f) in self.entries {
            let _ = DeleteObject(f);
        }
    }
}

unsafe fn create_term_font(dpi: u32, font_pt: i32) -> HFONT {
    create_styled_font(dpi, font_pt, GlyphStyle::PLAIN)
}

unsafe fn create_styled_font(dpi: u32, font_pt: i32, style: GlyphStyle) -> HFONT {
    let height = -(font_pt * dpi as i32 / 72);
    let weight = if style.bold { FW_BOLD } else { FW_NORMAL }.0 as i32;
    let italic = style.italic as u32;
    let underline = style.underline as u32;
    let strikeout = style.strikethrough as u32;
    let (candidates, pitch): (&[&str], u32) = if style.symbol {
        (&["Segoe UI Symbol"], (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32)
    } else {
        (
            &["Cascadia Code", "Cascadia Mono", "Consolas", "Courier New"],
            (FIXED_PITCH.0 | FF_MODERN.0) as u32,
        )
    };
    for name in candidates {
        let face = native_interop::wide_str(name);
        let f = CreateFontW(
            height,
            0,
            0,
            0,
            weight,
            italic,
            underline,
            strikeout,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            pitch,
            PCWSTR::from_raw(face.as_ptr()),
        );
        if !f.is_invalid() {
            return f;
        }
    }
    HFONT::default()
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

/// Text of `sel`, read straight out of the absolute line space — so a
/// selection dragged across a scroll copies every line it covers, including
/// the ones no longer on screen.
fn selection_text(grid: &crate::terminal::Grid, sel: &Selection) -> String {
    let ((srow, scol), (erow, ecol)) = sel.ordered();
    let mut out = String::new();
    let cols = grid.cols;
    for row in srow..=erow {
        let start_col = if row == srow { scol } else { 0 };
        let end_col = if row == erow { ecol } else { cols };
        let mut line = String::new();
        for col in start_col..end_col {
            line.push(grid.abs_cell(row, col).ch);
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

/// Turn clipboard text into the byte stream a TUI expects from a paste.
///
/// * Every newline flavour collapses to a bare CR — the wire form of Enter.
///   Passing the clipboard's CRLF through would make the LF land as an extra
///   line feed on top of the CR.
/// * Control characters other than tab are dropped, so clipboard content can
///   never inject escape sequences — including an `ESC[201~` that would close
///   the bracket early and let the remainder run as live keystrokes.
/// * With ?2004 active the whole block is framed by `ESC[200~` / `ESC[201~`.
///   That framing is what lets the TUI treat the text as one paste; without
///   it a line-editor reads each CR as a submit and splits the block.
fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 12);
    if bracketed {
        out.extend_from_slice(b"\x1b[200~");
    }
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(b'\r');
            }
            '\n' => out.push(b'\r'),
            '\t' => out.push(b'\t'),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {}
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    if bracketed {
        out.extend_from_slice(b"\x1b[201~");
    }
    out
}

/// Encode an xterm mouse event for the current encoding. `button` is the
/// raw button code (0/1/2 for press, 3 for release in legacy; 64/65 for
/// wheel-up/down; +32 for motion). `col` and `row` are 1-based cell coords.
fn encode_mouse_event(
    button: u32,
    col: u32,
    row: u32,
    encoding: crate::terminal::MouseEncoding,
) -> Vec<u8> {
    use crate::terminal::MouseEncoding;
    match encoding {
        MouseEncoding::Sgr => {
            // SGR: ESC [ < b ; x ; y M  (press / wheel) or m (release).
            // Wheel events are conventionally framed with `M` regardless of
            // direction — there's no separate release.
            format!("\x1b[<{button};{col};{row}M").into_bytes()
        }
        MouseEncoding::Legacy => {
            // Legacy X10: ESC [ M <b+32> <col+32> <row+32>.
            // Bytes can be non-printable but must fit in u8 — cap fields at
            // 223 (the encodable max). TUIs that asked for legacy already
            // accept this lossy clamp.
            let b = (button.min(223) + 32) as u8;
            let c = (col.min(223) + 32) as u8;
            let r = (row.min(223) + 32) as u8;
            vec![0x1b, b'[', b'M', b, c, r]
        }
    }
}

/// A key that has an xterm CSI encoding, in the shape its *unmodified* form
/// takes. With modifiers held all three collapse to the same parameterized
/// `ESC [ <param> ; <modifier> <final>` family.
enum CsiKey {
    /// `ESC [ <final>` — cursor keys, Home/End.
    Csi(u8),
    /// `ESC O <final>` — the SS3 keys, i.e. F1–F4.
    Ss3(u8),
    /// `ESC [ <n> ~` — the numbered "tilde" keys.
    Tilde(u8),
}

fn csi_key(vk: u32) -> Option<CsiKey> {
    let key = match vk {
        v if v == VK_UP.0 as u32 => CsiKey::Csi(b'A'),
        v if v == VK_DOWN.0 as u32 => CsiKey::Csi(b'B'),
        v if v == VK_RIGHT.0 as u32 => CsiKey::Csi(b'C'),
        v if v == VK_LEFT.0 as u32 => CsiKey::Csi(b'D'),
        v if v == VK_END.0 as u32 => CsiKey::Csi(b'F'),
        v if v == VK_HOME.0 as u32 => CsiKey::Csi(b'H'),
        v if v == VK_F1.0 as u32 => CsiKey::Ss3(b'P'),
        v if v == VK_F2.0 as u32 => CsiKey::Ss3(b'Q'),
        v if v == VK_F3.0 as u32 => CsiKey::Ss3(b'R'),
        v if v == VK_F4.0 as u32 => CsiKey::Ss3(b'S'),
        v if v == VK_INSERT.0 as u32 => CsiKey::Tilde(2),
        v if v == VK_DELETE.0 as u32 => CsiKey::Tilde(3),
        v if v == VK_PRIOR.0 as u32 => CsiKey::Tilde(5),
        v if v == VK_NEXT.0 as u32 => CsiKey::Tilde(6),
        v if v == VK_F5.0 as u32 => CsiKey::Tilde(15),
        v if v == VK_F6.0 as u32 => CsiKey::Tilde(17),
        v if v == VK_F7.0 as u32 => CsiKey::Tilde(18),
        v if v == VK_F8.0 as u32 => CsiKey::Tilde(19),
        v if v == VK_F9.0 as u32 => CsiKey::Tilde(20),
        v if v == VK_F10.0 as u32 => CsiKey::Tilde(21),
        v if v == VK_F11.0 as u32 => CsiKey::Tilde(23),
        v if v == VK_F12.0 as u32 => CsiKey::Tilde(24),
        _ => return None,
    };
    Some(key)
}

/// xterm's modifier parameter: 1 plus a bitmask of shift(1) / alt(2) /
/// ctrl(4). A value of 1 means "nothing held", which is written as the
/// short legacy sequence instead of a parameterized one.
fn modifier_param(shift: bool, alt: bool, ctrl: bool) -> u8 {
    1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8
}

fn encode_csi(key: CsiKey, modifier: u8) -> Vec<u8> {
    if modifier <= 1 {
        return match key {
            CsiKey::Csi(f) => vec![0x1b, b'[', f],
            CsiKey::Ss3(f) => vec![0x1b, b'O', f],
            CsiKey::Tilde(n) => format!("\x1b[{n}~").into_bytes(),
        };
    }
    match key {
        // Both families take `1` as the leading parameter once a modifier
        // is present — that's what turns SS3 into a CSI sequence for F1–F4.
        CsiKey::Csi(f) | CsiKey::Ss3(f) => {
            format!("\x1b[1;{modifier}{}", f as char).into_bytes()
        }
        CsiKey::Tilde(n) => format!("\x1b[{n};{modifier}~").into_bytes(),
    }
}

/// Encode keystrokes that don't have a printable WM_CHAR equivalent. Tab,
/// Escape, and Backspace are intentionally excluded — TranslateMessage posts
/// a WM_CHAR for them, which the host routes through `handle_char`.
/// Shift+Tab is the exception: TranslateMessage skips it.
///
/// Enter is handled here rather than via WM_CHAR because the shift state has
/// to survive: TranslateMessage flattens both Enter and Shift+Enter to the
/// same `WM_CHAR 0x0D`, so a WM_CHAR-only path cannot tell them apart.
/// `handle_char` drops 0x0D / 0x0A to keep this the only sender.
///
/// Ctrl / Shift / Alt ride along in the CSI modifier parameter, which is how
/// `Ctrl+Left` reaches a TUI as "previous word" rather than as a bare arrow.
/// `alt_held` comes from the WM_SYSKEYDOWN/WM_KEYDOWN split; the other two
/// are read off the keyboard state.
fn encode_key(vk: u32, alt_held: bool) -> Vec<u8> {
    let shift_held = unsafe { (GetKeyState(VK_SHIFT.0 as i32) as i16) < 0 };
    let ctrl_held = unsafe { (GetKeyState(VK_CONTROL.0 as i32) as i16) < 0 };

    if let Some(key) = csi_key(vk) {
        return encode_csi(key, modifier_param(shift_held, alt_held, ctrl_held));
    }

    // Keys with no CSI form. Alt is the classic ESC prefix here.
    let mut bytes = match vk {
        // Shift+Enter → ESC CR, the meta-Return encoding Claude Code and
        // other readline-style TUIs read as "insert a newline, don't submit".
        v if v == VK_RETURN.0 as u32 && shift_held => b"\x1b\r".to_vec(),
        v if v == VK_RETURN.0 as u32 => b"\r".to_vec(),
        v if v == VK_TAB.0 as u32 && shift_held => b"\x1b[Z".to_vec(),
        _ => return Vec::new(),
    };
    if alt_held {
        bytes.insert(0, 0x1b);
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::MouseEncoding;

    const TRACK_H: i32 = 200;
    const MIN_THUMB: i32 = 24;

    /// Caught up puts the thumb at the bottom, the oldest line at the top,
    /// and the thumb is the screen's share of the whole conversation.
    #[test]
    fn the_thumb_sits_where_the_viewport_is() {
        let (top, h) = thumb_geometry(TRACK_H, MIN_THUMB, 100, 20, 0);
        assert_eq!(h, TRACK_H * 20 / 120);
        assert_eq!(top + h, TRACK_H, "live screen should pin it to the bottom");

        let (top, _) = thumb_geometry(TRACK_H, MIN_THUMB, 100, 20, 100);
        assert_eq!(top, 0, "fully scrolled back should pin it to the top");

        let (half, h) = thumb_geometry(TRACK_H, MIN_THUMB, 100, 20, 50);
        assert!(
            half.abs_diff((TRACK_H - h) / 2) <= 1,
            "halfway back should sit mid-track, got {half}"
        );
    }

    /// A multi-line paste has to reach the TUI as one bracketed block with
    /// bare CRs — CRLF pairs would each land as an extra line feed, and
    /// without the brackets a line editor submits at the first newline.
    #[test]
    fn paste_is_bracketed_and_newlines_collapse_to_cr() {
        let out = encode_paste("one\r\ntwo\nthree", true);
        assert_eq!(out, b"\x1b[200~one\rtwo\rthree\x1b[201~");

        // No ?2004 from the TUI: same text, no framing.
        let out = encode_paste("one\r\ntwo", false);
        assert_eq!(out, b"one\rtwo");
    }

    /// Clipboard text is untrusted input: an embedded end-marker must not be
    /// able to close the bracket early and run the tail as live keystrokes.
    #[test]
    fn paste_strips_control_sequences_but_keeps_tabs() {
        let out = encode_paste("a\x1b[201~b\x00c\td", true);
        assert_eq!(out, b"\x1b[200~a[201~bc\td\x1b[201~");
    }

    /// A very long session still leaves something to grab.
    #[test]
    fn the_thumb_never_shrinks_past_its_floor() {
        let (_, h) = thumb_geometry(TRACK_H, MIN_THUMB, 100_000, 20, 0);
        assert_eq!(h, MIN_THUMB);
    }

    /// Dragging and drawing have to agree, or the thumb runs away from the
    /// cursor.
    #[test]
    fn dragging_the_thumb_round_trips_to_the_same_offset() {
        let history = 500;
        let rows = 30;
        let (_, thumb_h) = thumb_geometry(TRACK_H, MIN_THUMB, history, rows, 0);
        let travel = TRACK_H - thumb_h;
        for offset in [0, 1, 137, 499, 500] {
            let (top, _) = thumb_geometry(TRACK_H, MIN_THUMB, history, rows, offset);
            let back = offset_from_thumb_top(top, travel, history);
            // One pixel of travel is worth several lines; the round trip may
            // not cost more than that.
            let slack = history.div_ceil(travel as usize);
            assert!(
                back.abs_diff(offset) <= slack,
                "offset {offset} came back as {back}"
            );
        }
    }

    #[test]
    fn a_session_with_no_history_has_a_full_track_thumb() {
        let (top, h) = thumb_geometry(TRACK_H, MIN_THUMB, 0, 30, 0);
        assert_eq!((top, h), (0, TRACK_H));
        assert_eq!(offset_from_thumb_top(0, 0, 0), 0);
    }

    #[test]
    fn sgr_wheel_up_matches_xterm() {
        // Wheel-up is button 64 with `M` as the final byte regardless
        // of direction. Coords are 1-based. Claude Code's mouse decoder
        // expects exactly this framing — any drift here and scroll-in-
        // alt-screen breaks silently.
        let bytes = encode_mouse_event(64, 5, 12, MouseEncoding::Sgr);
        assert_eq!(bytes, b"\x1b[<64;5;12M");
    }

    #[test]
    fn sgr_wheel_down_matches_xterm() {
        let bytes = encode_mouse_event(65, 1, 1, MouseEncoding::Sgr);
        assert_eq!(bytes, b"\x1b[<65;1;1M");
    }

    #[test]
    fn unmodified_keys_keep_their_short_encoding() {
        assert_eq!(encode_csi(CsiKey::Csi(b'D'), 1), b"\x1b[D");
        assert_eq!(encode_csi(CsiKey::Ss3(b'P'), 1), b"\x1bOP");
        assert_eq!(encode_csi(CsiKey::Tilde(15), 1), b"\x1b[15~");
    }

    #[test]
    fn ctrl_arrows_carry_the_word_nav_modifier() {
        // 5 = 1 + ctrl(4). This is the sequence readline-style TUIs read as
        // "move a whole word".
        let ctrl = modifier_param(false, false, true);
        assert_eq!(ctrl, 5);
        assert_eq!(encode_csi(CsiKey::Csi(b'D'), ctrl), b"\x1b[1;5D");
        assert_eq!(encode_csi(CsiKey::Csi(b'C'), ctrl), b"\x1b[1;5C");
    }

    #[test]
    fn modifiers_combine_into_one_parameter() {
        // shift(1) + alt(2) + ctrl(4) on top of the base 1.
        assert_eq!(modifier_param(true, true, true), 8);
        assert_eq!(encode_csi(CsiKey::Tilde(3), 5), b"\x1b[3;5~");
        // F1 with a modifier drops SS3 for the parameterized CSI form.
        assert_eq!(encode_csi(CsiKey::Ss3(b'P'), 3), b"\x1b[1;3P");
    }

    #[test]
    fn legacy_encoding_offsets_by_32() {
        // X10 framing: ESC [ M then three bytes (button, col, row) each
        // shifted by 32. TUIs predating SGR rely on this exact framing.
        let bytes = encode_mouse_event(0, 5, 7, MouseEncoding::Legacy);
        assert_eq!(bytes, &[0x1b, b'[', b'M', 32, 37, 39]);
    }
}
