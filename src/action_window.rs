use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS,
    SIGDN_FILESYSPATH,
};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_CONTROL, VK_ESCAPE, VK_F4, VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dashboard::{self, NavState, PanelView};
use crate::grid_tile::{self, MAX_GRID};
use crate::native_interop::{self, Color};
use crate::project_tree::{self, NavTarget};
use crate::projects::{self, Project, ProjectNode};
use crate::session_view::SessionView;
use crate::sessions::{SessionId, SessionStatus, Sessions};
use crate::terminal_view::TerminalView;
use crate::window;

const PANEL_CLASS: &str = "ClaudeManagerPanel";

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

/// Quota bars mirrored from the taskbar widget into the caption strip, to
/// the left of the chrome buttons. Sized to sit level with those buttons
/// rather than to match the widget pixel for pixel.
const QUOTA_SEG_W: i32 = 7;
const QUOTA_SEG_H: i32 = 10;
const QUOTA_SEG_GAP: i32 = 1;
const QUOTA_SEG_COUNT: i32 = 10;
const QUOTA_CORNER: i32 = 2;
const QUOTA_LABEL_GAP: i32 = 5;
const QUOTA_TEXT_GAP: i32 = 6;
const QUOTA_BLOCK_GAP: i32 = 18;
const QUOTA_BUTTON_GAP: i32 = 16;
const QUOTA_FONT_PT: i32 = 8;
const QUOTA_TRACK_HEX: &str = "#45443F";
const QUOTA_FG_HEX: &str = "#8A847C";

const ORANGE_HEX: &str = "#D97757";
const CLAUDE_GREY_HEX: &str = "#262624";

/// Grid-size picker: a `MAX_GRID x MAX_GRID` sheet of cells hanging off the
/// view button. Hovering paints the top-left block the cursor spans; the
/// click commits that block as the grid view's `cols x rows`.
const PICKER_CELL: i32 = 13;
const PICKER_CELL_GAP: i32 = 3;
const PICKER_PAD: i32 = 8;
const PICKER_LABEL_H: i32 = 16;
const PICKER_BG_HEX: &str = "#1F1F1D";
const PICKER_BORDER_HEX: &str = "#3A3A38";
const PICKER_CELL_HEX: &str = "#6C6C64";
const PICKER_CELL_IDLE_HEX: &str = "#333330";
const PICKER_LABEL_FG_HEX: &str = "#7C766F";
const PICKER_LABEL_FONT_PT: i32 = 8;

/// Grid the view starts on before the user has ever picked one. Past that,
/// the panel opens on whatever `window::saved_grid_size` remembers.
pub const DEFAULT_GRID_COLS: i32 = 3;
pub const DEFAULT_GRID_ROWS: i32 = 2;


pub const WM_APP_TERM_OUTPUT: u32 = WM_APP + 100;
/// The folder picker came back with a directory. `lparam` is a leaked
/// `Box<PathBuf>` the handler takes ownership of again.
const WM_APP_FOLDER_PICKED: u32 = WM_APP + 101;

static PANEL_HWND: Mutex<isize> = Mutex::new(0);
static PANEL: Mutex<Option<Panel>> = Mutex::new(None);

/// All panel state lives in one struct so the layout/dispatch pipeline can
/// take a single mutex lock per message.
struct Panel {
    view: PanelView,
    /// Panel-wide "currently selected" session. Persists across view
    /// changes so jumping between the dashboard and the grid keeps the same
    /// session focused.
    focused_session: Option<SessionId>,
    sessions: Sessions,
    /// Latest project scan, pulled from `projects::global()` on the 1 Hz
    /// tick. Ordered newest-active first.
    projects: Vec<Project>,
    /// `projects` joined with the live sessions — exactly what the nav
    /// tree draws. Rebuilt whenever either side changes.
    nav_tree: Vec<ProjectNode>,
    /// Flagged sessions lifted out of `nav_tree` for the nav's "needs
    /// attention" section. Empty when nothing is waiting, which is what
    /// hides the section.
    attention_rows: Vec<projects::AttentionRow>,
    /// Projects the user has opened in the nav, keyed by path so the state
    /// survives the scanner reordering the tree.
    expanded_projects: HashSet<PathBuf>,
    /// Vertical scroll offset of the nav tree, in device pixels.
    nav_scroll_y: i32,
    /// Nav row under the cursor. Drives the row wash and the `+` glyph's
    /// hover colour.
    hovered_nav: Option<NavTarget>,
    /// Cached output of `dashboard::layout` — recomputed when the view
    /// changes or the panel resizes. Used by the input router so it doesn't
    /// have to re-run layout per event.
    layout_cache: Vec<(dashboard::Tile, RECT)>,
    /// Session whose terminal currently owns an in-progress mouse drag.
    dragging_session: Option<SessionId>,
    /// FIFO of sessions whose status flipped to `NeedsAttention`. The front
    /// is what queue mode shows in the main terminal.
    attention_queue: VecDeque<SessionId>,
    /// Vertical scroll offset for the grid view's cells. In *device* pixels,
    /// clamped at paint time.
    grid_scroll_y: i32,
    /// True while the user drags the grid's scrollbar thumb. Holds the
    /// y-pixel offset between the thumb origin and the mouse, plus the
    /// last-seen tile bounds so wheel arithmetic still works mid-drag.
    grid_scroll_drag: Option<GridScrollDrag>,
    /// Present while the user is moving or resizing a grid cell.
    grid_drag: Option<GridDrag>,
    /// Cell size of the grid view, chosen from the view button's right-click
    /// picker.
    grid_cols: i32,
    grid_rows: i32,
    /// Present while the grid-size picker sheet is showing.
    grid_picker: Option<GridPicker>,
    /// True while the animation timer is running. It only runs when the grid
    /// view has something to pulse, so an idle panel repaints once a second
    /// like it always did.
    pulse_timer_on: bool,
    /// The nav header's filter box.
    search: SearchState,
    /// True while the slide animation timer is running.
    search_timer_on: bool,
    /// Session control under the cursor — a cell's close cross, or a resume
    /// card's button. Drives their hover colour.
    hovered_control: Option<(SessionId, dashboard::SlotControl)>,
    /// Point size the terminals render at. Ctrl +/- walks it; the grid's
    /// cells follow one size smaller.
    font_pt: i32,
}

/// Filter box state. `anim` trails `open` for the length of the slide, and
/// `query` is what the tree is narrowed by — cleared the moment the box
/// starts closing, so the tree is whole again before the box is gone.
#[derive(Default)]
struct SearchState {
    open: bool,
    query: String,
    anim: f64,
}

impl SearchState {
    fn nav(&self) -> dashboard::NavSearch<'_> {
        dashboard::NavSearch {
            open: self.open,
            query: &self.query,
            anim: self.anim,
        }
    }

    /// Where the slide is heading.
    fn target(&self) -> f64 {
        if self.open {
            1.0
        } else {
            0.0
        }
    }

    /// Advance one animation frame. Returns `true` while there is more to
    /// do, which is what keeps the timer alive.
    fn step(&mut self) -> bool {
        let target = self.target();
        if (self.anim - target).abs() <= SEARCH_ANIM_STEP {
            self.anim = target;
            return false;
        }
        self.anim += if target > self.anim {
            SEARCH_ANIM_STEP
        } else {
            -SEARCH_ANIM_STEP
        };
        true
    }

    /// Apply one WM_CHAR to the query. Returns `true` if it changed.
    fn type_char(&mut self, code: u32) -> bool {
        match code {
            // Backspace.
            0x08 => self.query.pop().is_some(),
            // Control characters (Enter, Tab, Escape…) aren't text; Escape
            // closes the box from the key handler instead.
            c if c < 0x20 || c == 0x7f => false,
            c => match char::from_u32(c) {
                Some(ch) => {
                    self.query.push(ch);
                    true
                }
                None => false,
            },
        }
    }
}

/// Open state of the grid-size picker.
#[derive(Clone, Copy, Default)]
struct GridPicker {
    /// Block the cursor currently spans, as 1-based `(cols, rows)`.
    hover: Option<(i32, i32)>,
}

#[derive(Clone, Copy)]
struct GridScrollDrag {
    grab_offset: i32,
    bounds: RECT,
    content_h: i32,
}

/// A grid cell being moved or resized. The tile bounds are captured at the
/// press so the same cell arithmetic runs for the whole drag.
#[derive(Clone, Copy)]
struct GridDrag {
    id: SessionId,
    handle: grid_tile::GridHandle,
    bounds: RECT,
    cols: i32,
    rows: i32,
    /// Cell offset from the pointer to the cell's origin, which a move
    /// keeps constant so the frame doesn't jump under the cursor.
    grab: (i32, i32),
}

const TIMER_ATTENTION: usize = 1;
const TIMER_ATTENTION_INTERVAL_MS: u32 = 1_000;
/// Animation tick for the grid's attention pulse. Only runs while the grid
/// view actually has a flagged session on screen.
const TIMER_PULSE: usize = 2;
const TIMER_PULSE_INTERVAL_MS: u32 = 100;
/// Slide of the nav's filter box. Runs only while it is opening or closing.
const TIMER_SEARCH: usize = 3;
const TIMER_SEARCH_INTERVAL_MS: u32 = 16;
/// Fraction of the slide covered per frame — ~9 frames end to end.
const SEARCH_ANIM_STEP: f64 = 0.12;

pub fn register_classes(hinstance: HINSTANCE) {
    unsafe {
        let panel_class = native_interop::wide_str(PANEL_CLASS);
        let panel_wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(panel_wnd_proc),
            hInstance: hinstance,
            // Default to the normal arrow. WM_SETCURSOR overrides per-tile:
            // IBEAM inside the MainTerminal's text area, HAND on chrome
            // buttons and clickable list/card rows.
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(panel_class.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassExW(&panel_wc);
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

        // A real overlapped window, not a bare popup: the shell only offers
        // Snap Layouts, snap-to-edge and Snap Assist grouping to windows
        // that carry WS_THICKFRAME and WS_MAXIMIZEBOX. The whole caption /
        // border is then stripped again in WM_NCCALCSIZE so the panel still
        // paints its own chrome edge to edge.
        let hwnd = match CreateWindowExW(
            WS_EX_APPWINDOW,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
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

        // CreateWindowEx computed the first frame before this window had a
        // proc that strips it, so the caption is still cached. SWP_FRAMECHANGED
        // forces a fresh WM_NCCALCSIZE pass, which is what actually removes it.
        let _ = SetWindowPos(
            hwnd,
            HWND::default(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );

        *PANEL_HWND.lock().unwrap_or_else(|e| e.into_inner()) = hwnd.0 as isize;

        // Restore the workspace: every session the panel had open when it
        // was last closed comes back as a card, none of them running. What
        // to do with them is the user's call, not ours — auto-spawning
        // surprised people who just wanted to glance at what was going on.
        let mut sessions = Sessions::new();
        // A project directory that has since been deleted or unmounted
        // can't be resumed into, so it doesn't come back as a card either.
        let restored: Vec<crate::sessions::SavedSession> = crate::window::saved_sessions()
            .into_iter()
            .filter(|s| s.cwd.is_dir())
            .collect();
        for saved in &restored {
            let view = SessionView::new_dormant(
                hwnd,
                WM_APP_TERM_OUTPUT,
                &saved.name,
                resume_command(&saved.session_id),
                Some(saved.cwd.clone()),
            );
            // Follow the transcript straight away — the card's date and
            // context figures come from it.
            crate::claude_store::track(&saved.session_id);
            let id = sessions.add(
                saved.name.clone(),
                view,
                Some(saved.cwd.clone()),
                saved.session_id.clone(),
            );
            // Back into the cell it was dragged to, at the size it was
            // stretched to. Never arranged means it flows, as it always did.
            if let Some(session) = sessions.get_mut(id) {
                session.placement = saved.placement;
            }
        }

        let view = PanelView::Dashboard { queue_mode: false };

        // The nav is ordered by name, so pick out the most recently used
        // project and start it open: the first screen should show
        // conversations, not a wall of collapsed directories.
        let (grid_cols, grid_rows) = crate::window::saved_grid_size();

        let projects = projects::global().snapshot();
        let mut expanded_projects: HashSet<PathBuf> = projects
            .iter()
            .max_by_key(|p| p.last_active)
            .map(|p| p.path.clone())
            .into_iter()
            .collect();
        // A restored session that isn't on screen may as well not have been
        // restored — open the projects it belongs to.
        expanded_projects.extend(restored.iter().map(|s| s.cwd.clone()));

        let mut panel = Panel {
            view,
            focused_session: None,
            sessions,
            projects,
            nav_tree: Vec::new(),
            attention_rows: Vec::new(),
            expanded_projects,
            nav_scroll_y: 0,
            hovered_nav: None,
            layout_cache: Vec::new(),
            dragging_session: None,
            attention_queue: VecDeque::new(),
            grid_scroll_y: 0,
            grid_scroll_drag: None,
            grid_drag: None,
            grid_cols,
            grid_rows,
            grid_picker: None,
            pulse_timer_on: false,
            search: SearchState::default(),
            search_timer_on: false,
            hovered_control: None,
            font_pt: crate::window::saved_font_pt(),
        };
        panel.recompute_layout(hwnd);

        *PANEL.lock().unwrap_or_else(|e| e.into_inner()) = Some(panel);

        // 1 Hz timer drives the attention heuristic re-evaluation. The
        // heuristic depends on *time since last output* crossing a threshold,
        // which won't generate any PTY traffic on its own.
        SetTimer(
            hwnd,
            TIMER_ATTENTION,
            TIMER_ATTENTION_INTERVAL_MS,
            None,
        );

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(hwnd);
    }
}

impl Panel {
    fn recompute_layout(&mut self, hwnd: HWND) {
        let area = current_sessions_area(hwnd);
        let dpi = unsafe { GetDpiForWindow(hwnd).max(96) };
        let tree = projects::build_tree(&self.projects, &self.sessions);
        // Flagged sessions are read off the whole tree: a filter narrows
        // what you're looking for, it shouldn't hide something asking for
        // you.
        self.attention_rows = projects::attention_rows(&tree, &self.sessions);
        self.nav_tree = projects::filter_tree(tree, &self.search.query);
        self.layout_cache = dashboard::layout(
            &self.view,
            area,
            dpi,
            &mut self.sessions,
            &self.attention_queue,
            self.focused_session,
            self.grid_scroll_y,
            self.grid_cols,
            self.grid_rows,
            self.font_pt,
        );
        self.clamp_nav_scroll(dpi);
        self.sync_pulse_timer(hwnd);
    }

    /// Zoom the terminals. `delta` steps the point size; `None` goes back
    /// to the default. Every session's PTY is resized by the relayout, so
    /// claude reflows to the new column count on its own.
    fn zoom(&mut self, delta: Option<i32>, hwnd: HWND) {
        let target = match delta {
            Some(step) => self.font_pt + step,
            None => crate::terminal_view::FONT_POINT_SIZE,
        };
        let clamped = crate::terminal_view::clamp_font_pt(target);
        if clamped == self.font_pt {
            return;
        }
        self.font_pt = clamped;
        crate::window::set_saved_font_pt(clamped);
        self.recompute_layout(hwnd);
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
    }

    /// Borrowed view of the nav tree's state, handed to the tile for
    /// painting and hit-testing.
    fn nav(&self) -> NavState<'_> {
        NavState {
            tree: &self.nav_tree,
            attention: &self.attention_rows,
            expanded: &self.expanded_projects,
            scroll_y: self.nav_scroll_y,
            hovered: self.hovered_nav,
            search: self.search.nav(),
        }
    }

    /// Where the project tree is drawn in the current layout: its own tile
    /// in the dashboard view, the first grid cell in the grid view. `None`
    /// when the grid has scrolled that cell off-screen.
    fn nav_bounds(&self, dpi: u32) -> Option<RECT> {
        self.layout_cache.iter().find_map(|(tile, rect)| match tile {
            dashboard::Tile::ProjectTree => Some(*rect),
            dashboard::Tile::SessionGrid { scroll_y, cols, rows } => {
                grid_tile::project_body_rect(
                    *rect,
                    dpi,
                    &self.sessions,
                    *scroll_y,
                    *cols,
                    *rows,
                )
                .map(|body| body.bounds)
            }
            _ => None,
        })
    }

    /// Re-clamp the nav scroll offset — collapsing a project or losing a
    /// session can shrink the tree out from under it.
    fn clamp_nav_scroll(&mut self, dpi: u32) {
        let Some(bounds) = self.nav_bounds(dpi) else {
            return;
        };
        let clamped = {
            let nav = self.nav();
            project_tree::clamp_scroll(bounds, dpi, &nav, &self.sessions, self.nav_scroll_y)
        };
        self.nav_scroll_y = clamped;
    }

    /// Pull a fresh project scan. Returns `true` when the set of projects
    /// or their conversations changed, i.e. when the nav needs a repaint.
    fn refresh_projects(&mut self) -> bool {
        let next = projects::global().snapshot();
        if project_shape(&next) == project_shape(&self.projects) {
            return false;
        }
        self.projects = next;
        true
    }

    /// Session that receives keyboard input: whatever the dashboard's main
    /// slot is showing, or — in the grid, where every cell is a live
    /// terminal — the cell the user last clicked.
    fn keyboard_session(&self) -> Option<SessionId> {
        match self.view {
            PanelView::Dashboard { .. } => self
                .layout_cache
                .iter()
                .find_map(|(tile, _)| tile.keyboard_session()),
            PanelView::Grid => self.focused_session.or_else(|| self.sessions.first_id()),
        }
    }

    /// Terminal keystrokes should go to, if there is one.
    fn keyboard_terminal(&self) -> Option<&TerminalView> {
        let id = self.keyboard_session()?;
        self.sessions.get(id).map(|s| s.session_view.terminal())
    }

    /// Terminal of the session currently being mouse-dragged (selection).
    fn dragging_terminal(&self) -> Option<&TerminalView> {
        let id = self.dragging_session?;
        self.sessions.get(id).map(|s| s.session_view.terminal())
    }

    /// Apply a [`TileAction`] returned by a tile's input handler.
    /// `hwnd` is the panel window — needed for spawning new sessions.
    fn apply_tile_action(&mut self, action: dashboard::TileAction, hwnd: HWND) {
        match action {
            dashboard::TileAction::StartDrag(id) => {
                self.dragging_session = Some(id);
                // A press inside a terminal also claims the keyboard — in
                // the grid that's the only way to pick which cell you type
                // into.
                if self.focused_session != Some(id) {
                    self.apply_tile_action(dashboard::TileAction::FocusSession(id), hwnd);
                }
            }
            dashboard::TileAction::FocusSession(id) => {
                self.set_focused_session(Some(id));
                // Selecting a session is looking at it: it leaves the
                // attention queue now rather than on the next recompute.
                self.acknowledge(id);
                self.recompute_statuses();
                self.recompute_layout(hwnd);
                unsafe {
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            dashboard::TileAction::NewSessionIn(cwd) => {
                let session_id = crate::claude::new_session_id();
                let claude_cmd = crate::claude::ClaudeArgs {
                    session_id: Some(session_id.clone()),
                    ..Default::default()
                }
                .build_command_line();
                self.spawn_session(hwnd, &cwd, claude_cmd, session_id);
            }
            dashboard::TileAction::ResumeSession { cwd, session_id } => {
                // Already in the workspace — resuming twice would have two
                // PTYs appending to one transcript. A running one gets
                // focused; a restored one is exactly what this asks for, so
                // it starts.
                let existing = self
                    .sessions
                    .iter()
                    .find(|s| s.session_id == session_id)
                    .map(|s| (s.id, s.is_dormant()));
                match existing {
                    Some((id, true)) => {
                        self.apply_tile_action(dashboard::TileAction::ResumeDormant(id), hwnd);
                        return;
                    }
                    Some((id, false)) => {
                        self.apply_tile_action(dashboard::TileAction::FocusSession(id), hwnd);
                        return;
                    }
                    None => {}
                }
                // claude keeps writing the resumed conversation's own
                // transcript, so the session keeps its original UUID.
                let claude_cmd = resume_command(&session_id);
                self.spawn_session(hwnd, &cwd, claude_cmd, session_id);
            }
            dashboard::TileAction::ToggleProject(path) => {
                if !self.expanded_projects.remove(&path) {
                    self.expanded_projects.insert(path);
                }
                self.recompute_layout(hwnd);
                unsafe {
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            dashboard::TileAction::PickNewProject => {
                // The dialog runs on a thread of its own: it pumps its own
                // modal loop, and this one is holding the panel lock that
                // every message handler needs.
                pick_project_folder(hwnd);
            }
            dashboard::TileAction::ToggleSearch => {
                self.set_search_open(!self.search.open, hwnd);
            }
            dashboard::TileAction::ResumeDormant(id) => {
                let Some(session) = self.sessions.get_mut(id) else {
                    return;
                };
                if !session.is_dormant() {
                    return;
                }
                // The view already carries the `--resume` command line;
                // waking it lets the relayout below spawn the PTY.
                session.session_view.wake();
                session.status_label.clear();
                session.last_acknowledged_ms = crate::terminal::now_ms();
                crate::claude_store::track(&session.session_id);
                self.set_focused_session(Some(id));
                self.recompute_layout(hwnd);
                unsafe {
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            dashboard::TileAction::CloseSession(id) => {
                if !self.sessions.remove(id) {
                    return;
                }
                if self.focused_session == Some(id) {
                    self.focused_session = None;
                }
                self.attention_queue.retain(|q| *q != id);
                if self.hovered_control.map(|(hovered, _)| hovered) == Some(id) {
                    self.hovered_control = None;
                }
                self.save_open_sessions();
                self.recompute_layout(hwnd);
                unsafe {
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
        }
    }

    /// The grid tile as the current layout has it: its rect, scroll offset
    /// and grid size. `None` on the dashboard view, which has no grid.
    fn grid_tile_layout(&self) -> Option<(RECT, i32, i32, i32)> {
        self.layout_cache.iter().find_map(|(tile, rect)| match tile {
            dashboard::Tile::SessionGrid {
                scroll_y,
                cols,
                rows,
            } => Some((*rect, *scroll_y, *cols, *rows)),
            _ => None,
        })
    }

    /// Freeze every conversation in the cell it currently occupies. Called
    /// when a drag starts: without it the un-arranged cells would re-flow
    /// around the one being dragged at every step, and the arrangement the
    /// user ends up looking at wouldn't be the one that gets written down.
    fn pin_placements(&mut self, dpi: u32) {
        let Some((rect, _, cols, rows)) = self.grid_tile_layout() else {
            return;
        };
        let resolved = grid_tile::resolved_placements(rect, dpi, &self.sessions, cols, rows);
        for (session, placement) in self.sessions.iter_mut().zip(resolved) {
            session.placement = Some(placement);
        }
    }

    /// Put a conversation in `placement`. Whatever was already there gives
    /// way: a single cell landing on another single cell trades places with
    /// it, and anything else in the way is bumped back into the flow, which
    /// finds it the first free cell.
    fn set_placement(&mut self, id: SessionId, placement: grid_tile::GridPlacement) {
        let vacated = self.sessions.get(id).and_then(|s| s.placement);
        let displaced: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|s| s.id != id)
            .filter(|s| s.placement.map_or(false, |p| p.overlaps(&placement)))
            .map(|s| s.id)
            .collect();
        let swap = match (displaced.as_slice(), vacated) {
            ([other], Some(vacated)) => self
                .sessions
                .get(*other)
                .filter(|s| s.placement == Some(placement) && vacated.same_size(&placement))
                .map(|_| (*other, vacated)),
            _ => None,
        };
        match swap {
            Some((other, vacated)) => {
                if let Some(s) = self.sessions.get_mut(other) {
                    s.placement = Some(vacated);
                }
            }
            None => {
                for other in displaced {
                    if let Some(s) = self.sessions.get_mut(other) {
                        s.placement = None;
                    }
                }
            }
        }
        if let Some(s) = self.sessions.get_mut(id) {
            s.placement = Some(placement);
        }
    }

    /// Advance a move / resize drag to wherever the cursor now is. Returns
    /// `true` when the cell actually changed, which is what earns a repaint
    /// — most mouse moves land inside the cell the drag is already in.
    fn drag_grid_cell(&mut self, drag: GridDrag, x: i32, y: i32, dpi: u32) -> bool {
        let Some(placement) = grid_tile::drag_placement(
            x,
            y,
            drag.bounds,
            dpi,
            &self.sessions,
            self.grid_scroll_y,
            drag.cols,
            drag.rows,
            drag.id,
            drag.handle,
            drag.grab,
        ) else {
            return false;
        };
        if self.sessions.get(drag.id).and_then(|s| s.placement) == Some(placement) {
            return false;
        }
        self.set_placement(drag.id, placement);
        true
    }

    /// Write the open sessions to settings so the next panel — in this
    /// process or the next one — comes back to the same workspace. Cheap
    /// when nothing changed, which is the common case on the 1 Hz tick.
    fn save_open_sessions(&self) {
        let saved: Vec<crate::sessions::SavedSession> = self
            .sessions
            .iter()
            .filter_map(|s| {
                Some(crate::sessions::SavedSession {
                    session_id: s.session_id.clone(),
                    cwd: s.cwd.clone()?,
                    name: self.conversation_title(s),
                    placement: s.placement,
                })
            })
            .collect();
        crate::window::set_saved_sessions(saved);
    }

    /// The best name we have for a session — see [`crate::sessions::Session::label`].
    fn conversation_title(&self, session: &crate::sessions::Session) -> String {
        let title = transcript_title(&self.projects, &session.session_id);
        session.label(title.as_deref())
    }

    /// Open or close the filter box. Closing drops the query, so the tree
    /// is whole again as the box slides shut.
    fn set_search_open(&mut self, open: bool, hwnd: HWND) {
        if self.search.open == open {
            return;
        }
        self.search.open = open;
        if !open {
            self.search.query.clear();
        }
        self.sync_search_timer(hwnd);
        self.recompute_layout(hwnd);
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
    }

    /// Run the slide timer exactly while the box has ground left to cover.
    fn sync_search_timer(&mut self, hwnd: HWND) {
        let wanted = self.search.anim != self.search.target();
        if wanted == self.search_timer_on {
            return;
        }
        self.search_timer_on = wanted;
        unsafe {
            if wanted {
                SetTimer(hwnd, TIMER_SEARCH, TIMER_SEARCH_INTERVAL_MS, None);
            } else {
                let _ = KillTimer(hwnd, TIMER_SEARCH);
            }
        }
    }

    /// Start a terminal in `cwd`, focus it, and make sure its project is
    /// expanded so the new row is actually on screen. `session_id` is the
    /// UUID the command line pins (fresh via `--session-id`, or the
    /// resumed conversation's own id).
    fn spawn_session(&mut self, hwnd: HWND, cwd: &Path, claude_cmd: String, session_id: String) {
        let name = self.session_name_for(cwd);
        let cwd = cwd.to_path_buf();
        let view = SessionView::new(
            hwnd,
            WM_APP_TERM_OUTPUT,
            &name,
            claude_cmd,
            Some(cwd.clone()),
        );
        // Follow this session's transcript — the store ignores any id it
        // wasn't handed.
        crate::claude_store::track(&session_id);
        let id = self.sessions.add(name, view, Some(cwd.clone()), session_id);
        self.expanded_projects.insert(cwd);
        self.set_focused_session(Some(id));
        self.recompute_layout(hwnd);
        self.save_open_sessions();
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
    }

    /// Label for a session spawned in `cwd`: the project's directory name,
    /// numbered from the second concurrent session onwards. Only used until
    /// the transcript scanner picks up the conversation's opening prompt,
    /// which the nav then shows instead.
    fn session_name_for(&self, cwd: &Path) -> String {
        let base = cwd
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| cwd.to_string_lossy().to_string());
        let existing = self
            .sessions
            .iter()
            .filter(|s| s.cwd.as_deref() == Some(cwd))
            .count();
        if existing == 0 {
            base
        } else {
            format!("{base} ({})", existing + 1)
        }
    }

    fn set_focused_session(&mut self, id: Option<SessionId>) {
        self.focused_session = id;
    }

    fn toggle_queue_mode(&mut self) {
        if let PanelView::Dashboard { queue_mode } = &mut self.view {
            *queue_mode = !*queue_mode;
        }
    }

    /// Ctrl+Tab in the grid: move to the next terminal, taking anything that
    /// needs attention first. Landing on a session acknowledges it — you are
    /// looking at it now — so the flag and the pulse clear, and the next
    /// Ctrl+Tab moves on to whatever is still waiting. With nothing flagged
    /// this is a plain "next cell", wrapping at the end.
    fn cycle_grid_focus(&mut self) -> bool {
        let ids: Vec<SessionId> = self.sessions.iter().map(|s| s.id).collect();
        let Some(next) = next_grid_focus(&self.attention_queue, &ids, self.focused_session) else {
            return false;
        };
        self.focused_session = Some(next);
        self.acknowledge(next);
        true
    }

    /// Bring the focused conversation's cell into view. Ctrl+Tab can land
    /// on a cell the grid has scrolled past, and a focus you can't see
    /// reads as nothing having happened. Returns `true` when the grid
    /// actually moved.
    fn scroll_focus_into_view(&mut self, hwnd: HWND) -> bool {
        let (Some((rect, _, cols, rows)), Some(id)) =
            (self.grid_tile_layout(), self.focused_session)
        else {
            return false;
        };
        let dpi = unsafe { GetDpiForWindow(hwnd).max(96) };
        let target = grid_tile::scroll_to_show(
            rect,
            dpi,
            &self.sessions,
            self.grid_scroll_y,
            cols,
            rows,
            id,
        );
        if target == self.grid_scroll_y {
            return false;
        }
        self.grid_scroll_y = target;
        true
    }

    /// Mark a session as seen: it leaves the attention queue, and its status
    /// won't re-flag until claude reports something newer than this moment.
    fn acknowledge(&mut self, id: SessionId) {
        let now = crate::terminal::now_ms();
        if let Some(s) = self.sessions.get_mut(id) {
            s.last_acknowledged_ms = now;
        }
        self.attention_queue.retain(|q| *q != id);
    }

    /// Cycle through the attention queue. Rotates the current front to the
    /// back (acking it so it doesn't immediately re-flag) and moves
    /// `focused_session` to the new front so the MainTerminal slot updates
    /// in both queue mode (where it reads `queue.front()`) and regular mode
    /// (where it reads `focused_session`). Returns `true` if anything
    /// changed; the caller is expected to relayout + repaint.
    fn cycle_attention_queue(&mut self) -> bool {
        if self.attention_queue.is_empty() {
            return false;
        }
        let now = crate::terminal::now_ms();
        if self.attention_queue.len() >= 2 {
            if let Some(prev) = self.attention_queue.pop_front() {
                if let Some(s) = self.sessions.get_mut(prev) {
                    s.last_acknowledged_ms = now;
                }
                self.attention_queue.push_back(prev);
            }
        }
        if let Some(next) = self.attention_queue.front().copied() {
            self.focused_session = Some(next);
        }
        true
    }

    fn set_view(&mut self, new_view: PanelView) {
        if self.view != new_view {
            self.view = new_view;
        }
    }

    /// Start or stop the pulse animation to match what's on screen. Nothing
    /// pulses outside the grid view, and nothing pulses when no session is
    /// flagged — so the timer only exists while it has work.
    fn sync_pulse_timer(&mut self, hwnd: HWND) {
        let wanted = matches!(self.view, PanelView::Grid)
            && self
                .sessions
                .iter()
                .any(|s| s.status == SessionStatus::NeedsAttention);
        if wanted == self.pulse_timer_on {
            return;
        }
        self.pulse_timer_on = wanted;
        unsafe {
            if wanted {
                SetTimer(hwnd, TIMER_PULSE, TIMER_PULSE_INTERVAL_MS, None);
            } else {
                let _ = KillTimer(hwnd, TIMER_PULSE);
            }
        }
    }

    /// Walk every session, recompute its status from its terminal's
    /// last-output / last-input timestamps and the per-session
    /// `last_acknowledged_ms`, and keep the attention queue in sync.
    /// Returns `true` if any status flipped.
    ///
    /// Acknowledgement is an *event* — clicking a session or tabbing to it
    /// ([`Self::acknowledge`]). Two exceptions, both about what's on screen:
    ///
    /// * The dashboard's plain view fills the panel with one terminal, so the
    ///   session in it can't be flagged while it's the one being read. That
    ///   is a suppression, not an acknowledgement: look away and it raises
    ///   its hand again, because nothing was written down.
    /// * Queue mode drives the main slot off the queue, so its front is
    ///   acknowledged for real — that's what makes the queue advance.
    ///
    /// The grid gets neither. Every cell is on screen there, so "visible"
    /// says nothing about whether the user has looked, and the focused cell
    /// must still be able to flag when claude opens a prompt in it.
    fn recompute_statuses(&mut self) -> bool {
        let now = crate::terminal::now_ms();

        let mut suppress_id: Option<SessionId> = None;
        match self.view {
            PanelView::Dashboard { queue_mode: true } => {
                if let Some(id) = self.attention_queue.front().copied().or(self.focused_session) {
                    if let Some(s) = self.sessions.get_mut(id) {
                        s.last_acknowledged_ms = now;
                    }
                }
            }
            PanelView::Dashboard { .. } => suppress_id = self.focused_session,
            PanelView::Grid => {}
        }

        let mut changed = false;
        let Self {
            sessions,
            attention_queue,
            projects,
            ..
        } = self;
        let store = crate::claude_store::global();
        let agents = crate::agent_state::global();
        for session in sessions.iter_mut() {
            // A restored session has nothing to predict from and nothing to
            // ask for — it sits at its card until the user resumes it.
            if session.is_dormant() {
                continue;
            }
            let term = session.session_view.terminal();
            let last_out = term.last_output_ms();
            let last_in = term.last_input_ms();
            let cursor_visible = term
                .grid_arc()
                .map(|g| g.lock().unwrap_or_else(|p| p.into_inner()).cursor_visible)
                .unwrap_or(true);
            // We always know the session's UUID (set at spawn via
            // `--session-id` or copied from `--resume`), so we can look up
            // the matching jsonl directly. Returns None until claude has
            // written the file (which only happens on the first user
            // message — startup alone doesn't create it).
            let history = store.lookup_by_session_id(&session.session_id);
            let jsonl_mtime_ms = history.as_ref().and_then(|h| {
                h.last_modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_millis() as u64)
            });
            let jsonl_speaker = history.as_ref().map(|h| h.last_speaker);
            session.agent = agents.lookup(&session.session_id);
            // The frame's label follows the same source order as the nav's,
            // so a conversation reads the same wherever it appears.
            let title = transcript_title(projects, &session.session_id);
            let label = session.label(title.as_deref());
            if session.session_view.set_label(label) {
                changed = true;
            }
            let prediction = crate::sessions::predict_status(crate::sessions::StatusInputs {
                last_output_ms: last_out,
                last_input_ms: last_in,
                last_acknowledged_ms: session.last_acknowledged_ms,
                now_ms: now,
                cursor_visible,
                jsonl_mtime_ms,
                jsonl_speaker,
                agent: session.agent.clone(),
            });
            // The session filling the dashboard is being read right now, so
            // it doesn't get to shout. Nothing is recorded, so it flags again
            // the moment the user looks somewhere else.
            let new_status = if Some(session.id) == suppress_id
                && prediction.status == SessionStatus::NeedsAttention
            {
                SessionStatus::Idle
            } else {
                prediction.status
            };
            let label_changed = session.status_label != prediction.label;
            session.status_label = prediction.label.clone();
            let old = session.status;
            if new_status != old {
                if crate::diagnose::is_enabled() {
                    crate::diagnose::log(format!(
                        "status[{}] {:?} -> {:?} {}",
                        session.name, old, new_status, prediction.reason,
                    ));
                }
                session.status = new_status;
                changed = true;
                let id = session.id;
                let was_attention = old == SessionStatus::NeedsAttention;
                let is_attention = new_status == SessionStatus::NeedsAttention;
                if !was_attention && is_attention {
                    if !attention_queue.contains(&id) {
                        attention_queue.push_back(id);
                    }
                } else if was_attention && !is_attention {
                    attention_queue.retain(|q| *q != id);
                }
            } else if label_changed && crate::diagnose::is_enabled() {
                crate::diagnose::log(format!(
                    "status[{}] {} (bucket unchanged: {:?})",
                    session.name, prediction.reason, new_status,
                ));
            }
        }
        changed
    }
}

/// Opening prompt of the conversation with this id, if the transcript
/// scanner has read one. `None` until the user has sent a first message.
fn transcript_title(projects: &[crate::projects::Project], session_id: &str) -> Option<String> {
    projects
        .iter()
        .flat_map(|p| p.sessions.iter())
        .find(|h| h.session_id == session_id)
        .map(|h| h.title.clone())
}

/// Command line that picks a conversation back up where it left off.
fn resume_command(session_id: &str) -> String {
    crate::claude::ClaudeArgs {
        resume: Some(session_id.to_string()),
        ..Default::default()
    }
    .build_command_line()
}

/// Ask the shell for a directory, on a thread of its own, and post the
/// answer back to the panel. Nothing happens if the user cancels.
///
/// It cannot run inline: `IFileDialog::Show` spins its own modal message
/// loop, so the panel would re-enter its window proc while the caller
/// still holds the panel lock, and deadlock on the first repaint.
fn pick_project_folder(hwnd: HWND) {
    let target = hwnd.0 as isize;
    let _ = std::thread::Builder::new()
        .name("folder-picker".into())
        .spawn(move || {
            let owner = HWND(target as *mut _);
            let Some(path) = (unsafe { show_folder_dialog(owner) }) else {
                return;
            };
            let boxed = Box::into_raw(Box::new(path));
            let posted = unsafe {
                PostMessageW(
                    owner,
                    WM_APP_FOLDER_PICKED,
                    WPARAM(0),
                    LPARAM(boxed as isize),
                )
                .is_ok()
            };
            if !posted {
                // The panel is gone; nobody will claim the box.
                drop(unsafe { Box::from_raw(boxed) });
            }
        });
}

/// Shell folder browser. Runs on the picker thread, which is where the
/// apartment it initializes belongs.
unsafe fn show_folder_dialog(owner: HWND) -> Option<PathBuf> {
    if CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_err() {
        return None;
    }
    let picked = (|| {
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog
            .SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST)
            .ok()?;
        let title = native_interop::wide_str("Open project folder");
        let _ = dialog.SetTitle(PCWSTR::from_raw(title.as_ptr()));
        // Cancelling returns an error, which is the common path out of here.
        dialog.Show(owner).ok()?;
        let item = dialog.GetResult().ok()?;
        let wide = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = wide.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(wide.0 as *const _));
        path
    })();
    CoUninitialize();
    picked
}

/// Where Ctrl+Tab goes next in the grid: anything still flagged first (in
/// the order it raised its hand), otherwise the next cell after the focused
/// one, wrapping. Skips the session already focused so a single flagged
/// session doesn't trap the cycle. `None` when there is nowhere to go.
fn next_grid_focus(
    attention_queue: &VecDeque<SessionId>,
    ids: &[SessionId],
    focused: Option<SessionId>,
) -> Option<SessionId> {
    if let Some(id) = attention_queue
        .iter()
        .copied()
        .find(|id| Some(*id) != focused)
    {
        return Some(id);
    }
    if ids.is_empty() {
        return None;
    }
    match focused.and_then(|id| ids.iter().position(|x| *x == id)) {
        Some(i) => Some(ids[(i + 1) % ids.len()]),
        None => Some(ids[0]),
    }
}

/// Change signature of a project scan: which projects exist, in what
/// order, and what each one's conversations are called. Deliberately
/// excludes mtimes — a live session's transcript ticks every second and
/// would have the nav relayout on every scan for no visible difference.
fn project_shape(projects: &[Project]) -> Vec<(&Path, Vec<(&str, &str)>)> {
    projects
        .iter()
        .map(|p| {
            let sessions = p
                .sessions
                .iter()
                .map(|s| (s.session_id.as_str(), s.title.as_str()))
                .collect();
            (p.path.as_path(), sessions)
        })
        .collect()
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

/// Button visuals, ordered right-to-left as [close, maximize, minimize,
/// view]. Each button is `CHROME_BUTTON_SIZE` square in design pixels.
fn chrome_button_rects(client: &RECT, dpi: u32) -> [RECT; 4] {
    let scale = dpi as f64 / 96.0;
    let size = (CHROME_BUTTON_SIZE as f64 * scale).round() as i32;
    let margin = (CHROME_BUTTON_MARGIN as f64 * scale).round() as i32;
    let gap = (CHROME_BUTTON_GAP as f64 * scale).round() as i32;
    let mut right_edge = client.right - margin;
    let top = margin;
    let mut rects = [RECT::default(); 4];
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
/// area is comfortable. Adjacent hit rects abut (no gap, no overlap).
fn chrome_button_hit_rects(client: &RECT, dpi: u32) -> [RECT; 4] {
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

fn view_button_rect(client: &RECT, dpi: u32) -> RECT {
    chrome_button_rects(client, dpi)[3]
}

fn point_in(rect: &RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

/// System sizing cursor for one of the grid handles' hints. The non-sizing
/// hints never reach here — they have cursors of their own.
unsafe fn size_cursor(hint: dashboard::CursorHint) -> HCURSOR {
    let idc = match hint {
        dashboard::CursorHint::SizeAll => IDC_SIZEALL,
        dashboard::CursorHint::SizeWE => IDC_SIZEWE,
        dashboard::CursorHint::SizeNS => IDC_SIZENS,
        _ => IDC_SIZENWSE,
    };
    LoadCursorW(HINSTANCE::default(), idc).unwrap_or_default()
}

/// Pitch of one picker cell — the cell itself plus the gap after it.
fn picker_pitch(dpi: u32) -> i32 {
    let scale = dpi as f64 / 96.0;
    ((PICKER_CELL + PICKER_CELL_GAP) as f64 * scale).round() as i32
}

/// Outer rect of the grid-size picker sheet. It hangs below the view button
/// and extends leftwards, so the whole sheet stays inside the panel however
/// narrow the window is.
fn grid_picker_rect(client: &RECT, dpi: u32) -> RECT {
    let scale = dpi as f64 / 96.0;
    let cell = (PICKER_CELL as f64 * scale).round() as i32;
    let pad = (PICKER_PAD as f64 * scale).round() as i32;
    let label = (PICKER_LABEL_H as f64 * scale).round() as i32;
    let margin = (CHROME_BUTTON_MARGIN as f64 * scale).round() as i32;
    let matrix = (MAX_GRID - 1) * picker_pitch(dpi) + cell;
    let w = matrix + 2 * pad;
    let h = matrix + 2 * pad + label;
    let view = view_button_rect(client, dpi);
    let right = client.right - margin;
    let left = (right - w).max(client.left + margin);
    let top = view.bottom + margin;
    RECT {
        left,
        top,
        right: left + w,
        bottom: top + h,
    }
}

/// The cell under `(x, y)` as a 1-based `(cols, rows)` pair — the same
/// numbers the click commits. Points in the gaps between cells belong to
/// the cell they follow, so the sheet has no dead pixels.
fn grid_picker_cell_at(client: &RECT, dpi: u32, x: i32, y: i32) -> Option<(i32, i32)> {
    let scale = dpi as f64 / 96.0;
    let pad = (PICKER_PAD as f64 * scale).round() as i32;
    let rect = grid_picker_rect(client, dpi);
    let pitch = picker_pitch(dpi).max(1);
    let dx = x - (rect.left + pad);
    let dy = y - (rect.top + pad);
    if dx < 0 || dy < 0 {
        return None;
    }
    let (col, row) = (dx / pitch, dy / pitch);
    if col >= MAX_GRID || row >= MAX_GRID {
        return None;
    }
    Some((col + 1, row + 1))
}

fn paint_grid_picker(
    hdc: HDC,
    client: &RECT,
    dpi: u32,
    picker: &GridPicker,
    cols: i32,
    rows: i32,
) {
    let scale = dpi as f64 / 96.0;
    let rect = grid_picker_rect(client, dpi);
    let cell = (PICKER_CELL as f64 * scale).round() as i32;
    let pad = (PICKER_PAD as f64 * scale).round() as i32;
    let pitch = picker_pitch(dpi);
    let radius = (6.0 * scale).round().max(2.0) as i32;

    // The block the click would commit: what the cursor spans, or the grid
    // already in use when the cursor is off the cells.
    let (lit_cols, lit_rows) = picker.hover.unwrap_or((cols, rows));
    let lit_color = if picker.hover.is_some() {
        Color::from_hex(ORANGE_HEX)
    } else {
        Color::from_hex(PICKER_CELL_HEX)
    };
    let idle_color = Color::from_hex(PICKER_CELL_IDLE_HEX);

    unsafe {
        let bg = CreateSolidBrush(COLORREF(Color::from_hex(PICKER_BG_HEX).to_colorref()));
        let rgn = CreateRoundRectRgn(
            rect.left,
            rect.top,
            rect.right + 1,
            rect.bottom + 1,
            radius * 2,
            radius * 2,
        );
        let _ = FillRgn(hdc, rgn, bg);
        let _ = DeleteObject(rgn);
        let _ = DeleteObject(bg);

        let pen = CreatePen(
            PS_SOLID,
            1,
            COLORREF(Color::from_hex(PICKER_BORDER_HEX).to_colorref()),
        );
        let old_pen = SelectObject(hdc, pen);
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        let _ = RoundRect(
            hdc,
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            radius * 2,
            radius * 2,
        );
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(pen);

        for row in 0..MAX_GRID {
            for col in 0..MAX_GRID {
                let lit = col < lit_cols && row < lit_rows;
                let color = if lit { lit_color } else { idle_color };
                let cell_rect = RECT {
                    left: rect.left + pad + col * pitch,
                    top: rect.top + pad + row * pitch,
                    right: rect.left + pad + col * pitch + cell,
                    bottom: rect.top + pad + row * pitch + cell,
                };
                let brush = CreateSolidBrush(COLORREF(color.to_colorref()));
                FillRect(hdc, &cell_rect, brush);
                let _ = DeleteObject(brush);
            }
        }

        let face = native_interop::wide_str("Segoe UI");
        let font = CreateFontW(
            -(PICKER_LABEL_FONT_PT * dpi as i32 / 72),
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
        );
        let old_font = SelectObject(hdc, font);
        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(
            hdc,
            COLORREF(Color::from_hex(PICKER_LABEL_FG_HEX).to_colorref()),
        );
        let mut label: Vec<u16> = format!("{lit_cols} \u{00d7} {lit_rows}")
            .encode_utf16()
            .collect();
        let mut label_rect = RECT {
            left: rect.left + pad,
            top: rect.bottom - (PICKER_LABEL_H as f64 * scale).round() as i32 - pad / 2,
            right: rect.right - pad,
            bottom: rect.bottom - pad / 2,
        };
        let _ = DrawTextW(
            hdc,
            &mut label,
            &mut label_rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        SelectObject(hdc, old_font);
        let _ = DeleteObject(font);
    }
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

unsafe extern "system" fn panel_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        // Swallow the entire non-client area so the client rect covers the
        // whole window and our own chrome is all that shows. A maximized
        // window's rect overhangs the work area by the frame thickness on
        // every side, so inset by it there or the edges fall off-screen.
        WM_NCCALCSIZE if wparam.0 != 0 => {
            let params = &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS);
            if IsZoomed(hwnd).as_bool() {
                let dpi = GetDpiForWindow(hwnd).max(96);
                let fx = GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi)
                    + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
                let fy = GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi)
                    + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
                params.rgrc[0].left += fx;
                params.rgrc[0].right -= fx;
                params.rgrc[0].top += fy;
                params.rgrc[0].bottom -= fy;
            }
            LRESULT(0)
        }
        // DefWindowProc paints the system caption here on every activation
        // change, straight over our client area. Claiming the message keeps
        // the panel's own chrome; TRUE is the "frame is active" reply the
        // shell expects.
        WM_NCACTIVATE => LRESULT(1),
        WM_NCHITTEST => {
            let xs = (lparam.0 & 0xFFFF) as i16 as i32;
            let ys = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut pt = POINT { x: xs, y: ys };
            let _ = ScreenToClient(hwnd, &mut pt);
            let mut client = RECT::default();
            let _ = GetClientRect(hwnd, &mut client);
            let dpi = GetDpiForWindow(hwnd).max(96);

            // WM_NCCALCSIZE removed the frame, so the resize borders have to
            // be reported by hand — otherwise WS_THICKFRAME is inert and the
            // panel can only be resized by the shell's snap gestures.
            if !IsZoomed(hwnd).as_bool() {
                let border = GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi)
                    + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
                let left = pt.x < client.left + border;
                let right = pt.x >= client.right - border;
                let top = pt.y < client.top + border;
                let bottom = pt.y >= client.bottom - border;
                let hit = match (top, bottom, left, right) {
                    (true, _, true, _) => Some(HTTOPLEFT),
                    (true, _, _, true) => Some(HTTOPRIGHT),
                    (_, true, true, _) => Some(HTBOTTOMLEFT),
                    (_, true, _, true) => Some(HTBOTTOMRIGHT),
                    (true, ..) => Some(HTTOP),
                    (_, true, ..) => Some(HTBOTTOM),
                    (_, _, true, _) => Some(HTLEFT),
                    (_, _, _, true) => Some(HTRIGHT),
                    _ => None,
                };
                if let Some(hit) = hit {
                    return LRESULT(hit as isize);
                }
            }

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
            let hit = (lparam.0 & 0xFFFF) as u16;
            if hit == HTCLIENT as u16 {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = ScreenToClient(hwnd, &mut pt);
                let mut client = RECT::default();
                let _ = GetClientRect(hwnd, &mut client);
                let dpi = GetDpiForWindow(hwnd).max(96);

                // Chrome buttons take precedence — they sit above the
                // tile area in the caption strip.
                for r in chrome_button_hit_rects(&client, dpi).iter() {
                    if point_in(r, pt.x, pt.y) {
                        let cursor =
                            LoadCursorW(HINSTANCE::default(), IDC_HAND).unwrap_or_default();
                        SetCursor(cursor);
                        return LRESULT(1);
                    }
                }

                // Per-tile cursor.
                let panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(panel) = panel_guard.as_ref() {
                    // A cell being dragged snaps a whole cell at a time, so
                    // the pointer spends most of the drag in the middle of
                    // the frame rather than on the edge it grabbed. The
                    // cursor stays with the gesture until it is let go.
                    if let Some(drag) = panel.grid_drag {
                        let cursor = size_cursor(drag.handle.cursor());
                        SetCursor(cursor);
                        return LRESULT(1);
                    }
                    for (tile, rect) in &panel.layout_cache {
                        if !point_in(rect, pt.x, pt.y) {
                            continue;
                        }
                        let hint =
                            tile.cursor_at(pt.x, pt.y, *rect, dpi, &panel.sessions, &panel.nav());
                        let cursor = match hint {
                            // The system I-beam, recoloured: as shipped it is
                            // black on a near-black terminal.
                            // Per monitor: `dpi` is this window's current one,
                            // so dragging the panel to a differently scaled
                            // screen picks up that screen's cursor.
                            dashboard::CursorHint::IBeam => {
                                native_interop::light_ibeam_cursor(dpi)
                            }
                            dashboard::CursorHint::Hand => {
                                LoadCursorW(HINSTANCE::default(), IDC_HAND).unwrap_or_default()
                            }
                            dashboard::CursorHint::Arrow => {
                                LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default()
                            }
                            // A grid cell's own edges: the top strip moves
                            // it, the right / bottom ones stretch it.
                            dashboard::CursorHint::SizeAll
                            | dashboard::CursorHint::SizeWE
                            | dashboard::CursorHint::SizeNS
                            | dashboard::CursorHint::SizeNWSE => size_cursor(hint),
                            dashboard::CursorHint::Default => {
                                break;
                            }
                        };
                        SetCursor(cursor);
                        return LRESULT(1);
                    }
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        // Right-clicking the view button opens the grid-size picker; a
        // right-click anywhere else dismisses it.
        WM_RBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut client = RECT::default();
            let _ = GetClientRect(hwnd, &mut client);
            let dpi = GetDpiForWindow(hwnd).max(96);
            let on_view = point_in(&chrome_button_hit_rects(&client, dpi)[3], x, y);
            let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(panel) = panel_guard.as_mut() {
                panel.grid_picker = if on_view && panel.grid_picker.is_none() {
                    Some(GridPicker::default())
                } else {
                    None
                };
            }
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let _ = SetFocus(hwnd);
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut client = RECT::default();
            let _ = GetClientRect(hwnd, &mut client);
            let dpi = GetDpiForWindow(hwnd).max(96);

            // While the picker is open it owns every click: on a cell to
            // commit that grid, anywhere else to dismiss.
            {
                let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(panel) = panel_guard.as_mut() {
                    if panel.grid_picker.is_some() {
                        panel.grid_picker = None;
                        if let Some((cols, rows)) = grid_picker_cell_at(&client, dpi, x, y) {
                            panel.grid_cols = cols;
                            panel.grid_rows = rows;
                            crate::window::set_saved_grid_size(cols, rows);
                            // Picking a size is a request to see it.
                            panel.set_view(PanelView::Grid);
                        }
                        panel.recompute_layout(hwnd);
                        let _ = InvalidateRect(hwnd, None, false);
                        return LRESULT(0);
                    }
                }
            }

            let [close_hit, maximize_hit, minimize_hit, view_hit] =
                chrome_button_hit_rects(&client, dpi);
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
            if point_in(&view_hit, x, y) {
                cycle_view(hwnd);
                return LRESULT(0);
            }
            // Route to whichever tile contains the click. The grid tile is
            // special-cased so we can intercept scrollbar interactions.
            let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(panel) = panel_guard.as_mut() {
                let hit = panel
                    .layout_cache
                    .iter()
                    .find(|(_, r)| point_in(r, x, y))
                    .map(|(tile, rect)| (tile.clone(), *rect));
                if let Some((tile, rect)) = hit {
                    match tile {
                        dashboard::Tile::SessionGrid { scroll_y, cols, rows } => {
                            let click = grid_tile::handle_lbutton_down_ex(
                                x,
                                y,
                                rect,
                                dpi,
                                &panel.sessions,
                                scroll_y,
                                &panel.nav(),
                                cols,
                                rows,
                            );
                            match click {
                                grid_tile::GridClick::Action(action) => {
                                    panel.apply_tile_action(action, hwnd);
                                }
                                grid_tile::GridClick::ScrollThumbGrab {
                                    grab_offset,
                                    bounds,
                                    content_h,
                                } => {
                                    panel.grid_scroll_drag = Some(GridScrollDrag {
                                        grab_offset,
                                        bounds,
                                        content_h,
                                    });
                                    let _ = SetCapture(hwnd);
                                }
                                grid_tile::GridClick::HandleGrab { id, handle, grab } => {
                                    // Everything holds still for the length
                                    // of the drag, so only what the user
                                    // pushes out of the way actually moves.
                                    panel.pin_placements(dpi);
                                    panel.grid_drag = Some(GridDrag {
                                        id,
                                        handle,
                                        bounds: rect,
                                        cols,
                                        rows,
                                        grab,
                                    });
                                    let _ = SetCapture(hwnd);
                                    panel.apply_tile_action(
                                        dashboard::TileAction::FocusSession(id),
                                        hwnd,
                                    );
                                }
                                grid_tile::GridClick::ScrollPageJump { delta } => {
                                    panel.grid_scroll_y = grid_tile::clamp_scroll(
                                        rect,
                                        dpi,
                                        &panel.sessions,
                                        panel.grid_scroll_y + delta,
                                        cols,
                                        rows,
                                    );
                                    panel.recompute_layout(hwnd);
                                    let _ = InvalidateRect(hwnd, None, false);
                                }
                                grid_tile::GridClick::None => {}
                            }
                        }
                        _ => {
                            let action = tile.handle_lbutton_down(
                                x,
                                y,
                                rect,
                                dpi,
                                &panel.sessions,
                                &panel.nav(),
                            );
                            if let Some(action) = action {
                                panel.apply_tile_action(action, hwnd);
                            }
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(panel) = panel_guard.as_mut() {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;

                // The open picker swallows hover: everything underneath it
                // is obscured anyway.
                if let Some(picker) = panel.grid_picker.as_mut() {
                    let mut client = RECT::default();
                    let _ = GetClientRect(hwnd, &mut client);
                    let dpi = GetDpiForWindow(hwnd).max(96);
                    let hover = grid_picker_cell_at(&client, dpi, x, y);
                    if hover != picker.hover {
                        picker.hover = hover;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    return LRESULT(0);
                }

                if let Some(drag) = panel.grid_drag {
                    let dpi = GetDpiForWindow(hwnd).max(96);
                    if panel.drag_grid_cell(drag, x, y, dpi) {
                        panel.recompute_layout(hwnd);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    return LRESULT(0);
                }

                if let Some(drag) = panel.grid_scroll_drag {
                    let dpi = GetDpiForWindow(hwnd).max(96);
                    let new_scroll = grid_tile::scroll_y_from_drag(
                        drag.bounds,
                        dpi,
                        drag.content_h,
                        y,
                        drag.grab_offset,
                    );
                    if new_scroll != panel.grid_scroll_y {
                        panel.grid_scroll_y = new_scroll;
                        panel.recompute_layout(hwnd);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                } else if let Some(term) = panel.dragging_terminal() {
                    term.handle_mouse_move(x, y);
                }

                // Nav hover: the tree is a tile of its own on the dashboard
                // and the first cell of the grid, and drives the row wash
                // plus the `+` glyph colour in both.
                let dpi = GetDpiForWindow(hwnd).max(96);
                let mut nav_hit: Option<NavTarget> = None;
                for (tile, rect) in &panel.layout_cache {
                    if !point_in(rect, x, y) {
                        continue;
                    }
                    let tree_rect = match tile {
                        dashboard::Tile::SessionGrid { scroll_y, cols, rows } => {
                            grid_tile::project_body_rect(
                                *rect,
                                dpi,
                                &panel.sessions,
                                *scroll_y,
                                *cols,
                                *rows,
                            )
                            .filter(|body| point_in(&body.visible, x, y))
                            .map(|body| body.bounds)
                        }
                        dashboard::Tile::ProjectTree => Some(*rect),
                        _ => None,
                    };
                    if let Some(body) = tree_rect {
                        nav_hit =
                            project_tree::target_at(x, y, body, dpi, &panel.nav(), &panel.sessions);
                    }
                    break;
                }
                // Session controls: the close cross on a grid cell's frame,
                // and a restored session's Resume / Close buttons.
                let mut control_hit: Option<(SessionId, dashboard::SlotControl)> = None;
                for (tile, rect) in &panel.layout_cache {
                    if !point_in(rect, x, y) {
                        continue;
                    }
                    control_hit = match tile {
                        dashboard::Tile::SessionGrid { scroll_y, cols, rows } => {
                            grid_tile::control_at(
                                x,
                                y,
                                *rect,
                                dpi,
                                &panel.sessions,
                                *scroll_y,
                                *cols,
                                *rows,
                            )
                        }
                        dashboard::Tile::MainTerminal {
                            session_id: Some(id),
                            ..
                        } => panel
                            .sessions
                            .get(*id)
                            .filter(|s| s.is_dormant())
                            .and_then(|s| {
                                crate::resume_card::button_at(
                                    x,
                                    y,
                                    s.session_view.bounds(),
                                    dpi,
                                )
                            })
                            .map(|button| (*id, button.into())),
                        _ => None,
                    };
                    break;
                }
                if control_hit != panel.hovered_control {
                    panel.hovered_control = control_hit;
                    let _ = InvalidateRect(hwnd, None, false);
                }

                if nav_hit != panel.hovered_nav {
                    panel.hovered_nav = nav_hit;
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(panel) = panel_guard.as_mut() {
                if panel.grid_scroll_drag.take().is_some() {
                    let _ = ReleaseCapture();
                }
                if panel.grid_drag.take().is_some() {
                    let _ = ReleaseCapture();
                    // The arrangement is the user's, so it outlives the
                    // panel from the moment they let go of it.
                    panel.save_open_sessions();
                }
                if let Some(term) = panel.dragging_terminal() {
                    term.handle_mouse_up();
                }
                panel.dragging_session = None;
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            // wparam high word = wheel delta (signed), positive scrolls up.
            let delta = ((wparam.0 >> 16) as i16) as i32;
            // Low word holds the mouse/modifier flags; MK_SHIFT = 0x0004.
            // Shift makes the wheel address the grid itself rather than
            // whatever cell sits under the cursor.
            let shift = (wparam.0 & 0x0004) != 0;
            // Screen coords come in lparam; convert to client to find the
            // tile under the cursor.
            let mut pt = POINT {
                x: (lparam.0 & 0xFFFF) as i16 as i32,
                y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
            };
            let _ = ScreenToClient(hwnd, &mut pt);
            let dpi = GetDpiForWindow(hwnd).max(96);
            let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(panel) = panel_guard.as_mut() {
                let hit = panel
                    .layout_cache
                    .iter()
                    .find(|(_, r)| point_in(r, pt.x, pt.y))
                    .map(|(tile, rect)| (tile.clone(), *rect));
                match hit {
                    Some((tile @ dashboard::Tile::MainTerminal { .. }, rect)) => {
                        // Forward to the terminal as xterm mouse-wheel events
                        // when the running TUI has enabled mouse tracking.
                        // If it hasn't, we deliberately swallow the wheel
                        // silently — sending arrow keys would trip Claude
                        // Code's `arrow-burst` warning, and letting the
                        // message fall through invites touchpad drivers to
                        // synthesize their own VK_UP/VK_DOWN fallback.
                        let notches = delta / 120;
                        if let Some(term) = tile
                            .session_at(pt.x, pt.y, rect, dpi, &panel.sessions)
                            .and_then(|id| panel.sessions.get(id))
                            .map(|s| s.session_view.terminal())
                        {
                            let _ = term.handle_wheel(notches, pt.x, pt.y);
                        }
                    }
                    Some((dashboard::Tile::ProjectTree, rect)) => {
                        let scale = dpi as f64 / 96.0;
                        let line_px = (30.0 * scale).round() as i32;
                        let scroll_delta = -(delta * line_px) / 120;
                        let target = panel.nav_scroll_y + scroll_delta;
                        let new_scroll = {
                            let nav = panel.nav();
                            project_tree::clamp_scroll(
                                rect,
                                dpi,
                                &nav,
                                &panel.sessions,
                                target,
                            )
                        };
                        if new_scroll != panel.nav_scroll_y {
                            panel.nav_scroll_y = new_scroll;
                            let _ = InvalidateRect(hwnd, None, false);
                        }
                    }
                    Some((dashboard::Tile::SessionGrid { scroll_y, cols, rows }, rect)) => {
                        let scale = dpi as f64 / 96.0;
                        // Over the project cell the wheel belongs to the
                        // tree, exactly as it does in the sidebar.
                        let over_tree = grid_tile::project_body_rect(
                            rect,
                            dpi,
                            &panel.sessions,
                            scroll_y,
                            cols,
                            rows,
                        )
                        .filter(|body| point_in(&body.visible, pt.x, pt.y))
                        .map(|body| body.bounds);
                        if let Some(body) = over_tree.filter(|_| !shift) {
                            let line_px = (30.0 * scale).round() as i32;
                            let target = panel.nav_scroll_y - (delta * line_px) / 120;
                            let new_scroll = {
                                let nav = panel.nav();
                                project_tree::clamp_scroll(
                                    body,
                                    dpi,
                                    &nav,
                                    &panel.sessions,
                                    target,
                                )
                            };
                            if new_scroll != panel.nav_scroll_y {
                                panel.nav_scroll_y = new_scroll;
                                let _ = InvalidateRect(hwnd, None, false);
                            }
                            return LRESULT(0);
                        }
                        // Over a terminal cell the notch is that session's:
                        // its scrollback, or an xterm wheel event for a TUI
                        // that asked for mouse tracking. Only when the
                        // terminal declines does the wheel scroll the grid.
                        let over_terminal = grid_tile::session_at(
                            pt.x,
                            pt.y,
                            rect,
                            dpi,
                            &panel.sessions,
                            scroll_y,
                            cols,
                            rows,
                        )
                        .and_then(|id| panel.sessions.get(id));
                        if let Some(session) = over_terminal.filter(|_| !shift) {
                            let notches = delta / 120;
                            if session
                                .session_view
                                .terminal()
                                .handle_wheel(notches, pt.x, pt.y)
                            {
                                return LRESULT(0);
                            }
                        }
                        // 120 = WHEEL_DELTA. Translate to ~3 lines, ~cell_h /
                        // 4 per notch, but scaled by DPI.
                        let line_px = (40.0 * scale).round() as i32;
                        let scroll_delta = -(delta * line_px) / 120;
                        let new_scroll = grid_tile::clamp_scroll(
                            rect,
                            dpi,
                            &panel.sessions,
                            panel.grid_scroll_y + scroll_delta,
                            cols,
                            rows,
                        );
                        if new_scroll != panel.grid_scroll_y {
                            panel.grid_scroll_y = new_scroll;
                            panel.recompute_layout(hwnd);
                            let _ = InvalidateRect(hwnd, None, false);
                        }
                    }
                    _ => {}
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
                let panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(panel) = panel_guard.as_ref() {
                    let nav = panel.nav();
                    for (tile, rect) in &panel.layout_cache {
                        tile.paint(
                            mem_dc,
                            *rect,
                            dpi,
                            &panel.sessions,
                            panel.focused_session,
                            &nav,
                            panel.hovered_control,
                        );
                    }
                }
            }
            paint_chrome_buttons(mem_dc, &client, dpi, hwnd);
            paint_quota_strip(mem_dc, &client, dpi);
            {
                let panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(panel) = panel_guard.as_ref() {
                    if let Some(picker) = panel.grid_picker.as_ref() {
                        paint_grid_picker(
                            mem_dc,
                            &client,
                            dpi,
                            picker,
                            panel.grid_cols,
                            panel.grid_rows,
                        );
                    }
                }
            }

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
            let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(panel) = panel_guard.as_mut() {
                // While the filter box is open it owns the keyboard — that's
                // what makes it a text field rather than a decoration.
                if panel.search.open {
                    if panel.search.type_char(code) {
                        panel.recompute_layout(hwnd);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    return LRESULT(0);
                }
                if let Some(term) = panel.keyboard_terminal() {
                    term.handle_char(code);
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let vk = wparam.0 as u32;
            let alt_held = msg == WM_SYSKEYDOWN;
            // Alt+F4 and Alt+Space are the shell's, not the session's.
            if alt_held && (vk == VK_F4.0 as u32 || vk == VK_SPACE.0 as u32) {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            let ctrl_held = (GetKeyState(VK_CONTROL.0 as i32) as i16) < 0;
            // Ctrl+Q toggles queue mode for the dashboard's main terminal.
            // 0x51 is the virtual-key code for the 'Q' letter.
            if ctrl_held && vk == 0x51 {
                let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(panel) = panel_guard.as_mut() {
                    panel.toggle_queue_mode();
                    panel.recompute_layout(hwnd);
                }
                let _ = InvalidateRect(hwnd, None, false);
                return LRESULT(0);
            }
            // Ctrl+Tab. In the grid it walks to the next terminal, taking
            // anything that needs attention first and acknowledging whatever
            // it lands on. On the dashboard it rotates the attention queue
            // through the single main slot. 0x09 is VK_TAB.
            if ctrl_held && vk == 0x09 {
                let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(panel) = panel_guard.as_mut() {
                    let queue_len = panel.attention_queue.len();
                    let advanced = match panel.view {
                        PanelView::Grid => {
                            let advanced = panel.cycle_grid_focus();
                            if advanced {
                                panel.scroll_focus_into_view(hwnd);
                            }
                            advanced
                        }
                        PanelView::Dashboard { .. } => panel.cycle_attention_queue(),
                    };
                    if crate::diagnose::is_enabled() {
                        crate::diagnose::log(format!(
                            "ctrl+tab: queue_len={queue_len} advanced={advanced} focused={:?}",
                            panel.focused_session,
                        ));
                    }
                    if advanced {
                        panel.recompute_layout(hwnd);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
                return LRESULT(0);
            }
            // Ctrl +/- zooms the terminals, Ctrl+0 resets. Both the main
            // row and the numpad, since terminals are the one place people
            // reach for either. These are claimed before the session sees
            // them — no TUI expects them, and every terminal zooms this way.
            if ctrl_held {
                let step = match vk {
                    // VK_OEM_PLUS / VK_ADD, VK_OEM_MINUS / VK_SUBTRACT.
                    0xBB | 0x6B => Some(Some(1)),
                    0xBD | 0x6D => Some(Some(-1)),
                    // '0' and numpad 0 — back to the default size.
                    0x30 | 0x60 => Some(None),
                    _ => None,
                };
                if let Some(delta) = step {
                    let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(panel) = panel_guard.as_mut() {
                        panel.zoom(delta, hwnd);
                    }
                    return LRESULT(0);
                }
            }
            // Escape dismisses the grid-size picker, then the filter box,
            // before the key reaches any session.
            if vk == VK_ESCAPE.0 as u32 {
                let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(panel) = panel_guard.as_mut() {
                    if panel.grid_picker.take().is_some() {
                        let _ = InvalidateRect(hwnd, None, false);
                        return LRESULT(0);
                    }
                    if panel.search.open {
                        panel.set_search_open(false, hwnd);
                        return LRESULT(0);
                    }
                }
            }
            // Ctrl+1 / Ctrl+2 switch between the two panel views. (0x31 /
            // 0x32 are the virtual-key codes for the digit keys.)
            if ctrl_held && (0x31..=0x32).contains(&vk) {
                let view = if vk == 0x31 {
                    PanelView::Dashboard { queue_mode: false }
                } else {
                    PanelView::Grid
                };
                switch_view(hwnd, view);
                return LRESULT(0);
            }
            let panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(panel) = panel_guard.as_ref() {
                // The filter box has the keyboard: its text arrives as
                // WM_CHAR, and nothing else may reach a session — a
                // backspace meant for the box must not edit claude's prompt.
                if panel.search.open {
                    return LRESULT(0);
                }
                if let Some(term) = panel.keyboard_terminal() {
                    if term.handle_key_down(vk, alt_held) {
                        return LRESULT(0);
                    }
                }
            }
            drop(panel_guard);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        // Alt + a printable key. Windows sends these to the menu loop rather
        // than as WM_CHAR, so DefWindowProc would beep and drop them; hand
        // them to the session as a meta sequence instead.
        WM_SYSCHAR => {
            let code = wparam.0 as u32;
            let panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(panel) = panel_guard.as_ref() {
                if let Some(term) = panel.keyboard_terminal() {
                    if term.handle_alt_char(code) {
                        return LRESULT(0);
                    }
                }
            }
            drop(panel_guard);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        m if m == WM_APP_TERM_OUTPUT => {
            // A session emitted output. Clear paint-pending on every session
            // (cheap if already false) and invalidate.
            let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(panel) = panel_guard.as_mut() {
                for s in panel.sessions.iter() {
                    s.session_view.terminal().clear_paint_pending();
                }
                // Output may have flipped a session out of NeedsAttention or
                // into Working — keep the queue and indicators in sync.
                if panel.recompute_statuses() {
                    panel.recompute_layout(hwnd);
                }
                panel.sync_pulse_timer(hwnd);
            }
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        m if m == WM_APP_FOLDER_PICKED => {
            if lparam.0 == 0 {
                return LRESULT(0);
            }
            let path = *Box::from_raw(lparam.0 as *mut PathBuf);
            let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(panel) = panel_guard.as_mut() {
                panel.apply_tile_action(dashboard::TileAction::NewSessionIn(path), hwnd);
            }
            LRESULT(0)
        }
        WM_SIZE => {
            let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(panel) = panel_guard.as_mut() {
                panel.recompute_layout(hwnd);
            }
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_ATTENTION {
                let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(panel) = panel_guard.as_mut() {
                    // The project scanner runs on its own thread; pick up
                    // whatever it found since the last tick. A new project
                    // or conversation changes the nav tree, so relayout.
                    let projects_changed = panel.refresh_projects();
                    if panel.recompute_statuses() || projects_changed {
                        panel.recompute_layout(hwnd);
                    }
                    panel.sync_pulse_timer(hwnd);
                    // Keep the remembered workspace current: a crash should
                    // cost at most the last second, and a conversation whose
                    // title the scanner has just resolved should come back
                    // under that name.
                    panel.save_open_sessions();
                }
                // Always repaint on the 1 Hz tick: statuses are derived
                // from the transcript scanner, which advances on its own
                // thread rather than through a change notification. A
                // once-per-second paint is cheap and keeps every tile in
                // sync.
                let _ = InvalidateRect(hwnd, None, false);
            }
            if wparam.0 == TIMER_PULSE {
                // Animation frame only — nothing to recompute, the pulse
                // phase comes off the wall clock at paint time.
                let _ = InvalidateRect(hwnd, None, false);
            }
            if wparam.0 == TIMER_SEARCH {
                let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(panel) = panel_guard.as_mut() {
                    if !panel.search.step() {
                        panel.sync_search_timer(hwnd);
                    }
                }
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = KillTimer(hwnd, TIMER_ATTENTION);
            let _ = KillTimer(hwnd, TIMER_PULSE);
            let _ = KillTimer(hwnd, TIMER_SEARCH);
            {
                // Closing the panel ends its PTYs, so this is the moment the
                // workspace has to be written down — reopening restores the
                // same sessions as cards.
                let panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(panel) = panel_guard.as_ref() {
                    panel.save_open_sessions();
                }
            }
            *PANEL_HWND.lock().unwrap_or_else(|e| e.into_inner()) = 0;
            *PANEL.lock().unwrap_or_else(|e| e.into_inner()) = None;
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Toggle the panel between its two views. Right-clicking the same button
/// opens the grid-size picker instead.
fn cycle_view(hwnd: HWND) {
    let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(panel) = panel_guard.as_mut() {
        let next = match panel.view {
            PanelView::Dashboard { .. } => PanelView::Grid,
            PanelView::Grid => PanelView::Dashboard { queue_mode: false },
        };
        panel.set_view(next);
        panel.recompute_layout(hwnd);
    }
    unsafe {
        let _ = InvalidateRect(hwnd, None, false);
    }
}

fn switch_view(hwnd: HWND, view: PanelView) {
    let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(panel) = panel_guard.as_mut() {
        panel.set_view(view);
        panel.recompute_layout(hwnd);
    }
    unsafe {
        let _ = InvalidateRect(hwnd, None, false);
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

/// Repaint the caption strip after the poller lands new numbers. The panel
/// has no poll of its own; it reads `window::quota_rows` when it paints, so
/// it needs telling that the answer changed.
pub fn refresh_quota_strip() {
    let hwnd_val = *PANEL_HWND.lock().unwrap_or_else(|e| e.into_inner());
    if hwnd_val == 0 {
        return;
    }
    let hwnd = HWND(hwnd_val as *mut _);
    unsafe {
        if !IsWindow(hwnd).as_bool() {
            return;
        }
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        let dpi = GetDpiForWindow(hwnd).max(96);
        let strip = caption_strip(&client, dpi);
        let _ = InvalidateRect(hwnd, Some(&strip), false);
    }
}

/// The widget's two quota bars, drawn right-aligned against the chrome
/// buttons. Two tiers: the full "42% - 2h 10m" line, and bare percentages
/// once the panel is too narrow for it. Narrower still and the strip stays
/// empty - half a bar reads worse than none.
fn paint_quota_strip(hdc: HDC, client: &RECT, dpi: u32) {
    let Some(rows) = window::quota_rows() else {
        return;
    };
    let scale = dpi as f64 / 96.0;
    let px = |v: i32| (v as f64 * scale).round() as i32;
    let strip = caption_strip(client, dpi);
    let seg_w = px(QUOTA_SEG_W).max(2);
    let seg_h = px(QUOTA_SEG_H).max(4);
    let seg_gap = px(QUOTA_SEG_GAP).max(1);
    let bar_w = QUOTA_SEG_COUNT * (seg_w + seg_gap) - seg_gap;
    let top = strip.top + ((strip.bottom - strip.top) - seg_h) / 2;
    let right_edge = view_button_rect(client, dpi).left - px(QUOTA_BUTTON_GAP);
    let min_left = client.left + px(PANEL_PAD_X);

    unsafe {
        let face = native_interop::wide_str("Segoe UI");
        let font = CreateFontW(
            -(QUOTA_FONT_PT * dpi as i32 / 72),
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
        );
        let old_font = SelectObject(hdc, font);
        let _ = SetBkMode(hdc, TRANSPARENT);

        let text_w = |text: &str| -> i32 {
            let wide: Vec<u16> = text.encode_utf16().collect();
            let mut size = SIZE::default();
            let _ = GetTextExtentPoint32W(hdc, &wide, &mut size);
            size.cx
        };

        let full: Vec<String> = rows.iter().map(|r| r.text.clone()).collect();
        let terse: Vec<String> = rows
            .iter()
            .map(|r| format!("{:.0}%", r.percent.clamp(0.0, 100.0)))
            .collect();
        let block_w = |texts: &[String]| -> i32 {
            let mut total = 0;
            for (row, text) in rows.iter().zip(texts) {
                total += text_w(row.label)
                    + px(QUOTA_LABEL_GAP)
                    + bar_w
                    + px(QUOTA_TEXT_GAP)
                    + text_w(text);
            }
            total + px(QUOTA_BLOCK_GAP)
        };

        let texts = if right_edge - block_w(&full) >= min_left {
            &full
        } else if right_edge - block_w(&terse) >= min_left {
            &terse
        } else {
            SelectObject(hdc, old_font);
            let _ = DeleteObject(font);
            return;
        };

        // Laid out right to left so the block stays pinned to the buttons
        // whatever the label and countdown widths come out to.
        let accent = Color::from_hex(ORANGE_HEX);
        let track = Color::from_hex(QUOTA_TRACK_HEX);
        let _ = SetTextColor(hdc, COLORREF(Color::from_hex(QUOTA_FG_HEX).to_colorref()));
        let mut x = right_edge;
        for (row, text) in rows.iter().zip(texts).rev() {
            let tw = text_w(text);
            x -= tw;
            draw_quota_text(hdc, text, x, top, tw, seg_h);
            x -= px(QUOTA_TEXT_GAP) + bar_w;
            draw_quota_bar(
                hdc,
                x,
                top,
                seg_w,
                seg_h,
                seg_gap,
                px(QUOTA_CORNER).max(1),
                row.percent,
                &accent,
                &track,
            );
            let lw = text_w(row.label);
            x -= px(QUOTA_LABEL_GAP) + lw;
            draw_quota_text(hdc, row.label, x, top, lw, seg_h);
            x -= px(QUOTA_BLOCK_GAP);
        }

        SelectObject(hdc, old_font);
        let _ = DeleteObject(font);
    }
}

fn draw_quota_text(hdc: HDC, text: &str, x: i32, top: i32, width: i32, height: i32) {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    let mut rect = RECT {
        left: x,
        top,
        right: x + width,
        bottom: top + height,
    };
    unsafe {
        let _ = DrawTextW(
            hdc,
            &mut wide,
            &mut rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
    }
}

/// One ten-segment bar. The segment straddling the fill line is clipped to
/// its rounded outline so a partial fill keeps the pill shape.
#[allow(clippy::too_many_arguments)]
fn draw_quota_bar(
    hdc: HDC,
    x: i32,
    top: i32,
    seg_w: i32,
    seg_h: i32,
    seg_gap: i32,
    corner: i32,
    percent: f64,
    accent: &Color,
    track: &Color,
) {
    let percent = percent.clamp(0.0, 100.0);
    for i in 0..QUOTA_SEG_COUNT {
        let seg_x = x + i * (seg_w + seg_gap);
        let seg_start = i as f64 * 10.0;
        let seg_rect = RECT {
            left: seg_x,
            top,
            right: seg_x + seg_w,
            bottom: top + seg_h,
        };
        if percent >= seg_start + 10.0 {
            fill_rounded(hdc, &seg_rect, accent, corner);
            continue;
        }
        fill_rounded(hdc, &seg_rect, track, corner);
        if percent <= seg_start {
            continue;
        }
        let fill_w = (seg_w as f64 * (percent - seg_start) / 10.0) as i32;
        if fill_w <= 0 {
            continue;
        }
        let fill_rect = RECT {
            right: seg_rect.left + fill_w,
            ..seg_rect
        };
        unsafe {
            let rgn = CreateRoundRectRgn(
                seg_rect.left,
                seg_rect.top,
                seg_rect.right + 1,
                seg_rect.bottom + 1,
                corner * 2,
                corner * 2,
            );
            let _ = SelectClipRgn(hdc, rgn);
            let brush = CreateSolidBrush(COLORREF(accent.to_colorref()));
            FillRect(hdc, &fill_rect, brush);
            let _ = DeleteObject(brush);
            let _ = SelectClipRgn(hdc, HRGN::default());
            let _ = DeleteObject(rgn);
        }
    }
}

fn fill_rounded(hdc: HDC, rect: &RECT, color: &Color, radius: i32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color.to_colorref()));
        let rgn = CreateRoundRectRgn(
            rect.left,
            rect.top,
            rect.right + 1,
            rect.bottom + 1,
            radius * 2,
            radius * 2,
        );
        let _ = FillRgn(hdc, rgn, brush);
        let _ = DeleteObject(rgn);
        let _ = DeleteObject(brush);
    }
}

fn paint_chrome_buttons(hdc: HDC, client: &RECT, dpi: u32, hwnd: HWND) {
    let orange = Color::from_hex(ORANGE_HEX);
    let scale = dpi as f64 / 96.0;
    let pen_w = (CHROME_STROKE as f64 * scale).round().max(1.0) as i32;
    let close = close_button_rect(client, dpi);
    let maximize = maximize_button_rect(client, dpi);
    let minimize = minimize_button_rect(client, dpi);
    let view = view_button_rect(client, dpi);

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

        // Maximize / restore.
        if IsZoomed(hwnd).as_bool() {
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

        // View cycle: a small 2x2 grid that hints at "switch layout".
        let mid_x = (view.left + view.right) / 2;
        let mid_y = (view.top + view.bottom) / 2;
        let _ = MoveToEx(hdc, mid_x, view.top, None);
        let _ = LineTo(hdc, mid_x, view.bottom);
        let _ = MoveToEx(hdc, view.left, mid_y, None);
        let _ = LineTo(hdc, view.right, mid_y);
        let _ = Rectangle(hdc, view.left, view.top, view.right, view.bottom);

        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(pen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_tab_takes_flagged_sessions_first() {
        let ids = [1, 2, 3, 4];
        let queue: VecDeque<SessionId> = [3, 4].into_iter().collect();
        // Focused on 1, two sessions waiting: the queue wins over grid order.
        assert_eq!(next_grid_focus(&queue, &ids, Some(1)), Some(3));
        // Landing on 3 acknowledges it, so the next hop is the other flagged
        // one rather than 3 again.
        let queue: VecDeque<SessionId> = [4].into_iter().collect();
        assert_eq!(next_grid_focus(&queue, &ids, Some(3)), Some(4));
    }

    #[test]
    fn ctrl_tab_walks_the_grid_when_nothing_is_flagged() {
        let ids = [1, 2, 3];
        let empty = VecDeque::new();
        assert_eq!(next_grid_focus(&empty, &ids, Some(1)), Some(2));
        assert_eq!(next_grid_focus(&empty, &ids, Some(3)), Some(1));
        assert_eq!(next_grid_focus(&empty, &ids, None), Some(1));
        assert_eq!(next_grid_focus(&empty, &[], Some(1)), None);
    }

    /// A stale queue entry for the session already focused must not pin the
    /// cycle in place — that would make Ctrl+Tab a no-op key.
    #[test]
    fn the_focused_session_never_traps_the_cycle() {
        let ids = [1, 2];
        let queue: VecDeque<SessionId> = [1].into_iter().collect();
        assert_eq!(next_grid_focus(&queue, &ids, Some(1)), Some(2));
    }
}
