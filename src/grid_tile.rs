//! The grid view's tile: a `cols x rows` grid of live terminals. Slot 0 is
//! the project tree — the grid has no sidebar, so that cell is how sessions
//! get started and resumed — and every live session follows in its own cell,
//! rendered by the same [`SessionView`](crate::session_view::SessionView) the
//! dashboard's main slot uses.
//!
//! Cells are equal-sized and stretched to fill the tile; the user picks
//! `cols x rows` from the view button's right-click menu. More sessions than
//! fit flow into extra rows at the same cell size and are reached by
//! scrolling.
//!
//! Where a conversation sits is the user's to change: its frame's top edge
//! drags the cell to another slot, and its right / bottom edges stretch it
//! across several. That arrangement is a [`GridPlacement`] per session,
//! written to settings.json, so it comes back on the next run. Conversations
//! that have never been arranged flow into whatever cells the arranged ones
//! leave free.
//!
//! Each cell is a real terminal: its PTY is resized to the cell, it renders
//! its own cursor and input box, and mouse/wheel/keyboard events route to it.
//! Clicking a cell makes that session the keyboard target.

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;

use crate::dashboard::{self, CursorHint, NavState, SlotControl, TileAction};
use crate::native_interop::Color;
use crate::project_tree;
use crate::resume_card;
use crate::session_view::ChromeStyle;
use crate::sessions::{Session, SessionId, SessionStatus, Sessions};
use crate::terminal_view::MIN_FONT_POINT_SIZE;

/// Largest grid the size picker offers, in either direction.
pub const MAX_GRID: i32 = 10;

/// How much smaller the grid's terminals render than the dashboard's main
/// slot. A cell is a fraction of the panel, and the PTY's column count
/// follows the font, so the smaller face is what keeps claude's layout from
/// collapsing into a two-word-per-line column. The user zooms one size;
/// this keeps the cells in proportion with it.
const CELL_FONT_OFFSET: i32 = 3;

/// Point size a cell renders at, given the size the main slot is set to.
pub fn cell_font_pt(main_pt: i32) -> i32 {
    (main_pt - CELL_FONT_OFFSET).max(MIN_FONT_POINT_SIZE - CELL_FONT_OFFSET)
}

/// Design pixels at 96 DPI.
/// Floor on a cell's size. Cells normally stretch to fill the tile; these
/// only bite when the chosen grid is finer than the panel can show, at
/// which point the grid overflows and scrolls instead of collapsing.
const MIN_CELL_W: i32 = 160;
const MIN_CELL_H: i32 = 120;
const CELL_GAP: i32 = 12;
const CELL_PADDING: i32 = 10;
/// Width of the scrollbar gutter on the right side of the grid.
const SCROLLBAR_W: i32 = 10;
/// Minimum scrollbar thumb height — keeps the thumb grabbable even when
/// content is much taller than the tile.
const SCROLLBAR_MIN_THUMB_H: i32 = 30;
const BORDER_RADIUS: i32 = 8;
/// Grab band on a frame's top edge — the strip the name label floats in,
/// which is the cell's title bar as far as dragging is concerned. It
/// straddles the border line, so the label itself is grabbable.
const MOVE_BAND: i32 = 9;
/// Grab band inside a frame's right / bottom edges. The terminal insets its
/// own content by more than this, so a resize grab never eats a click that
/// would have selected text or hit the scrollback bar.
const RESIZE_BAND: i32 = 7;

const PANEL_BG_HEX: &str = "#262624";
const CELL_BG_HEX: &str = "#1F1F1D";
const CELL_BORDER_HEX: &str = "#3A3A38";
const STATUS_IDLE_HEX: &str = "#5A5F58";
const STATUS_THINKING_HEX: &str = "#5BD16B";
const STATUS_NEEDS_HEX: &str = "#E07A5F";
const SCROLLBAR_TRACK_HEX: &str = "#1B1B19";
const SCROLLBAR_THUMB_HEX: &str = "#3A3A38";

/// Dot colour for a session's status, drawn in the terminal's name label.
pub fn status_color(status: SessionStatus) -> Color {
    match status {
        SessionStatus::Idle => Color::from_hex(STATUS_IDLE_HEX),
        SessionStatus::Thinking => Color::from_hex(STATUS_THINKING_HEX),
        SessionStatus::NeedsAttention => Color::from_hex(STATUS_NEEDS_HEX),
    }
}

/// Full cycle of the attention pulse, in milliseconds.
const PULSE_PERIOD_MS: u64 = 1400;

/// Where in the pulse we are right now, `0.0..=1.0`, as a cosine ease so the
/// brightest and dimmest points hold for a moment instead of snapping. Driven
/// off the wall clock rather than a frame counter, so every cell pulses in
/// step and a dropped repaint doesn't shift the phase.
pub fn pulse_level() -> f64 {
    let phase = (crate::terminal::now_ms() % PULSE_PERIOD_MS) as f64 / PULSE_PERIOD_MS as f64;
    (1.0 - (phase * std::f64::consts::TAU).cos()) / 2.0
}

/// Where a cell sits in the grid, in cell units: an origin plus how many
/// cells it spans. Dragging a frame's top edge moves the origin; dragging
/// its right / bottom edge changes the span. This is also exactly what gets
/// written to settings.json, so an arrangement survives a restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GridPlacement {
    pub col: i32,
    pub row: i32,
    pub cols: i32,
    pub rows: i32,
}

impl GridPlacement {
    /// A single cell at `(col, row)`.
    pub fn cell(col: i32, row: i32) -> Self {
        Self {
            col,
            row,
            cols: 1,
            rows: 1,
        }
    }

    /// First column / row past this placement.
    fn right(&self) -> i32 {
        self.col + self.cols
    }
    fn bottom(&self) -> i32 {
        self.row + self.rows
    }

    pub fn overlaps(&self, other: &Self) -> bool {
        self.col < other.right()
            && other.col < self.right()
            && self.row < other.bottom()
            && other.row < self.bottom()
    }

    /// Same span, wherever the two sit. Two cells of one size can trade
    /// places; anything else has to be bumped out of the way instead.
    pub fn same_size(&self, other: &Self) -> bool {
        self.cols == other.cols && self.rows == other.rows
    }

    /// Fit inside a grid `cols` wide: at least one cell of span, the origin
    /// on the board, and the right edge no further than the last column.
    /// Rows are unbounded downwards — the grid scrolls.
    fn clamped(self, cols: i32) -> Self {
        let cols_span = self.cols.clamp(1, cols.max(1));
        let rows_span = self.rows.clamp(1, MAX_GRID);
        Self {
            col: self.col.clamp(0, (cols - cols_span).max(0)),
            row: self.row.max(0),
            cols: cols_span,
            rows: rows_span,
        }
    }
}

/// Layout snapshot for one paint of the grid: column count + the stretched
/// per-cell width / height / gap, plus the inner rect (the tile minus the
/// scrollbar gutter). Computed once per paint and reused for hit-testing.
#[derive(Clone, Copy)]
struct GridLayout {
    inner: RECT,
    cell_w: i32,
    cell_h: i32,
    gap: i32,
    cols: i32,
}

/// A layout with every slot placed: `slots[0]` is the project cell, and each
/// session in `Sessions` order follows. Arranged sessions keep the placement
/// they were given; the rest flow into the cells left over.
struct GridPlan {
    layout: GridLayout,
    slots: Vec<GridPlacement>,
    /// Total content height at this arrangement, used for scrollbar thumb
    /// sizing and scroll clamping.
    content_h: i32,
}

/// Grid slot holding the project list. Session cells start one slot later.
const PROJECT_SLOT: usize = 0;

/// Build the layout for a `cols x rows_target` grid. `rows_target` is what
/// the *cell height* is derived from — how many rows are actually used
/// follows from where the sessions sit, so a grid holding more than one
/// screenful keeps the chosen cell size and scrolls.
fn compute_grid_layout(bounds: &RECT, dpi: u32, cols: i32, rows_target: i32) -> GridLayout {
    let scale = dpi as f64 / 96.0;
    let cols = cols.clamp(1, MAX_GRID);
    let rows_target = rows_target.clamp(1, MAX_GRID);
    let gap = (CELL_GAP as f64 * scale).round() as i32;
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
    let total_w = (inner.right - inner.left).max(1);
    let total_h = (inner.bottom - inner.top).max(1);
    let cell_w =
        ((total_w - (cols + 1) * gap) / cols).max((MIN_CELL_W as f64 * scale).round() as i32);
    let cell_h = ((total_h - (rows_target + 1) * gap) / rows_target)
        .max((MIN_CELL_H as f64 * scale).round() as i32);
    GridLayout {
        inner,
        cell_w,
        cell_h,
        gap,
        cols,
    }
}

/// Place every slot. `arranged[i]` is the placement session `i` was last
/// dragged to, or `None` for one the user has never arranged.
///
/// Arranged cells are laid first and keep exactly where they are; everything
/// else fills the gaps they leave, row by row, which is what makes an
/// un-arranged grid look like the plain flow it always was. An arranged
/// placement that collides with one already down is treated as un-arranged —
/// the alternative is drawing two terminals on top of each other.
fn resolve_slots(arranged: &[Option<GridPlacement>], cols: i32) -> Vec<GridPlacement> {
    let mut taken: Vec<GridPlacement> = vec![GridPlacement::cell(0, 0)];
    let mut slots: Vec<Option<GridPlacement>> = vec![None; arranged.len()];

    for (i, wanted) in arranged.iter().enumerate() {
        let Some(wanted) = wanted.map(|p| p.clamped(cols)) else {
            continue;
        };
        if taken.iter().any(|t| t.overlaps(&wanted)) {
            continue;
        }
        taken.push(wanted);
        slots[i] = Some(wanted);
    }

    for (i, slot) in slots.iter_mut().enumerate() {
        if slot.is_some() {
            continue;
        }
        // A cell that was stretched keeps its size when it re-flows; only
        // where it lands is up for grabs.
        let span = arranged[i].map(|p| p.clamped(cols)).unwrap_or(GridPlacement::cell(0, 0));
        let placed = first_free(&taken, cols, span.cols, span.rows);
        taken.push(placed);
        *slot = Some(placed);
    }

    let mut out = vec![GridPlacement::cell(0, 0)];
    out.extend(slots.into_iter().map(|s| s.unwrap_or(GridPlacement::cell(0, 0))));
    out
}

/// Topmost-leftmost free block of `span_c x span_r` cells. Scans row by row
/// past the bottom of everything placed so far, so it always finds one.
fn first_free(taken: &[GridPlacement], cols: i32, span_c: i32, span_r: i32) -> GridPlacement {
    let span_c = span_c.clamp(1, cols.max(1));
    let span_r = span_r.max(1);
    let below = taken.iter().map(|t| t.bottom()).max().unwrap_or(0);
    for row in 0..=below {
        for col in 0..=(cols - span_c).max(0) {
            let candidate = GridPlacement {
                col,
                row,
                cols: span_c,
                rows: span_r,
            };
            if !taken.iter().any(|t| t.overlaps(&candidate)) {
                return candidate;
            }
        }
    }
    // Every row down to the last occupied one is spoken for; open a new one.
    GridPlacement {
        col: 0,
        row: below,
        cols: span_c,
        rows: span_r,
    }
}

fn build_plan(
    bounds: &RECT,
    dpi: u32,
    arranged: &[Option<GridPlacement>],
    cols: i32,
    rows: i32,
) -> GridPlan {
    let layout = compute_grid_layout(bounds, dpi, cols, rows);
    let slots = resolve_slots(arranged, layout.cols);
    let used_rows = slots.iter().map(|s| s.bottom()).max().unwrap_or(0);
    let content_h = if used_rows == 0 {
        0
    } else {
        used_rows * (layout.cell_h + layout.gap) + layout.gap
    };
    GridPlan {
        layout,
        slots,
        content_h,
    }
}

/// The plan for the current session list — what every entry point here
/// starts from.
fn plan(bounds: &RECT, dpi: u32, sessions: &Sessions, cols: i32, rows: i32) -> GridPlan {
    let arranged: Vec<Option<GridPlacement>> = sessions.iter().map(|s| s.placement).collect();
    build_plan(bounds, dpi, &arranged, cols, rows)
}

impl GridPlan {
    /// Maximum legal scroll for this arrangement — `scroll_y` clamps to
    /// `[0, max_scroll]`. Zero when the content fits.
    fn max_scroll(&self) -> i32 {
        let visible_h = self.layout.inner.bottom - self.layout.inner.top;
        (self.content_h - visible_h).max(0)
    }

    /// Rect of slot `i` in the layout-virtual (pre-scroll) coordinate
    /// system. The caller subtracts `scroll_y` to get the on-screen position.
    fn slot_rect(&self, i: usize) -> RECT {
        let p = self.slots[i];
        let l = &self.layout;
        let x = l.inner.left + l.gap + p.col * (l.cell_w + l.gap);
        let y = l.inner.top + l.gap + p.row * (l.cell_h + l.gap);
        RECT {
            left: x,
            top: y,
            right: x + p.cols * l.cell_w + (p.cols - 1) * l.gap,
            bottom: y + p.rows * l.cell_h + (p.rows - 1) * l.gap,
        }
    }

    /// On-screen rect of slot `i`, i.e. [`slot_rect`](Self::slot_rect)
    /// shifted by the scroll offset. `None` when the slot falls entirely
    /// outside the visible band.
    fn visible_slot_rect(&self, i: usize, scroll_y: i32) -> Option<RECT> {
        let virt = self.slot_rect(i);
        let cell = RECT {
            top: virt.top - scroll_y,
            bottom: virt.bottom - scroll_y,
            ..virt
        };
        if cell.bottom < self.layout.inner.top || cell.top >= self.layout.inner.bottom {
            return None;
        }
        Some(cell)
    }

    /// Cell the point `(x, y_virtual)` falls in — `y_virtual` being a
    /// y already shifted back into the pre-scroll system. Out-of-grid points
    /// give out-of-range cells; callers clamp through
    /// [`GridPlacement::clamped`].
    fn cell_at(&self, x: i32, y_virtual: i32) -> (i32, i32) {
        let l = &self.layout;
        let step_x = (l.cell_w + l.gap).max(1);
        let step_y = (l.cell_h + l.gap).max(1);
        (
            floor_div(x - l.inner.left - l.gap, step_x),
            floor_div(y_virtual - l.inner.top - l.gap, step_y),
        )
    }
}

/// Floor division — `i32`'s `/` truncates towards zero, which would fold the
/// cell left of the grid's origin onto the first one.
fn floor_div(a: i32, b: i32) -> i32 {
    let q = a / b;
    if a % b != 0 && (a < 0) != (b < 0) {
        q - 1
    } else {
        q
    }
}

/// Compute the scrollbar's track + thumb rect inside `bounds`. Returns
/// `None` when content fits (no scrollbar needed).
fn scrollbar_rects(bounds: &RECT, plan: &GridPlan, dpi: u32, scroll_y: i32) -> Option<(RECT, RECT)> {
    let visible_h = plan.layout.inner.bottom - plan.layout.inner.top;
    if plan.content_h <= visible_h {
        return None;
    }
    let scale = dpi as f64 / 96.0;
    let scrollbar_w = (SCROLLBAR_W as f64 * scale).round() as i32;
    let track = RECT {
        left: bounds.right - scrollbar_w,
        top: bounds.top + plan.layout.gap,
        right: bounds.right,
        bottom: bounds.bottom - plan.layout.gap,
    };
    let track_h = (track.bottom - track.top).max(1);
    let min_thumb = (SCROLLBAR_MIN_THUMB_H as f64 * scale).round() as i32;
    let raw_thumb = (visible_h as i64 * track_h as i64 / plan.content_h as i64) as i32;
    let thumb_h = raw_thumb.max(min_thumb).min(track_h);
    let max_s = plan.max_scroll().max(1);
    let thumb_top =
        track.top + ((scroll_y as i64 * (track_h - thumb_h) as i64) / max_s as i64) as i32;
    let thumb = RECT {
        left: track.left,
        top: thumb_top,
        right: track.right,
        bottom: thumb_top + thumb_h,
    };
    Some((track, thumb))
}

/// The region of the project cell that the tree itself is drawn into.
/// The panel routes hover / wheel / click there so the tree behaves exactly
/// as it does in the sidebar. `None` when the cell is scrolled out of view.
pub fn project_body_rect(
    bounds: RECT,
    dpi: u32,
    sessions: &Sessions,
    scroll_y: i32,
    cols: i32,
    rows: i32,
) -> Option<RECT> {
    let plan = plan(&bounds, dpi, sessions, cols, rows);
    let scroll_y = scroll_y.clamp(0, plan.max_scroll());
    let cell = plan.visible_slot_rect(PROJECT_SLOT, scroll_y)?;
    let body = project_body(&cell, dpi as f64 / 96.0);
    if body.bottom <= body.top {
        return None;
    }
    // Clip to the tile so a half-scrolled cell doesn't hand out rows that
    // are painted outside the grid.
    Some(RECT {
        left: body.left,
        top: body.top.max(plan.layout.inner.top),
        right: body.right,
        bottom: body.bottom.min(plan.layout.inner.bottom),
    })
}

/// Where the tree is drawn inside the project cell. It includes the title
/// row: the tree draws its own header there, with the "New" badge and the
/// filter box, exactly as it does in the sidebar.
fn project_body(cell: &RECT, scale: f64) -> RECT {
    let pad = (CELL_PADDING as f64 * scale).round() as i32;
    RECT {
        left: cell.left + pad,
        top: cell.top + pad / 2,
        right: cell.right - pad,
        bottom: cell.bottom - pad,
    }
}

/// Give every session the cell it occupies, which is what sizes its PTY.
/// Called from `dashboard::layout` before any paint, so the rects a session
/// paints itself into and the ones this module hit-tests always agree.
pub fn assign_bounds(
    bounds: RECT,
    dpi: u32,
    sessions: &mut Sessions,
    scroll_y: i32,
    cols: i32,
    rows: i32,
    font_pt: i32,
) {
    let plan = plan(&bounds, dpi, sessions, cols, rows);
    let scroll_y = scroll_y.clamp(0, plan.max_scroll());
    let cell_pt = cell_font_pt(font_pt);
    for (i, session) in sessions.iter_mut().enumerate() {
        let virt = plan.slot_rect(i + 1);
        let cell = RECT {
            top: virt.top - scroll_y,
            bottom: virt.bottom - scroll_y,
            ..virt
        };
        session.session_view.set_bounds(cell, dpi, cell_pt);
    }
}

/// Where each session sits right now, in `Sessions` order — including the
/// ones that landed there by flowing rather than by being dragged. The panel
/// pins these onto the sessions when a drag starts, so the cells nobody
/// touched hold still while one is moved.
pub fn resolved_placements(
    bounds: RECT,
    dpi: u32,
    sessions: &Sessions,
    cols: i32,
    rows: i32,
) -> Vec<GridPlacement> {
    plan(&bounds, dpi, sessions, cols, rows).slots[1..].to_vec()
}

pub fn paint(
    hdc: HDC,
    bounds: RECT,
    dpi: u32,
    sessions: &Sessions,
    focused: Option<SessionId>,
    scroll_y: i32,
    nav: &NavState<'_>,
    cols: i32,
    rows: i32,
    hovered_control: Option<(SessionId, SlotControl)>,
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

    let cells: Vec<&Session> = sessions.iter().collect();
    let plan = plan(&bounds, dpi, sessions, cols, rows);
    let scroll_y = scroll_y.clamp(0, plan.max_scroll());

    // Clip cell rendering to the inner area so partially-visible rows at
    // the top/bottom of the viewport draw cleanly without overflowing into
    // adjacent tiles.
    let saved = unsafe { SaveDC(hdc) };
    unsafe {
        let _ = IntersectClipRect(
            hdc,
            plan.layout.inner.left,
            plan.layout.inner.top,
            plan.layout.inner.right,
            plan.layout.inner.bottom,
        );
    }

    // Slots run in session order, not top-down — an arranged grid puts them
    // wherever the user dropped them — so every one is considered.
    for slot in 0..plan.slots.len() {
        let Some(cell) = plan.visible_slot_rect(slot, scroll_y) else {
            continue;
        };
        if slot == PROJECT_SLOT {
            paint_project_cell(hdc, cell, dpi, scale, radius, nav, sessions, focused);
            continue;
        }
        // The session paints itself into the bounds `assign_bounds` gave it,
        // which is this same cell: border, name label, close cross, and the
        // live terminal.
        let session = cells[slot - 1];
        let needs_attention = session.status == SessionStatus::NeedsAttention;
        session.session_view.paint(
            hdc,
            dpi,
            ChromeStyle {
                focused: focused == Some(session.id),
                status: Some(status_color(session.status)),
                pulse: needs_attention.then(pulse_level),
                closable: true,
                close_hovered: hovered_control == Some((session.id, SlotControl::FrameClose)),
            },
        );
        // A restored session has no terminal behind that frame — the card
        // fills the cell until the user resumes it.
        if session.is_dormant() {
            // The view's own bounds, not the cell rect — the same source
            // `control_at` hit-tests against, so the buttons can't drift
            // from where they were drawn.
            resume_card::paint(
                hdc,
                session.session_view.bounds(),
                dpi,
                session,
                dashboard::card_hover(hovered_control, session.id),
            );
        }
    }

    unsafe {
        let _ = RestoreDC(hdc, saved);
    }

    paint_scrollbar(hdc, &bounds, &plan, dpi, scroll_y);
}

fn paint_scrollbar(hdc: HDC, bounds: &RECT, plan: &GridPlan, dpi: u32, scroll_y: i32) {
    let Some((track, thumb)) = scrollbar_rects(bounds, plan, dpi, scroll_y) else {
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

/// Rounded background + 1 px border for the project cell. Session cells draw
/// their own chrome as part of [`SessionView::paint`].
fn paint_cell_chrome(hdc: HDC, cell: &RECT, radius: i32, bg: Color) {
    let cell_border = Color::from_hex(CELL_BORDER_HEX);
    unsafe {
        let brush = CreateSolidBrush(COLORREF(bg.to_colorref()));
        let rgn = CreateRoundRectRgn(
            cell.left,
            cell.top,
            cell.right + 1,
            cell.bottom + 1,
            radius * 2,
            radius * 2,
        );
        let _ = FillRgn(hdc, rgn, brush);
        let _ = DeleteObject(rgn);
        let _ = DeleteObject(brush);

        let pen = CreatePen(PS_SOLID, 1, COLORREF(cell_border.to_colorref()));
        let old_pen = SelectObject(hdc, pen);
        let null_brush = GetStockObject(NULL_BRUSH);
        let old_brush = SelectObject(hdc, null_brush);
        let _ = RoundRect(
            hdc,
            cell.left,
            cell.top,
            cell.right,
            cell.bottom,
            radius * 2,
            radius * 2,
        );
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(pen);
    }
}

/// Slot 0: the same project tree the dashboard's sidebar draws — header,
/// `+` buttons and all — so the grid view can start and resume sessions
/// without a sidebar of its own.
fn paint_project_cell(
    hdc: HDC,
    cell: RECT,
    dpi: u32,
    scale: f64,
    radius: i32,
    nav: &NavState<'_>,
    sessions: &Sessions,
    focused: Option<SessionId>,
) {
    paint_cell_chrome(hdc, &cell, radius, Color::from_hex(CELL_BG_HEX));

    let body = project_body(&cell, scale);
    if body.bottom <= body.top {
        return;
    }
    project_tree::paint(hdc, body, dpi, nav, sessions, focused);
}

/// Session whose cell contains `(x, y)`, if any. Used to route wheel events
/// and terminal mouse input to the right terminal.
pub fn session_at(
    x: i32,
    y: i32,
    bounds: RECT,
    dpi: u32,
    sessions: &Sessions,
    scroll_y: i32,
    cols: i32,
    rows: i32,
) -> Option<SessionId> {
    let cells: Vec<&Session> = sessions.iter().collect();
    let plan = plan(&bounds, dpi, sessions, cols, rows);
    let scroll_y = scroll_y.clamp(0, plan.max_scroll());
    if !point_in(&plan.layout.inner, x, y) {
        return None;
    }
    for slot in 1..plan.slots.len() {
        let Some(cell) = plan.visible_slot_rect(slot, scroll_y) else {
            continue;
        };
        if point_in(&cell, x, y) {
            return Some(cells[slot - 1].id);
        }
    }
    None
}

/// A cell's drag handles. The top edge — the strip the name label floats in
/// — moves the conversation; the right and bottom edges stretch it across
/// more cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GridHandle {
    Move,
    ResizeRight,
    ResizeBottom,
    ResizeCorner,
}

impl GridHandle {
    pub fn cursor(self) -> CursorHint {
        match self {
            GridHandle::Move => CursorHint::SizeAll,
            GridHandle::ResizeRight => CursorHint::SizeWE,
            GridHandle::ResizeBottom => CursorHint::SizeNS,
            GridHandle::ResizeCorner => CursorHint::SizeNWSE,
        }
    }
}

/// Which of a cell's handles `(x, y)` lands on, if any.
fn handle_in_cell(cell: &RECT, x: i32, y: i32, scale: f64) -> Option<GridHandle> {
    let resize = (RESIZE_BAND as f64 * scale).round().max(2.0) as i32;
    let move_band = (MOVE_BAND as f64 * scale).round().max(2.0) as i32;
    // The name label straddles the top border, so the move band reaches a
    // little above the frame — grabbing the label has to grab the cell.
    let overhang = move_band / 2;
    if x < cell.left || x >= cell.right || y < cell.top - overhang || y >= cell.bottom {
        return None;
    }
    let right = x >= cell.right - resize;
    let bottom = y >= cell.bottom - resize;
    match (right, bottom) {
        (true, true) => Some(GridHandle::ResizeCorner),
        (true, false) => Some(GridHandle::ResizeRight),
        (false, true) => Some(GridHandle::ResizeBottom),
        (false, false) if y < cell.top + move_band => Some(GridHandle::Move),
        _ => None,
    }
}

/// The move / resize handle under `(x, y)`, and the session it belongs to.
/// The project cell has none — it stays in the corner, which is where the
/// grid's only way to start a conversation belongs.
pub fn handle_at(
    x: i32,
    y: i32,
    bounds: RECT,
    dpi: u32,
    sessions: &Sessions,
    scroll_y: i32,
    cols: i32,
    rows: i32,
) -> Option<(SessionId, GridHandle)> {
    let cells: Vec<&Session> = sessions.iter().collect();
    let plan = plan(&bounds, dpi, sessions, cols, rows);
    let scroll_y = scroll_y.clamp(0, plan.max_scroll());
    if !point_in(&plan.layout.inner, x, y) {
        return None;
    }
    let scale = dpi as f64 / 96.0;
    for slot in 1..plan.slots.len() {
        let Some(cell) = plan.visible_slot_rect(slot, scroll_y) else {
            continue;
        };
        if let Some(handle) = handle_in_cell(&cell, x, y, scale) {
            return Some((cells[slot - 1].id, handle));
        }
    }
    None
}

/// Cell offset between where a move-drag was grabbed and the cell's own
/// origin, so the frame keeps its position under the cursor as it is
/// dragged rather than snapping its corner to the pointer.
pub fn grab_offset(
    x: i32,
    y: i32,
    bounds: RECT,
    dpi: u32,
    sessions: &Sessions,
    scroll_y: i32,
    cols: i32,
    rows: i32,
    id: SessionId,
) -> (i32, i32) {
    let plan = plan(&bounds, dpi, sessions, cols, rows);
    let scroll_y = scroll_y.clamp(0, plan.max_scroll());
    let Some(slot) = session_slot(sessions, id) else {
        return (0, 0);
    };
    let origin = plan.slots[slot];
    let (col, row) = plan.cell_at(x, y + scroll_y);
    (col - origin.col, row - origin.row)
}

/// Index of `id`'s slot in the plan (its session index, offset by the
/// project cell).
fn session_slot(sessions: &Sessions, id: SessionId) -> Option<usize> {
    sessions.iter().position(|s| s.id == id).map(|i| i + 1)
}

/// Where a drag in progress puts its cell, given the cursor at `(x, y)`.
/// `None` when the answer would sit on the project cell — that corner is
/// spoken for, so the frame simply refuses to go there.
pub fn drag_placement(
    x: i32,
    y: i32,
    bounds: RECT,
    dpi: u32,
    sessions: &Sessions,
    scroll_y: i32,
    cols: i32,
    rows: i32,
    id: SessionId,
    handle: GridHandle,
    grab: (i32, i32),
) -> Option<GridPlacement> {
    let plan = plan(&bounds, dpi, sessions, cols, rows);
    let scroll_y = scroll_y.clamp(0, plan.max_scroll());
    let slot = session_slot(sessions, id)?;
    let current = plan.slots[slot];
    let (col, row) = plan.cell_at(x, y + scroll_y);

    let wanted = match handle {
        GridHandle::Move => GridPlacement {
            col: col - grab.0,
            row: row - grab.1,
            ..current
        },
        GridHandle::ResizeRight => GridPlacement {
            cols: col - current.col + 1,
            ..current
        },
        GridHandle::ResizeBottom => GridPlacement {
            rows: row - current.row + 1,
            ..current
        },
        GridHandle::ResizeCorner => GridPlacement {
            cols: col - current.col + 1,
            rows: row - current.row + 1,
            ..current
        },
    }
    .clamped(plan.layout.cols);

    if wanted.overlaps(&plan.slots[PROJECT_SLOT]) {
        return None;
    }
    Some(wanted)
}

/// The control under `(x, y)`, when the point is on a session's own chrome
/// rather than its terminal: the frame's close cross, or — on a restored
/// session — one of the resume card's buttons. The panel keeps this as its
/// hover state, and the click handler resolves it to an action.
pub fn control_at(
    x: i32,
    y: i32,
    bounds: RECT,
    dpi: u32,
    sessions: &Sessions,
    scroll_y: i32,
    cols: i32,
    rows: i32,
) -> Option<(SessionId, SlotControl)> {
    let plan = plan(&bounds, dpi, sessions, cols, rows);
    if !point_in(&plan.layout.inner, x, y) {
        return None;
    }
    // The cross sits *on* the frame, which means half of it hangs above the
    // cell — so it is tested against every session's own bounds rather than
    // through `session_at`. Sessions scrolled out of view keep stale bounds
    // outside `inner`, which the guard above has already excluded.
    for session in sessions.iter() {
        if session.session_view.hits_close(x, y, dpi) {
            return Some((session.id, SlotControl::FrameClose));
        }
    }
    let id = session_at(x, y, bounds, dpi, sessions, scroll_y, cols, rows)?;
    let session = sessions.get(id)?;
    if session.is_dormant() {
        let button = resume_card::button_at(x, y, session.session_view.bounds(), dpi)?;
        return Some((id, button.into()));
    }
    None
}

/// Cursor selection on the grid:
///   * scrollbar thumb, close cross, resume-card button → Hand
///   * a cell's move / resize handles → the matching sizing cursor
///   * inside the project cell → whatever the tree asks for
///   * inside a terminal's text area → IBeam
///   * Arrow otherwise.
pub fn cursor_at(
    x: i32,
    y: i32,
    bounds: RECT,
    dpi: u32,
    sessions: &Sessions,
    scroll_y: i32,
    nav: &NavState<'_>,
    cols: i32,
    rows: i32,
) -> CursorHint {
    let plan = plan(&bounds, dpi, sessions, cols, rows);
    let clamped = scroll_y.clamp(0, plan.max_scroll());
    if let Some((_, thumb)) = scrollbar_rects(&bounds, &plan, dpi, clamped) {
        if point_in(&thumb, x, y) {
            return CursorHint::Hand;
        }
    }
    if !point_in(&plan.layout.inner, x, y) {
        return CursorHint::Arrow;
    }
    if let Some(cell) = plan.visible_slot_rect(PROJECT_SLOT, clamped) {
        if point_in(&cell, x, y) {
            let Some(body) = project_body_rect(bounds, dpi, sessions, scroll_y, cols, rows) else {
                return CursorHint::Arrow;
            };
            return project_tree::cursor_at(x, y, body, dpi, nav, sessions);
        }
    }
    if control_at(x, y, bounds, dpi, sessions, scroll_y, cols, rows).is_some() {
        return CursorHint::Hand;
    }
    if let Some((_, handle)) = handle_at(x, y, bounds, dpi, sessions, scroll_y, cols, rows) {
        return handle.cursor();
    }
    match session_at(x, y, bounds, dpi, sessions, scroll_y, cols, rows) {
        Some(id) => sessions
            .get(id)
            .map(|s| {
                // A dormant cell is a card on an empty frame; there is no
                // text under the cursor to select.
                let term = s.session_view.terminal();
                if s.is_dormant() {
                    CursorHint::Arrow
                } else if term.over_scrollbar(x, y, dpi) {
                    CursorHint::Hand
                } else if point_in(&term.bounds(), x, y) {
                    CursorHint::IBeam
                } else {
                    CursorHint::Arrow
                }
            })
            .unwrap_or(CursorHint::Arrow),
        None => CursorHint::Arrow,
    }
}

/// Outcome of clicking inside the grid tile. The panel layer interprets
/// this — cell clicks become tile actions, scrollbar interactions update
/// the panel's `grid_scroll_y` directly.
pub enum GridClick {
    Action(TileAction),
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
    /// User grabbed a cell's move or resize handle. `grab` is the cell
    /// offset from the pointer to the cell's origin, which a move keeps
    /// constant.
    HandleGrab {
        id: SessionId,
        handle: GridHandle,
        grab: (i32, i32),
    },
    None,
}

pub fn handle_lbutton_down(
    x: i32,
    y: i32,
    bounds: RECT,
    dpi: u32,
    sessions: &Sessions,
    scroll_y: i32,
    nav: &NavState<'_>,
    cols: i32,
    rows: i32,
) -> Option<TileAction> {
    match handle_lbutton_down_ex(x, y, bounds, dpi, sessions, scroll_y, nav, cols, rows) {
        GridClick::Action(action) => Some(action),
        _ => None,
    }
}

/// Extended click handler used by the panel directly (so it can react to
/// scrollbar interactions in addition to cell actions).
pub fn handle_lbutton_down_ex(
    x: i32,
    y: i32,
    bounds: RECT,
    dpi: u32,
    sessions: &Sessions,
    scroll_y: i32,
    nav: &NavState<'_>,
    cols: i32,
    rows: i32,
) -> GridClick {
    let plan = plan(&bounds, dpi, sessions, cols, rows);
    let clamped = scroll_y.clamp(0, plan.max_scroll());
    if let Some((track, thumb)) = scrollbar_rects(&bounds, &plan, dpi, clamped) {
        if point_in(&thumb, x, y) {
            return GridClick::ScrollThumbGrab {
                grab_offset: y - thumb.top,
                bounds,
                content_h: plan.content_h,
            };
        }
        if point_in(&track, x, y) {
            // Page-jump in the direction of the click.
            let visible_h = plan.layout.inner.bottom - plan.layout.inner.top;
            let delta = if y < thumb.top { -visible_h } else { visible_h };
            return GridClick::ScrollPageJump { delta };
        }
    }
    if !point_in(&plan.layout.inner, x, y) {
        return GridClick::None;
    }
    if let Some(cell) = plan.visible_slot_rect(PROJECT_SLOT, clamped) {
        if point_in(&cell, x, y) {
            let Some(body) = project_body_rect(bounds, dpi, sessions, scroll_y, cols, rows) else {
                return GridClick::None;
            };
            return match project_tree::handle_lbutton_down(x, y, body, dpi, nav, sessions) {
                Some(action) => GridClick::Action(action),
                // Clicking the cell's chrome (title, padding) is a no-op
                // rather than a fall-through — the grid owns the whole cell.
                None => GridClick::None,
            };
        }
    }
    // The cell's own controls come before its terminal: the close cross
    // sits on the frame, and a restored session has buttons where the
    // terminal would be.
    if let Some((id, control)) = control_at(x, y, bounds, dpi, sessions, scroll_y, cols, rows) {
        return GridClick::Action(match control {
            SlotControl::Resume => TileAction::ResumeDormant(id),
            SlotControl::Close | SlotControl::FrameClose => TileAction::CloseSession(id),
        });
    }
    // Then the frame's edges — a press there arranges the cell rather than
    // reaching the terminal inside it.
    if let Some((id, handle)) = handle_at(x, y, bounds, dpi, sessions, scroll_y, cols, rows) {
        let grab = grab_offset(x, y, bounds, dpi, sessions, scroll_y, cols, rows, id);
        return GridClick::HandleGrab { id, handle, grab };
    }
    // A click in a terminal cell both selects that session for keyboard
    // input and anchors a selection drag in its grid.
    match session_at(x, y, bounds, dpi, sessions, scroll_y, cols, rows) {
        Some(id) => {
            if let Some(s) = sessions.get(id) {
                // Clicking the body of a dormant cell selects it without
                // starting a selection on a terminal that isn't there.
                if s.is_dormant() {
                    return GridClick::Action(TileAction::FocusSession(id));
                }
                s.session_view.terminal().handle_mouse_down(x, y);
            }
            GridClick::Action(TileAction::StartDrag(id))
        }
        None => GridClick::None,
    }
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
    let gap = (CELL_GAP as f64 * scale).round() as i32;
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

/// Clamp a scroll value against the grid layout for the current session list
/// + tile bounds. Lets the panel apply `delta` from mousewheel events without
/// re-deriving the layout itself.
pub fn clamp_scroll(
    bounds: RECT,
    dpi: u32,
    sessions: &Sessions,
    scroll_y: i32,
    cols: i32,
    rows: i32,
) -> i32 {
    let plan = plan(&bounds, dpi, sessions, cols, rows);
    scroll_y.clamp(0, plan.max_scroll())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile() -> RECT {
        RECT {
            left: 0,
            top: 0,
            right: 900,
            bottom: 600,
        }
    }

    /// `n` sessions, none of them arranged.
    fn flowing(n: usize) -> Vec<Option<GridPlacement>> {
        vec![None; n]
    }

    /// Zooming moves the cells with the main slot, and the defaults are
    /// the sizes the two slots shipped with.
    #[test]
    fn cell_font_follows_the_main_slot() {
        use crate::terminal_view::{FONT_POINT_SIZE, MIN_FONT_POINT_SIZE};
        assert_eq!(cell_font_pt(FONT_POINT_SIZE), 8);
        assert_eq!(cell_font_pt(FONT_POINT_SIZE + 4), 12);
        // The floor moves with the main slot's floor, so the smallest
        // zoom step still leaves the cells legible.
        assert_eq!(
            cell_font_pt(MIN_FONT_POINT_SIZE),
            MIN_FONT_POINT_SIZE - CELL_FONT_OFFSET
        );
    }

    #[test]
    fn cells_fill_the_tile_for_the_chosen_grid() {
        let b = tile();
        let l = compute_grid_layout(&b, 96, 3, 2);
        assert_eq!(l.cols, 3);
        // Three columns + four gaps span the tile minus the scrollbar
        // gutter, give or take the per-cell integer-division remainder.
        let leftover_x = (l.inner.right - l.inner.left) - (3 * l.cell_w + 4 * l.gap);
        let leftover_y = (l.inner.bottom - l.inner.top) - (2 * l.cell_h + 3 * l.gap);
        assert!((0..3).contains(&leftover_x), "leftover_x {leftover_x}");
        assert!((0..2).contains(&leftover_y), "leftover_y {leftover_y}");
    }

    #[test]
    fn extra_cells_flow_into_scrollable_rows() {
        let b = tile();
        // 2x2 asked for, 7 slots to place — the cell size is still the 2x2
        // one, and the overflow becomes scroll.
        let two_by_two = build_plan(&b, 96, &flowing(3), 2, 2);
        let overflowing = build_plan(&b, 96, &flowing(6), 2, 2);
        assert_eq!(overflowing.layout.cell_h, two_by_two.layout.cell_h);
        assert!(overflowing.max_scroll() > 0);
        assert_eq!(two_by_two.max_scroll(), 0);
    }

    #[test]
    fn a_grid_finer_than_the_tile_falls_back_to_minimum_cells() {
        let b = tile();
        let l = compute_grid_layout(&b, 96, MAX_GRID, MAX_GRID);
        assert_eq!(l.cell_w, MIN_CELL_W);
        assert_eq!(l.cell_h, MIN_CELL_H);
    }

    #[test]
    fn grid_dimensions_are_clamped_to_the_pickers_range() {
        let b = tile();
        assert_eq!(compute_grid_layout(&b, 96, 0, 0).cols, 1);
        assert_eq!(compute_grid_layout(&b, 96, 99, 99).cols, MAX_GRID);
    }

    /// Nothing arranged is the layout the grid always had: the project cell
    /// first, then one session per cell, filling rows left to right.
    #[test]
    fn un_arranged_sessions_flow_in_order() {
        let slots = resolve_slots(&flowing(4), 3);
        assert_eq!(slots[0], GridPlacement::cell(0, 0));
        assert_eq!(slots[1], GridPlacement::cell(1, 0));
        assert_eq!(slots[2], GridPlacement::cell(2, 0));
        assert_eq!(slots[3], GridPlacement::cell(0, 1));
        assert_eq!(slots[4], GridPlacement::cell(1, 1));
    }

    /// An arranged session holds its cell, and the rest fill in around it —
    /// including the cells it left behind.
    #[test]
    fn arranged_sessions_hold_their_cell_and_the_rest_flow_around_them() {
        let mut arranged = flowing(3);
        arranged[2] = Some(GridPlacement::cell(1, 0));
        let slots = resolve_slots(&arranged, 3);
        assert_eq!(slots[3], GridPlacement::cell(1, 0), "kept where it was put");
        assert_eq!(slots[1], GridPlacement::cell(2, 0));
        assert_eq!(slots[2], GridPlacement::cell(0, 1));
    }

    /// A stretched cell takes the space it was given, and the flow steps
    /// over it rather than under it.
    #[test]
    fn a_stretched_cell_takes_the_cells_it_spans() {
        let mut arranged = flowing(3);
        arranged[0] = Some(GridPlacement {
            col: 1,
            row: 0,
            cols: 2,
            rows: 2,
        });
        let slots = resolve_slots(&arranged, 3);
        assert_eq!(slots[2], GridPlacement::cell(0, 1));
        assert_eq!(slots[3], GridPlacement::cell(0, 2));

        // …and the content is as tall as the lowest cell reaches.
        let plan = build_plan(&tile(), 96, &arranged, 3, 2);
        let used_rows = 3;
        assert_eq!(
            plan.content_h,
            used_rows * (plan.layout.cell_h + plan.layout.gap) + plan.layout.gap
        );
    }

    /// Narrowing the grid can't leave a cell hanging off the right edge or
    /// wider than the grid itself.
    #[test]
    fn placements_are_pulled_back_onto_a_narrower_grid() {
        let mut arranged = flowing(1);
        arranged[0] = Some(GridPlacement {
            col: 4,
            row: 1,
            cols: 3,
            rows: 1,
        });
        let slots = resolve_slots(&arranged, 2);
        assert_eq!(
            slots[1],
            GridPlacement {
                col: 0,
                row: 1,
                cols: 2,
                rows: 1
            }
        );
    }

    /// Two sessions asking for the same cell would paint on top of each
    /// other; the later one flows instead.
    #[test]
    fn a_colliding_placement_falls_back_to_the_flow() {
        let arranged = vec![
            Some(GridPlacement::cell(1, 0)),
            Some(GridPlacement::cell(1, 0)),
        ];
        let slots = resolve_slots(&arranged, 3);
        assert_eq!(slots[1], GridPlacement::cell(1, 0));
        assert_eq!(slots[2], GridPlacement::cell(2, 0));
    }

    /// Two sessions, laid out over a 3x2 grid: slot 1 and slot 2 flow into
    /// the two cells beside the project cell.
    fn two_sessions() -> Sessions {
        use windows::Win32::Foundation::HWND;
        let mut sessions = Sessions::new();
        for i in 0..2 {
            let view = crate::session_view::SessionView::new_dormant(
                HWND::default(),
                0,
                format!("session {i}"),
                "cmd",
                None,
            );
            sessions.add(format!("session {i}"), view, None, format!("id-{i}"));
        }
        sessions
    }

    /// The centre of the cell at `(col, row)`, in on-screen coordinates.
    fn point_in_cell(plan: &GridPlan, col: i32, row: i32) -> (i32, i32) {
        let l = &plan.layout;
        (
            l.inner.left + l.gap + col * (l.cell_w + l.gap) + l.cell_w / 2,
            l.inner.top + l.gap + row * (l.cell_h + l.gap) + l.cell_h / 2,
        )
    }

    /// Dragging a frame keeps the grabbed point under the cursor: grab a
    /// two-wide cell by its right half and it stays grabbed there, span
    /// intact.
    #[test]
    fn a_move_drag_keeps_its_grab_offset_and_its_span() {
        let b = tile();
        let mut sessions = two_sessions();
        let id = sessions.first_id().unwrap();
        sessions.get_mut(id).unwrap().placement = Some(GridPlacement {
            col: 1,
            row: 0,
            cols: 2,
            rows: 1,
        });

        // Grabbed by its right-hand cell, so the origin trails the pointer
        // by one column the whole way.
        let plan = build_plan(&b, 96, &[sessions.get(id).unwrap().placement, None], 3, 2);
        let (gx, gy) = point_in_cell(&plan, 2, 0);
        let grab = grab_offset(gx, gy, b, 96, &sessions, 0, 3, 2, id);
        assert_eq!(grab, (1, 0));

        let (dx, dy) = point_in_cell(&plan, 1, 1);
        let moved = drag_placement(
            dx,
            dy,
            b,
            96,
            &sessions,
            0,
            3,
            2,
            id,
            GridHandle::Move,
            grab,
        )
        .expect("a free cell accepts the drop");
        assert_eq!(
            moved,
            GridPlacement {
                col: 0,
                row: 1,
                cols: 2,
                rows: 1
            }
        );
    }

    /// Stretching by the bottom-right corner grows the span towards the
    /// cell the pointer is in — the origin doesn't budge.
    #[test]
    fn a_corner_drag_stretches_towards_the_pointer() {
        let b = tile();
        let mut sessions = two_sessions();
        let id = sessions.first_id().unwrap();
        sessions.get_mut(id).unwrap().placement = Some(GridPlacement::cell(1, 0));

        let plan = build_plan(&b, 96, &[sessions.get(id).unwrap().placement, None], 3, 2);
        let (x, y) = point_in_cell(&plan, 2, 1);
        let resized =
            drag_placement(x, y, b, 96, &sessions, 0, 3, 2, id, GridHandle::ResizeCorner, (0, 0))
                .expect("nothing in the way");
        assert_eq!(
            resized,
            GridPlacement {
                col: 1,
                row: 0,
                cols: 2,
                rows: 2
            }
        );
    }

    /// The project cell is how the grid starts conversations; it keeps its
    /// corner, so a frame dragged onto it simply doesn't go.
    #[test]
    fn a_cell_cannot_be_dropped_on_the_project_list() {
        let b = tile();
        let mut sessions = two_sessions();
        let id = sessions.first_id().unwrap();
        sessions.get_mut(id).unwrap().placement = Some(GridPlacement::cell(1, 0));

        let plan = build_plan(&b, 96, &[sessions.get(id).unwrap().placement, None], 3, 2);
        let (x, y) = point_in_cell(&plan, 0, 0);
        assert!(
            drag_placement(x, y, b, 96, &sessions, 0, 3, 2, id, GridHandle::Move, (0, 0)).is_none()
        );
    }

    /// Points left of the first column belong to no cell — truncating
    /// division would fold them onto it.
    #[test]
    fn cells_left_of_the_grid_read_as_negative() {
        assert_eq!(floor_div(-1, 100), -1);
        assert_eq!(floor_div(-100, 100), -1);
        assert_eq!(floor_div(0, 100), 0);
        assert_eq!(floor_div(150, 100), 1);
    }

    /// The pointer's cell is the one the arithmetic must agree with the
    /// painter on, gaps included.
    #[test]
    fn a_point_inside_a_cell_reads_back_as_that_cell() {
        let b = tile();
        let plan = build_plan(&b, 96, &flowing(5), 3, 2);
        for slot in 0..plan.slots.len() {
            let rect = plan.slot_rect(slot);
            let p = plan.slots[slot];
            assert_eq!(
                plan.cell_at(rect.left + 1, rect.top + 1),
                (p.col, p.row),
                "slot {slot}"
            );
        }
    }

    /// The frame's edges arrange the cell; its middle belongs to the
    /// terminal.
    #[test]
    fn the_frames_edges_are_the_drag_handles() {
        let cell = RECT {
            left: 100,
            top: 100,
            right: 300,
            bottom: 260,
        };
        assert_eq!(handle_in_cell(&cell, 200, 102, 1.0), Some(GridHandle::Move));
        // The label floats on the border line, so just above it still grabs.
        assert_eq!(handle_in_cell(&cell, 200, 98, 1.0), Some(GridHandle::Move));
        assert_eq!(
            handle_in_cell(&cell, 297, 180, 1.0),
            Some(GridHandle::ResizeRight)
        );
        assert_eq!(
            handle_in_cell(&cell, 200, 257, 1.0),
            Some(GridHandle::ResizeBottom)
        );
        assert_eq!(
            handle_in_cell(&cell, 297, 257, 1.0),
            Some(GridHandle::ResizeCorner)
        );
        assert_eq!(handle_in_cell(&cell, 200, 180, 1.0), None, "the terminal");
        assert_eq!(handle_in_cell(&cell, 400, 180, 1.0), None, "another cell");
    }
}
