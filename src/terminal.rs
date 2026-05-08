//! ConPTY-based terminal emulator. Owns a child process attached to a Windows
//! pseudo-console, reads its output on a worker thread, parses a subset of
//! VT/ANSI sequences into a cell grid, and exposes input/resize for the UI.

use std::ffi::c_void;
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows::Win32::System::Console::*;
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::diagnose;

/// PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE value (Windows 10+).
const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;

// ---------------------------------------------------------------------------
// Color and attribute model.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // `Default` is reserved for future use; reset attrs use `None` instead.
pub enum AnsiColor {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl AnsiColor {
    pub fn to_rgb(self, default: (u8, u8, u8)) -> (u8, u8, u8) {
        match self {
            AnsiColor::Default => default,
            AnsiColor::Rgb(r, g, b) => (r, g, b),
            AnsiColor::Indexed(i) => palette_256(i),
        }
    }
}

/// 256-color xterm palette. 0..15 are the standard ANSI colors, 16..231 form
/// a 6×6×6 cube, 232..255 are 24 grey levels.
fn palette_256(i: u8) -> (u8, u8, u8) {
    const BASIC: [(u8, u8, u8); 16] = [
        (0x1c, 0x1c, 0x1c), // black (use bg-ish dark grey)
        (0xcc, 0x55, 0x55), // red
        (0x55, 0xaa, 0x55), // green
        (0xcc, 0xaa, 0x55), // yellow
        (0x55, 0x88, 0xcc), // blue
        (0xaa, 0x66, 0xcc), // magenta
        (0x55, 0xaa, 0xaa), // cyan
        (0xcc, 0xcc, 0xcc), // white
        (0x66, 0x66, 0x66), // bright black
        (0xff, 0x77, 0x77), // bright red
        (0x77, 0xdd, 0x77), // bright green
        (0xff, 0xdd, 0x77), // bright yellow
        (0x77, 0xaa, 0xff), // bright blue
        (0xdd, 0x88, 0xff), // bright magenta
        (0x77, 0xdd, 0xdd), // bright cyan
        (0xff, 0xff, 0xff), // bright white
    ];
    if (i as usize) < BASIC.len() {
        return BASIC[i as usize];
    }
    if i >= 232 {
        let v = 8 + (i - 232) * 10;
        return (v, v, v);
    }
    let i = i - 16;
    let r = i / 36;
    let g = (i / 6) % 6;
    let b = i % 6;
    let scale = |c: u8| -> u8 {
        if c == 0 {
            0
        } else {
            55 + c * 40
        }
    };
    (scale(r), scale(g), scale(b))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellAttrs {
    pub fg: Option<AnsiColor>,
    pub bg: Option<AnsiColor>,
    pub bold: bool,
    pub underline: bool,
    pub reverse: bool,
}

impl CellAttrs {
    fn reset(&mut self) {
        *self = CellAttrs::default();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub attrs: CellAttrs,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            attrs: CellAttrs::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Grid (screen buffer).
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Grid {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Cell>,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_visible: bool,
    pub scroll_top: u16,
    pub scroll_bottom: u16,
    pub current_attrs: CellAttrs,
    saved_cursor: Option<(u16, u16, CellAttrs)>,
    /// VT100 deferred-wrap flag. After writing to the last column we leave the
    /// cursor on that column with `pending_wrap = true`; the wrap actually
    /// happens when the next printable character arrives. CR/LF/cursor-move
    /// commands cancel the pending wrap. This is what prevents a full-width
    /// row immediately followed by `\r\n` from advancing two rows.
    pending_wrap: bool,
    /// Position of the last "naked reverse-video space" cell — the standard
    /// pattern TUIs use to draw their own block cursor (`ESC[7m` + ` ` +
    /// `ESC[27m`). When a new such cell is written far away from the previous
    /// one, the previous one is almost certainly a stale cursor visual the
    /// TUI forgot to clean up; we clear it ourselves.
    last_cursor_glyph_cell: Option<(u16, u16)>,
    /// Set to true whenever any cell changes value. The worker thread reads
    /// + clears this after each parse iteration to drive the per-session
    /// "last meaningful output" timestamp — so cursor blinks / pings that
    /// don't actually change the screen don't keep the status green.
    content_dirty: bool,
}

/// Replace `cells[start..end]` with `blank`, returning whether any cell
/// actually differed before the write. Used by erase / scroll operations
/// to avoid spurious `content_dirty` bumps when the wiped region was
/// already blank — that was making claude code's idle "redraw the input
/// chrome every second" loop look like real activity.
fn blank_range(cells: &mut [Cell], start: usize, end: usize, blank: Cell) -> bool {
    let mut changed = false;
    for c in &mut cells[start..end] {
        if *c != blank {
            *c = blank;
            changed = true;
        }
    }
    changed
}

impl Grid {
    fn new(cols: u16, rows: u16) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            cells: vec![Cell::default(); cols as usize * rows as usize],
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            current_attrs: CellAttrs::default(),
            saved_cursor: None,
            pending_wrap: false,
            last_cursor_glyph_cell: None,
            content_dirty: false,
        }
    }

    /// Read and clear the dirty flag. Called by the worker after each parse
    /// iteration: returns true iff at least one cell value actually changed.
    pub fn take_content_dirty(&mut self) -> bool {
        let v = self.content_dirty;
        self.content_dirty = false;
        v
    }

    fn idx(&self, row: u16, col: u16) -> usize {
        row as usize * self.cols as usize + col as usize
    }

    pub fn cell(&self, row: u16, col: u16) -> Cell {
        if row >= self.rows || col >= self.cols {
            return Cell::default();
        }
        self.cells[self.idx(row, col)]
    }

    fn set_cell(&mut self, row: u16, col: u16, cell: Cell) {
        if row >= self.rows || col >= self.cols {
            return;
        }
        let i = self.idx(row, col);
        if self.cells[i] != cell {
            self.cells[i] = cell;
            self.content_dirty = true;
        }
    }

    fn put_char(&mut self, ch: char) {
        let attrs = self.current_attrs;

        // If a previous put_char hit the last column it left the cursor on
        // that column with `pending_wrap = true`. The wrap is committed only
        // when *another* character actually arrives, which is exactly when
        // `\r\n` or absolute cursor moves can still cancel it.
        if self.pending_wrap {
            self.pending_wrap = false;
            self.cursor_col = 0;
            self.cursor_row_advance();
        }
        if self.cursor_col >= self.cols {
            self.cursor_col = self.cols - 1;
        }
        let row = self.cursor_row;
        let col = self.cursor_col;

        // A "naked reverse-video space" — `ESC[7m` then ` ` then `ESC[27m`
        // with no other SGR — is the canonical TUI pattern for drawing a
        // block cursor when the application can't trust the terminal's own
        // cursor. claude code uses it heavily and hides the native cursor
        // with `ESC[?25l`, so we keep storing the cell (otherwise no cursor
        // visual at all). The catch: when the TUI moves on, it doesn't
        // always overwrite the previous cursor cell, leaving white blocks
        // behind. We track where the last such glyph went and, when a new
        // one arrives somewhere non-adjacent, clear the old slot.
        //
        // The "non-adjacent" check is what keeps legitimate inverse-video
        // bars (status lines, vim selections on empty lines) intact — those
        // emit consecutive reverse-spaces along the same row.
        let is_cursor_glyph = ch == ' '
            && attrs.reverse
            && attrs.fg.is_none()
            && attrs.bg.is_none()
            && !attrs.bold
            && !attrs.underline;
        if is_cursor_glyph {
            if let Some((pr, pc)) = self.last_cursor_glyph_cell {
                let adjacent = pr == row && pc.abs_diff(col) <= 1;
                if !adjacent && pr < self.rows && pc < self.cols {
                    let prev = self.cells[self.idx(pr, pc)];
                    let prev_is_glyph = prev.ch == ' '
                        && prev.attrs.reverse
                        && prev.attrs.fg.is_none()
                        && prev.attrs.bg.is_none()
                        && !prev.attrs.bold
                        && !prev.attrs.underline;
                    if prev_is_glyph {
                        let i = self.idx(pr, pc);
                        self.cells[i] = Cell::default();
                    }
                }
            }
            self.last_cursor_glyph_cell = Some((row, col));
        }

        if diagnose::is_enabled() && (attrs.reverse || attrs.bg.is_some()) {
            diagnose::log(format!(
                "term: write {:?} at ({}, {}) reverse={} bg={:?}",
                ch, row, col, attrs.reverse, attrs.bg
            ));
        }
        self.set_cell(row, col, Cell { ch, attrs });
        if self.cursor_col + 1 >= self.cols {
            // Stay on the last column; defer the wrap.
            self.pending_wrap = true;
        } else {
            self.cursor_col += 1;
        }
    }

    fn cursor_row_advance(&mut self) {
        if self.cursor_row == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.cursor_row + 1 < self.rows {
            self.cursor_row += 1;
        }
    }

    fn scroll_up(&mut self, n: u16) {
        let top = self.scroll_top as usize;
        let bot = self.scroll_bottom as usize;
        if bot < top {
            return;
        }
        let n = (n as usize).min(bot - top + 1);
        let cols = self.cols as usize;
        let blank = self.blank_cell();
        self.content_dirty = true;
        for row in top..=bot {
            let dst_start = row * cols;
            let dst_end = dst_start + cols;
            if row + n <= bot {
                let src_start = (row + n) * cols;
                let src_end = src_start + cols;
                self.cells.copy_within(src_start..src_end, dst_start);
            } else {
                for c in &mut self.cells[dst_start..dst_end] {
                    *c = blank;
                }
            }
        }
    }

    fn scroll_down(&mut self, n: u16) {
        let top = self.scroll_top as usize;
        let bot = self.scroll_bottom as usize;
        if bot < top {
            return;
        }
        let n = (n as usize).min(bot - top + 1);
        let cols = self.cols as usize;
        let blank = self.blank_cell();
        self.content_dirty = true;
        for row in (top..=bot).rev() {
            let dst_start = row * cols;
            let dst_end = dst_start + cols;
            if row >= top + n {
                let src_start = (row - n) * cols;
                let src_end = src_start + cols;
                self.cells.copy_within(src_start..src_end, dst_start);
            } else {
                for c in &mut self.cells[dst_start..dst_end] {
                    *c = blank;
                }
            }
        }
    }

    /// Cell to fill into erased / scrolled-out positions. Uses the current
    /// background color so colored-background apps work, but drops bold /
    /// underline / reverse so an active reverse-video SGR doesn't bleed into
    /// erased regions and leave white blocks behind.
    fn blank_cell(&self) -> Cell {
        let mut attrs = CellAttrs::default();
        attrs.bg = self.current_attrs.bg;
        Cell { ch: ' ', attrs }
    }

    fn erase_in_display(&mut self, mode: u16) {
        let blank = self.blank_cell();
        let len = self.cells.len();
        let mut changed = false;
        match mode {
            0 => {
                let start = self.idx(self.cursor_row, self.cursor_col);
                changed = blank_range(&mut self.cells, start, len, blank);
            }
            1 => {
                let end = (self.idx(self.cursor_row, self.cursor_col) + 1).min(len);
                changed = blank_range(&mut self.cells, 0, end, blank);
            }
            2 | 3 => {
                changed = blank_range(&mut self.cells, 0, len, blank);
            }
            _ => {}
        }
        if changed {
            self.content_dirty = true;
        }
    }

    fn erase_in_line(&mut self, mode: u16) {
        let blank = self.blank_cell();
        let row = self.cursor_row;
        let row_start = self.idx(row, 0);
        let row_end = row_start + self.cols as usize;
        let cur = self.idx(row, self.cursor_col);
        let mut changed = false;
        match mode {
            0 => changed = blank_range(&mut self.cells, cur, row_end, blank),
            1 => {
                let end = (cur + 1).min(row_end);
                changed = blank_range(&mut self.cells, row_start, end, blank);
            }
            2 => changed = blank_range(&mut self.cells, row_start, row_end, blank),
            _ => {}
        }
        if changed {
            self.content_dirty = true;
        }
    }

    fn delete_chars(&mut self, n: u16) {
        let row = self.cursor_row;
        let col = self.cursor_col;
        let n = (n as usize).min((self.cols - col) as usize);
        let row_start = self.idx(row, 0);
        let cols = self.cols as usize;
        let cur = row_start + col as usize;
        let row_end = row_start + cols;
        // Shift left by n
        let blank = self.blank_cell();
        if cur + n <= row_end {
            self.cells.copy_within((cur + n)..row_end, cur);
        }
        for c in &mut self.cells[(row_end - n)..row_end] {
            *c = blank;
        }
        self.content_dirty = true;
    }

    /// DECECH: erase N characters at the cursor position with the cursor
    /// staying put. This is what TUIs use to wipe a stale glyph (e.g. a
    /// previously-rendered reverse-video cursor cell) without disturbing the
    /// surrounding line.
    fn erase_chars(&mut self, n: u16) {
        let row = self.cursor_row;
        let col = self.cursor_col;
        if col >= self.cols {
            return;
        }
        let n = (n as usize).min((self.cols - col) as usize);
        if n == 0 {
            return;
        }
        let row_start = self.idx(row, 0);
        let cur = row_start + col as usize;
        let blank = self.blank_cell();
        if blank_range(&mut self.cells, cur, cur + n, blank) {
            self.content_dirty = true;
        }
    }

    fn insert_chars(&mut self, n: u16) {
        let row = self.cursor_row;
        let col = self.cursor_col;
        let n = (n as usize).min((self.cols - col) as usize);
        let row_start = self.idx(row, 0);
        let cols = self.cols as usize;
        let cur = row_start + col as usize;
        let row_end = row_start + cols;
        // Shift right by n
        if n > 0 && cur + n <= row_end {
            for i in (cur..(row_end - n)).rev() {
                self.cells[i + n] = self.cells[i];
            }
        }
        let blank = self.blank_cell();
        let fill_end = (cur + n).min(row_end);
        for c in &mut self.cells[cur..fill_end] {
            *c = blank;
        }
        self.content_dirty = true;
    }

    fn insert_lines(&mut self, n: u16) {
        if self.cursor_row < self.scroll_top || self.cursor_row > self.scroll_bottom {
            return;
        }
        let saved_top = self.scroll_top;
        self.scroll_top = self.cursor_row;
        self.scroll_down(n);
        self.scroll_top = saved_top;
    }

    fn delete_lines(&mut self, n: u16) {
        if self.cursor_row < self.scroll_top || self.cursor_row > self.scroll_bottom {
            return;
        }
        let saved_top = self.scroll_top;
        self.scroll_top = self.cursor_row;
        self.scroll_up(n);
        self.scroll_top = saved_top;
    }

    fn set_cursor(&mut self, row: u16, col: u16) {
        self.cursor_row = row.min(self.rows.saturating_sub(1));
        self.cursor_col = col.min(self.cols.saturating_sub(1));
        self.pending_wrap = false;
    }

    fn cancel_pending_wrap(&mut self) {
        self.pending_wrap = false;
    }

    fn save_cursor(&mut self) {
        self.saved_cursor = Some((self.cursor_row, self.cursor_col, self.current_attrs));
    }

    fn restore_cursor(&mut self) {
        if let Some((r, c, a)) = self.saved_cursor {
            self.cursor_row = r.min(self.rows.saturating_sub(1));
            self.cursor_col = c.min(self.cols.saturating_sub(1));
            self.current_attrs = a;
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        let mut new_cells = vec![Cell::default(); cols as usize * rows as usize];
        let copy_rows = self.rows.min(rows) as usize;
        let copy_cols = self.cols.min(cols) as usize;
        for r in 0..copy_rows {
            for c in 0..copy_cols {
                let old = self.cells[r * self.cols as usize + c];
                new_cells[r * cols as usize + c] = old;
            }
        }
        self.cells = new_cells;
        self.cols = cols;
        self.rows = rows;
        self.scroll_top = 0;
        self.scroll_bottom = rows.saturating_sub(1);
        self.cursor_row = self.cursor_row.min(rows - 1);
        self.cursor_col = self.cursor_col.min(cols - 1);
    }
}

// ---------------------------------------------------------------------------
// ANSI/VT parser.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum ParseState {
    Ground,
    Esc,
    Csi,
    Osc,
    CsiPrivate, // saw ?
}

pub struct Parser {
    state: ParseState,
    params: Vec<u32>,
    current_param: Option<u32>,
    intermediate: u8,
    // UTF-8 reassembly
    utf8_buf: [u8; 4],
    utf8_len: usize,
    utf8_expected: usize,
    // alt screen toggle
    using_alt: bool,
    alt_grid: Option<Grid>,
}

impl Parser {
    pub fn new() -> Self {
        Self {
            state: ParseState::Ground,
            params: Vec::with_capacity(8),
            current_param: None,
            intermediate: 0,
            utf8_buf: [0u8; 4],
            utf8_len: 0,
            utf8_expected: 0,
            using_alt: false,
            alt_grid: None,
        }
    }

    pub fn feed(&mut self, grid: &mut Grid, bytes: &[u8]) {
        for &b in bytes {
            self.feed_byte(grid, b);
        }
    }

    fn feed_byte(&mut self, grid: &mut Grid, b: u8) {
        // C0 controls handled in any state
        match self.state {
            ParseState::Ground => {
                if self.utf8_len > 0 {
                    self.utf8_buf[self.utf8_len] = b;
                    self.utf8_len += 1;
                    if self.utf8_len == self.utf8_expected {
                        if let Ok(s) = std::str::from_utf8(&self.utf8_buf[..self.utf8_len]) {
                            if let Some(c) = s.chars().next() {
                                grid.put_char(c);
                            }
                        }
                        self.utf8_len = 0;
                    }
                    return;
                }
                self.ground_byte(grid, b);
            }
            ParseState::Esc => self.esc_byte(grid, b),
            ParseState::Csi | ParseState::CsiPrivate => self.csi_byte(grid, b),
            ParseState::Osc => self.osc_byte(b),
        }
    }

    fn ground_byte(&mut self, grid: &mut Grid, b: u8) {
        match b {
            0x07 => {} // BEL
            0x08 => {
                // BS
                grid.cancel_pending_wrap();
                if grid.cursor_col > 0 {
                    grid.cursor_col -= 1;
                }
            }
            0x09 => {
                // HT — advance to next 8-col stop
                grid.cancel_pending_wrap();
                let next = ((grid.cursor_col / 8) + 1) * 8;
                grid.cursor_col = next.min(grid.cols.saturating_sub(1));
            }
            0x0A | 0x0B | 0x0C => {
                // LF / VT / FF
                grid.cancel_pending_wrap();
                grid.cursor_row_advance();
            }
            0x0D => {
                grid.cancel_pending_wrap();
                grid.cursor_col = 0;
            }
            0x1B => {
                self.state = ParseState::Esc;
                self.params.clear();
                self.current_param = None;
                self.intermediate = 0;
            }
            0x00..=0x1F | 0x7F => {} // ignore other controls
            _ => {
                // UTF-8 decoding
                if b < 0x80 {
                    grid.put_char(b as char);
                } else {
                    let expected = if b & 0xE0 == 0xC0 {
                        2
                    } else if b & 0xF0 == 0xE0 {
                        3
                    } else if b & 0xF8 == 0xF0 {
                        4
                    } else {
                        // Invalid leading byte; render replacement
                        grid.put_char(char::REPLACEMENT_CHARACTER);
                        return;
                    };
                    self.utf8_buf[0] = b;
                    self.utf8_len = 1;
                    self.utf8_expected = expected;
                }
            }
        }
    }

    fn esc_byte(&mut self, grid: &mut Grid, b: u8) {
        match b {
            b'[' => {
                self.state = ParseState::Csi;
                self.params.clear();
                self.current_param = None;
                self.intermediate = 0;
            }
            b']' => {
                self.state = ParseState::Osc;
            }
            b'7' => {
                grid.save_cursor();
                self.state = ParseState::Ground;
            }
            b'8' => {
                grid.restore_cursor();
                self.state = ParseState::Ground;
            }
            b'c' => {
                // RIS — reset
                grid.current_attrs.reset();
                grid.set_cursor(0, 0);
                grid.erase_in_display(2);
                self.state = ParseState::Ground;
            }
            b'D' => {
                // IND — index (cursor down or scroll)
                grid.cursor_row_advance();
                self.state = ParseState::Ground;
            }
            b'E' => {
                grid.cursor_col = 0;
                grid.cursor_row_advance();
                self.state = ParseState::Ground;
            }
            b'M' => {
                // RI — reverse index
                if grid.cursor_row == grid.scroll_top {
                    grid.scroll_down(1);
                } else if grid.cursor_row > 0 {
                    grid.cursor_row -= 1;
                }
                self.state = ParseState::Ground;
            }
            0x20..=0x2F => {
                // Intermediate, just skip — final byte will follow
            }
            _ => {
                self.state = ParseState::Ground;
            }
        }
    }

    fn push_param_digit(&mut self, b: u8) {
        let d = (b - b'0') as u32;
        let cur = self.current_param.unwrap_or(0);
        let next = cur.saturating_mul(10).saturating_add(d);
        self.current_param = Some(next);
    }

    fn finalize_param(&mut self) {
        let v = self.current_param.unwrap_or(0);
        self.params.push(v);
        self.current_param = None;
    }

    fn param_or(&self, idx: usize, default: u32) -> u32 {
        self.params.get(idx).copied().filter(|&v| v != 0).unwrap_or(default)
    }

    fn param_or_zero(&self, idx: usize) -> u32 {
        self.params.get(idx).copied().unwrap_or(0)
    }

    fn csi_byte(&mut self, grid: &mut Grid, b: u8) {
        match b {
            b'?' if self.params.is_empty() && self.current_param.is_none() => {
                self.state = ParseState::CsiPrivate;
            }
            b'>' | b'=' if self.params.is_empty() && self.current_param.is_none() => {
                // Unsupported leading byte — just keep parsing
            }
            b'0'..=b'9' => {
                self.push_param_digit(b);
            }
            b';' | b':' => {
                self.finalize_param();
            }
            0x40..=0x7E => {
                if self.current_param.is_some() || self.params.is_empty() {
                    // Always finalize the in-progress param so dispatch sees it.
                    if self.current_param.is_some() {
                        self.finalize_param();
                    }
                }
                let private = self.state == ParseState::CsiPrivate;
                self.dispatch_csi(grid, b, private);
                self.state = ParseState::Ground;
                self.params.clear();
                self.current_param = None;
            }
            _ => {}
        }
    }

    fn dispatch_csi(&mut self, grid: &mut Grid, final_byte: u8, private: bool) {
        if diagnose::is_enabled() {
            let prefix = if private { "?" } else { "" };
            diagnose::log(format!(
                "term: CSI {prefix}{:?} {} (cursor=({},{}))",
                self.params,
                final_byte as char,
                grid.cursor_row,
                grid.cursor_col
            ));
        }
        // Cursor-moving and content-modifying sequences cancel a deferred
        // last-column wrap. SGR (m), mode-set (h/l), DECSCUSR (q), and tab
        // clear (g) don't touch the cursor — they MUST NOT cancel pending
        // wrap, otherwise apps that style a prompt right after filling a row
        // (e.g. "<full-width divider><SGR><next-row content>") drop their
        // wrap and the next character lands on the previous row.
        let preserves_pending_wrap = matches!(
            final_byte,
            b'm' | b'h' | b'l' | b'q' | b'g'
        );
        if !preserves_pending_wrap {
            grid.cancel_pending_wrap();
        }
        match (private, final_byte) {
            (false, b'A') => {
                let n = self.param_or(0, 1) as u16;
                grid.cursor_row = grid.cursor_row.saturating_sub(n);
            }
            (false, b'B') => {
                let n = self.param_or(0, 1) as u16;
                grid.cursor_row =
                    (grid.cursor_row.saturating_add(n)).min(grid.rows.saturating_sub(1));
            }
            (false, b'C') => {
                let n = self.param_or(0, 1) as u16;
                grid.cursor_col =
                    (grid.cursor_col.saturating_add(n)).min(grid.cols.saturating_sub(1));
            }
            (false, b'D') => {
                let n = self.param_or(0, 1) as u16;
                grid.cursor_col = grid.cursor_col.saturating_sub(n);
            }
            (false, b'E') => {
                let n = self.param_or(0, 1) as u16;
                grid.cursor_row =
                    (grid.cursor_row.saturating_add(n)).min(grid.rows.saturating_sub(1));
                grid.cursor_col = 0;
            }
            (false, b'F') => {
                let n = self.param_or(0, 1) as u16;
                grid.cursor_row = grid.cursor_row.saturating_sub(n);
                grid.cursor_col = 0;
            }
            (false, b'G') => {
                let col = self.param_or(0, 1) as u16;
                grid.cursor_col = col.saturating_sub(1).min(grid.cols.saturating_sub(1));
            }
            (false, b'H') | (false, b'f') => {
                let row = self.param_or(0, 1) as u16;
                let col = self.param_or(1, 1) as u16;
                grid.set_cursor(row.saturating_sub(1), col.saturating_sub(1));
            }
            (false, b'd') => {
                let row = self.param_or(0, 1) as u16;
                grid.cursor_row = row.saturating_sub(1).min(grid.rows.saturating_sub(1));
            }
            (false, b'J') => {
                let m = self.param_or_zero(0) as u16;
                grid.erase_in_display(m);
            }
            (false, b'K') => {
                let m = self.param_or_zero(0) as u16;
                grid.erase_in_line(m);
            }
            (false, b'L') => {
                let n = self.param_or(0, 1) as u16;
                grid.insert_lines(n);
            }
            (false, b'M') => {
                let n = self.param_or(0, 1) as u16;
                grid.delete_lines(n);
            }
            (false, b'P') => {
                let n = self.param_or(0, 1) as u16;
                grid.delete_chars(n);
            }
            (false, b'X') => {
                let n = self.param_or(0, 1) as u16;
                grid.erase_chars(n);
            }
            (false, b'@') => {
                let n = self.param_or(0, 1) as u16;
                grid.insert_chars(n);
            }
            (false, b'S') => {
                let n = self.param_or(0, 1) as u16;
                grid.scroll_up(n);
            }
            (false, b'T') => {
                let n = self.param_or(0, 1) as u16;
                grid.scroll_down(n);
            }
            (false, b'r') => {
                let top = self.param_or(0, 1) as u16;
                let bottom = self.param_or(1, grid.rows as u32) as u16;
                grid.scroll_top = top.saturating_sub(1).min(grid.rows.saturating_sub(1));
                grid.scroll_bottom = bottom
                    .saturating_sub(1)
                    .min(grid.rows.saturating_sub(1))
                    .max(grid.scroll_top);
                grid.set_cursor(0, 0);
            }
            (false, b's') => grid.save_cursor(),
            (false, b'u') => grid.restore_cursor(),
            (false, b'm') => self.apply_sgr(grid),
            (false, b'Z') => {
                // CBT: cursor backward tabulation, 8-col stops.
                let n = self.param_or(0, 1) as u16;
                let mut col = grid.cursor_col;
                for _ in 0..n {
                    if col == 0 {
                        break;
                    }
                    col = ((col.saturating_sub(1)) / 8) * 8;
                }
                grid.cursor_col = col;
            }
            (false, b'g') => {
                // TBC: tab clear — we don't track custom tab stops, no-op.
            }
            (false, b'q') => {
                // DECSCUSR: cursor shape. We render our own cursor visual.
            }
            (true, b'h') => self.set_dec_modes(grid, true),
            (true, b'l') => self.set_dec_modes(grid, false),
            _ => {}
        }
    }

    fn set_dec_modes(&mut self, grid: &mut Grid, enable: bool) {
        for &p in &self.params {
            match p {
                25 => grid.cursor_visible = enable,
                1049 | 47 | 1047 => {
                    if enable && !self.using_alt {
                        let alt = mem::replace(grid, Grid::new(grid.cols, grid.rows));
                        self.alt_grid = Some(alt);
                        self.using_alt = true;
                    } else if !enable && self.using_alt {
                        if let Some(saved) = self.alt_grid.take() {
                            *grid = saved;
                        }
                        self.using_alt = false;
                    }
                }
                _ => {}
            }
        }
    }

    fn apply_sgr(&mut self, grid: &mut Grid) {
        if self.params.is_empty() {
            grid.current_attrs.reset();
            return;
        }
        let mut i = 0;
        while i < self.params.len() {
            let p = self.params[i];
            match p {
                0 => grid.current_attrs.reset(),
                1 => grid.current_attrs.bold = true,
                4 => grid.current_attrs.underline = true,
                7 => grid.current_attrs.reverse = true,
                22 => grid.current_attrs.bold = false,
                24 => grid.current_attrs.underline = false,
                27 => grid.current_attrs.reverse = false,
                30..=37 => grid.current_attrs.fg = Some(AnsiColor::Indexed((p - 30) as u8)),
                38 => {
                    if let Some((color, consumed)) = parse_extended_color(&self.params[i + 1..]) {
                        grid.current_attrs.fg = Some(color);
                        i += consumed;
                    }
                }
                39 => grid.current_attrs.fg = None,
                40..=47 => grid.current_attrs.bg = Some(AnsiColor::Indexed((p - 40) as u8)),
                48 => {
                    if let Some((color, consumed)) = parse_extended_color(&self.params[i + 1..]) {
                        grid.current_attrs.bg = Some(color);
                        i += consumed;
                    }
                }
                49 => grid.current_attrs.bg = None,
                90..=97 => grid.current_attrs.fg = Some(AnsiColor::Indexed((p - 90 + 8) as u8)),
                100..=107 => grid.current_attrs.bg = Some(AnsiColor::Indexed((p - 100 + 8) as u8)),
                _ => {}
            }
            i += 1;
        }
    }

    fn osc_byte(&mut self, b: u8) {
        // Skip until BEL or ESC \
        match b {
            0x07 => {
                self.state = ParseState::Ground;
            }
            0x1B => {
                // wait for backslash via Esc state, but we just go to Ground
                self.state = ParseState::Ground;
            }
            _ => {}
        }
    }
}

fn parse_extended_color(rest: &[u32]) -> Option<(AnsiColor, usize)> {
    match rest.first()? {
        5 => {
            // 5;n — indexed
            let n = *rest.get(1)? as u8;
            Some((AnsiColor::Indexed(n), 2))
        }
        2 => {
            // 2;r;g;b
            let r = *rest.get(1)? as u8;
            let g = *rest.get(2)? as u8;
            let b = *rest.get(3)? as u8;
            Some((AnsiColor::Rgb(r, g, b), 4))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// PTY ownership and worker thread.
// ---------------------------------------------------------------------------

struct PtyInner {
    hpc: HPCON,
    h_in_write: HANDLE,
    h_out_read: HANDLE,
    h_process: HANDLE,
    h_thread: HANDLE,
}

unsafe impl Send for PtyInner {}
unsafe impl Sync for PtyInner {}

impl Drop for PtyInner {
    fn drop(&mut self) {
        // We deliberately do NOT TerminateProcess(h_process) here. The
        // spawned process is our claude-shim, which has its own
        // refcount-based lifecycle (terminal-attached + per-session
        // pipe subscribers). Hard-killing it would defeat that — a
        // session with external subscribers should survive when the
        // manager panel closes. Closing the ConPTY hands the shim an
        // EOF on stdin, which its `local_stdin_to_pty` thread treats
        // as "my console went away" and propagates to the lifecycle
        // watcher, which makes the right call (kill claude only if no
        // subscribers are left).
        unsafe {
            if self.hpc.0 != 0 {
                ClosePseudoConsole(self.hpc);
            }
            if !self.h_in_write.is_invalid() {
                let _ = CloseHandle(self.h_in_write);
            }
            if !self.h_out_read.is_invalid() {
                let _ = CloseHandle(self.h_out_read);
            }
            if !self.h_process.is_invalid() {
                let _ = CloseHandle(self.h_process);
            }
            if !self.h_thread.is_invalid() {
                let _ = CloseHandle(self.h_thread);
            }
        }
    }
}

pub struct Terminal {
    pty: Arc<Mutex<Option<PtyInner>>>,
    pub grid: Arc<Mutex<Grid>>,
    pub cols: u16,
    pub rows: u16,
    closed: Arc<AtomicBool>,
    paint_pending: Arc<AtomicBool>,
    /// Wall-clock millis (since UNIX epoch) when the worker thread last
    /// parsed output bytes. Compared against `last_input_ms` to drive the
    /// per-session attention heuristic.
    last_output_ms: Arc<AtomicU64>,
    last_input_ms: Arc<AtomicU64>,
}

/// Wall-clock millis since the UNIX epoch — best-effort, returns 0 if the
/// clock is broken (which it shouldn't be).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Terminal {
    pub fn spawn(
        cols: u16,
        rows: u16,
        cmd: &str,
        cwd: Option<&std::path::Path>,
        notify_hwnd: HWND,
        notify_msg: u32,
    ) -> Result<Self, String> {
        let pty_inner = unsafe { spawn_pty(cols, rows, cmd, cwd)? };
        let grid = Arc::new(Mutex::new(Grid::new(cols, rows)));
        let pty = Arc::new(Mutex::new(Some(pty_inner)));
        let closed = Arc::new(AtomicBool::new(false));
        let paint_pending = Arc::new(AtomicBool::new(false));
        let now = now_ms();
        let last_output_ms = Arc::new(AtomicU64::new(now));
        let last_input_ms = Arc::new(AtomicU64::new(now));

        let h_out_read_addr: isize = {
            let guard = pty.lock().map_err(|_| "pty mutex poisoned")?;
            guard.as_ref().unwrap().h_out_read.0 as isize
        };

        let grid_for_thread = Arc::clone(&grid);
        let closed_for_thread = Arc::clone(&closed);
        let paint_pending_for_thread = Arc::clone(&paint_pending);
        let last_output_for_thread = Arc::clone(&last_output_ms);
        let notify = notify_hwnd.0 as isize;
        thread::spawn(move || {
            let h = HANDLE(h_out_read_addr as *mut _);
            output_reader_loop(
                h,
                grid_for_thread,
                closed_for_thread,
                paint_pending_for_thread,
                last_output_for_thread,
                notify,
                notify_msg,
            );
        });

        Ok(Self {
            pty,
            grid,
            cols,
            rows,
            closed,
            paint_pending,
            last_output_ms,
            last_input_ms,
        })
    }

    pub fn last_output_ms(&self) -> u64 {
        self.last_output_ms.load(Ordering::Acquire)
    }

    pub fn last_input_ms(&self) -> u64 {
        self.last_input_ms.load(Ordering::Acquire)
    }

    pub fn paint_pending(&self) -> &Arc<AtomicBool> {
        &self.paint_pending
    }

    pub fn write_input(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.last_input_ms.store(now_ms(), Ordering::Release);
        let h = {
            let guard = match self.pty.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            match guard.as_ref() {
                Some(inner) => inner.h_in_write,
                None => return,
            }
        };
        unsafe {
            let mut written: u32 = 0;
            let _ = WriteFile(
                h,
                Some(data),
                Some(&mut written),
                None,
            );
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        if let Ok(mut grid) = self.grid.lock() {
            grid.resize(cols, rows);
        }
        let hpc = {
            let guard = match self.pty.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.as_ref().map(|inner| inner.hpc)
        };
        if let Some(hpc) = hpc {
            unsafe {
                let _ = ResizePseudoConsole(
                    hpc,
                    COORD {
                        X: cols as i16,
                        Y: rows as i16,
                    },
                );
            }
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        // Closing the PTY breaks the reader's pipe, unblocking ReadFile.
        let mut guard = match self.pty.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *guard = None;
    }
}

unsafe fn spawn_pty(
    cols: u16,
    rows: u16,
    cmd: &str,
    cwd: Option<&std::path::Path>,
) -> Result<PtyInner, String> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: TRUE,
    };
    let sa_ptr: *const SECURITY_ATTRIBUTES = &sa;

    let mut h_pty_in_read = HANDLE::default();
    let mut h_pty_in_write = HANDLE::default();
    let mut h_pty_out_read = HANDLE::default();
    let mut h_pty_out_write = HANDLE::default();

    if CreatePipe(&mut h_pty_in_read, &mut h_pty_in_write, Some(sa_ptr), 0).is_err() {
        return Err("CreatePipe (in) failed".into());
    }
    if CreatePipe(&mut h_pty_out_read, &mut h_pty_out_write, Some(sa_ptr), 0).is_err() {
        let _ = CloseHandle(h_pty_in_read);
        let _ = CloseHandle(h_pty_in_write);
        return Err("CreatePipe (out) failed".into());
    }

    let size = COORD {
        X: cols.max(1) as i16,
        Y: rows.max(1) as i16,
    };
    let hpc = match CreatePseudoConsole(size, h_pty_in_read, h_pty_out_write, 0) {
        Ok(h) => h,
        Err(e) => {
            let _ = CloseHandle(h_pty_in_read);
            let _ = CloseHandle(h_pty_out_write);
            let _ = CloseHandle(h_pty_in_write);
            let _ = CloseHandle(h_pty_out_read);
            return Err(format!("CreatePseudoConsole failed: {e}"));
        }
    };
    // ConPTY duplicates the handles internally; we close our refs.
    let _ = CloseHandle(h_pty_in_read);
    let _ = CloseHandle(h_pty_out_write);

    let mut attr_size: usize = 0;
    let _ = InitializeProcThreadAttributeList(
        LPPROC_THREAD_ATTRIBUTE_LIST(ptr::null_mut()),
        1,
        0,
        &mut attr_size,
    );
    if attr_size == 0 {
        ClosePseudoConsole(hpc);
        let _ = CloseHandle(h_pty_in_write);
        let _ = CloseHandle(h_pty_out_read);
        return Err("InitializeProcThreadAttributeList sizing failed".into());
    }
    let mut attr_buf: Vec<u8> = vec![0u8; attr_size];
    let attr_list = LPPROC_THREAD_ATTRIBUTE_LIST(attr_buf.as_mut_ptr() as *mut _);
    if InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size).is_err() {
        ClosePseudoConsole(hpc);
        let _ = CloseHandle(h_pty_in_write);
        let _ = CloseHandle(h_pty_out_read);
        return Err("InitializeProcThreadAttributeList failed".into());
    }
    if UpdateProcThreadAttribute(
        attr_list,
        0,
        PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
        Some(hpc.0 as *const c_void),
        mem::size_of::<HPCON>(),
        None,
        None,
    )
    .is_err()
    {
        DeleteProcThreadAttributeList(attr_list);
        ClosePseudoConsole(hpc);
        let _ = CloseHandle(h_pty_in_write);
        let _ = CloseHandle(h_pty_out_read);
        return Err("UpdateProcThreadAttribute failed".into());
    }

    let mut si: STARTUPINFOEXW = mem::zeroed();
    si.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
    si.lpAttributeList = attr_list;

    let mut cmd_wide: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
    let cwd_wide: Option<Vec<u16>> = cwd.map(|p| {
        p.to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    });
    let cwd_ptr = match &cwd_wide {
        Some(v) => PCWSTR::from_raw(v.as_ptr()),
        None => PCWSTR::null(),
    };
    let mut pi = PROCESS_INFORMATION::default();

    let ok = CreateProcessW(
        PCWSTR::null(),
        PWSTR::from_raw(cmd_wide.as_mut_ptr()),
        None,
        None,
        false,
        EXTENDED_STARTUPINFO_PRESENT,
        None,
        cwd_ptr,
        &si.StartupInfo,
        &mut pi,
    )
    .is_ok();

    DeleteProcThreadAttributeList(attr_list);

    if !ok {
        ClosePseudoConsole(hpc);
        let _ = CloseHandle(h_pty_in_write);
        let _ = CloseHandle(h_pty_out_read);
        return Err("CreateProcessW failed".into());
    }

    Ok(PtyInner {
        hpc,
        h_in_write: h_pty_in_write,
        h_out_read: h_pty_out_read,
        h_process: pi.hProcess,
        h_thread: pi.hThread,
    })
}

fn output_reader_loop(
    h_out_read: HANDLE,
    grid: Arc<Mutex<Grid>>,
    closed: Arc<AtomicBool>,
    paint_pending: Arc<AtomicBool>,
    last_output_ms: Arc<AtomicU64>,
    notify_hwnd: isize,
    notify_msg: u32,
) {
    let mut parser = Parser::new();
    let mut buf = [0u8; 4096];
    loop {
        if closed.load(Ordering::Acquire) {
            return;
        }
        let mut n: u32 = 0;
        let ok = unsafe {
            ReadFile(
                h_out_read,
                Some(&mut buf[..]),
                Some(&mut n),
                None,
            )
        };
        if ok.is_err() || n == 0 {
            // Pipe closed (child exited or PTY destroyed).
            diagnose::log("terminal: ReadFile returned 0 / err — exiting reader");
            return;
        }
        let any_change = {
            let mut g = match grid.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            parser.feed(&mut g, &buf[..n as usize]);
            g.take_content_dirty()
        };
        // Only count this as "real activity" if the grid contents actually
        // changed. Cursor / SGR / mode pings that pass through the parser
        // without modifying any cell shouldn't keep a session marked Working
        // forever.
        if any_change {
            last_output_ms.store(now_ms(), Ordering::Release);
        }
        // Coalesce repaints: only post a fresh notification if the previous
        // one hasn't been consumed yet. The UI side clears the flag before
        // painting, so output that arrives during a paint triggers exactly
        // one follow-up paint.
        if !paint_pending.swap(true, Ordering::AcqRel) {
            unsafe {
                let _ = PostMessageW(
                    HWND(notify_hwnd as *mut _),
                    notify_msg,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }
}
