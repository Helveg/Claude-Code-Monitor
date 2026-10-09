//! The action panel's view + tile abstraction.
//!
//! A `PanelView` describes which arrangement of tiles the panel is rendering
//! (Dashboard or Grid). A `Tile` is one piece of that arrangement.
//! `layout()` takes a view + the panel's client rect and produces an ordered
//! `Vec<(Tile, RECT)>` for the painter and the input router to walk.
//!
//! Each tile's `handle_lbutton_down` returns an optional [`TileAction`]
//! the panel applies — focusing a session, starting a terminal drag, or
//! creating a new session.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::HDC;

use crate::grid_tile::{self, GridShape};
use crate::project_tree::{self, NavTarget};
use crate::projects::{AttentionRow, ProjectNode};
use crate::resume_card::{self, CardButton};
use crate::session_view::ChromeStyle;
use crate::sessions::{SessionId, Sessions};

/// The nav tree's state, which lives on the panel rather than in the tile:
/// the project snapshot being drawn, which projects are open, the scroll
/// offset, and the row under the cursor.
pub struct NavState<'a> {
    /// The tree as drawn — already narrowed by [`NavSearch::query`], so the
    /// indices in a [`NavTarget`] address this same filtered list.
    pub tree: &'a [ProjectNode],
    /// Sessions claude has flagged, drawn as a section above the projects.
    /// Empty means the section isn't there at all.
    pub attention: &'a [AttentionRow],
    pub expanded: &'a HashSet<PathBuf>,
    /// Projects showing their whole conversation history rather than the
    /// most recent handful. Keyed by path, like [`Self::expanded`].
    pub history_expanded: &'a HashSet<PathBuf>,
    pub scroll_y: i32,
    pub hovered: Option<NavTarget>,
    /// The project whose remove control has been clicked once and is
    /// showing its "Remove?" confirmation.
    pub armed_remove: Option<&'a Path>,
    pub search: NavSearch<'a>,
    /// Where the grid view keeps the tree, which the header's dock control
    /// swaps. `None` outside the grid, where the tree is always a sidebar.
    pub grid_dock: Option<GridDock>,
}

/// Where the grid view keeps the project list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GridDock {
    /// The grid's first cell.
    Cell,
    /// A sidebar beside the grid, as in the focus view.
    Sidebar,
}

/// How the user has arranged the project list, written to settings.json.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NavLayout {
    /// Hidden in every view, until the caption's pane toggle brings it back.
    #[serde(default)]
    pub collapsed: bool,
    /// The grid view shows the list as a sidebar instead of its first cell.
    #[serde(default)]
    pub grid_sidebar: bool,
}

/// Where the project list ends up in one view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavPlacement {
    Sidebar,
    GridCell,
    Hidden,
}

impl NavLayout {
    pub fn placement(self, view: &PanelView) -> NavPlacement {
        if self.collapsed {
            return NavPlacement::Hidden;
        }
        match view {
            PanelView::Grid if !self.grid_sidebar => NavPlacement::GridCell,
            _ => NavPlacement::Sidebar,
        }
    }

    /// The grid's dock state, for the tree header's control. `None` outside
    /// the grid view.
    pub fn grid_dock(self, view: &PanelView) -> Option<GridDock> {
        match view {
            PanelView::Grid if self.grid_sidebar => Some(GridDock::Sidebar),
            PanelView::Grid => Some(GridDock::Cell),
            PanelView::Dashboard { .. } => None,
        }
    }
}

/// State of the nav header's filter box.
#[derive(Clone, Copy, Default)]
pub struct NavSearch<'a> {
    /// True from the click that opens the box until the one that closes it.
    /// Typing goes to the box while this holds.
    pub open: bool,
    /// What the user has typed. Empty while closed.
    pub query: &'a str,
    /// How far the box has slid open, `0.0..=1.0`. Runs behind `open` for
    /// the length of the animation, which is why both exist.
    pub anim: f64,
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // queue_mode lights up in Phase 3.
pub enum Tile {
    /// The big terminal slot.
    MainTerminal {
        session_id: Option<SessionId>,
        queue_mode: bool,
    },
    /// Tree of projects, each expanding to its live sessions and past
    /// conversations, with a `+` per project to start a new one.
    ProjectTree,
    /// `cols x rows` grid of live terminals, one per session — after the
    /// project list, when `grid.nav_cell` gives it the first cell.
    /// `scroll_y` shifts the rendered cell rows up by that many pixels — the
    /// panel feeds in the current scroll offset so the same cache survives
    /// across paints.
    SessionGrid { scroll_y: i32, grid: GridShape },
}

#[derive(Clone, Debug)]
pub enum TileAction {
    /// A press landed inside the given session's terminal: make it the
    /// keyboard target and start a selection drag on it.
    StartDrag(SessionId),
    /// Make the given session the focused one (renders in MainTerminal).
    FocusSession(SessionId),
    /// Spawn a fresh claude session with the given directory as its cwd —
    /// that's all it takes to scope a session to a project.
    NewSessionIn(PathBuf),
    /// Spawn a terminal that resumes a past conversation (`--resume`) in
    /// the project it belongs to.
    ResumeSession { cwd: PathBuf, session_id: String },
    /// Take a project out of the nav. Its transcripts stay on disk; starting
    /// a session in the directory again brings it back.
    HideProject(PathBuf),
    /// Show or dismiss the "Remove?" confirmation on a project row — the
    /// first click on its trash can, which removes nothing by itself.
    ToggleRemoveConfirm(PathBuf),
    /// Collapse / expand a project in the nav tree, keyed by path so the
    /// state survives the scanner reordering the tree.
    ToggleProject(PathBuf),
    /// Show a project's whole conversation history, or fold it back to the
    /// most recent few.
    ToggleHistory(PathBuf),
    /// Ask for a directory, then start a session in it — the nav's "New"
    /// badge. A project only exists once claude has run somewhere, so
    /// picking a folder and opening it are the same gesture.
    PickNewProject,
    /// Open or close the nav's filter box.
    ToggleSearch,
    /// Move the grid view's project list between its first cell and a
    /// sidebar.
    ToggleGridDock,
    /// Start a restored session's conversation back up, in the slot it
    /// already occupies.
    ResumeDormant(SessionId),
    /// Drop a session from the workspace: its cell goes, and it won't come
    /// back on the next restart. A live one's PTY closes with it.
    CloseSession(SessionId),
}

/// A control drawn on a session's slot rather than in the terminal: the
/// resume card's two answers, and the close cross on a grid cell's frame.
/// The panel tracks which one the cursor is over so it can be lit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotControl {
    Resume,
    Close,
    FrameClose,
}

impl From<CardButton> for SlotControl {
    fn from(button: CardButton) -> Self {
        match button {
            CardButton::Resume => SlotControl::Resume,
            CardButton::Close => SlotControl::Close,
        }
    }
}

/// Which card button `control` lights up, if it is one.
pub fn card_hover(control: Option<(SessionId, SlotControl)>, id: SessionId) -> Option<CardButton> {
    match control {
        Some((hovered, SlotControl::Resume)) if hovered == id => Some(CardButton::Resume),
        Some((hovered, SlotControl::Close)) if hovered == id => Some(CardButton::Close),
        _ => None,
    }
}

/// Cursor a tile wants Windows to render at a given point. The panel maps
/// this to the appropriate `LoadCursor` IDC_*; `Default` means "let the
/// caller pick" and is typically rendered as the regular arrow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorHint {
    Default,
    Arrow,
    IBeam,
    Hand,
    /// Over a grid cell's move handle — the strip its name label sits in.
    SizeAll,
    /// Over a grid cell's right, bottom, or bottom-right resize edge.
    SizeWE,
    SizeNS,
    SizeNWSE,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelView {
    /// Default: project sidebar on the left, main terminal filling the rest.
    Dashboard { queue_mode: bool },
    /// Grid of live terminals. The project list takes the grid's first cell
    /// or sits beside it as a sidebar, per [`NavLayout::grid_sidebar`].
    Grid,
}

/// Layout constants in design pixels at 96 DPI.
const SIDEBAR_WIDTH: i32 = 220;
const TILE_GAP: i32 = 8;

/// The sidebar's rect, and what is left of `panel` beside it once `gap` is
/// taken out between the two.
fn split_sidebar(panel: RECT, dpi: u32, gap: i32) -> (RECT, RECT) {
    let scale = dpi as f64 / 96.0;
    let sidebar_w = (SIDEBAR_WIDTH as f64 * scale).round() as i32;
    let sidebar = RECT {
        right: panel.left + sidebar_w,
        ..panel
    };
    let rest = RECT {
        left: sidebar.right + gap,
        ..panel
    };
    (sidebar, rest)
}

/// Build the tile list for the current view. Also writes the assigned bounds
/// into each session's `SessionView` so subsequent paint calls (which read
/// `self.bounds`) see the right region.
pub fn layout(
    view: &PanelView,
    panel: RECT,
    dpi: u32,
    sessions: &mut Sessions,
    attention_queue: &VecDeque<SessionId>,
    focused_session: Option<SessionId>,
    grid_scroll_y: i32,
    grid_cols: i32,
    grid_rows: i32,
    nav_layout: NavLayout,
    font_pt: i32,
) -> Vec<(Tile, RECT)> {
    let mut tiles: Vec<(Tile, RECT)> = Vec::new();
    let placement = nav_layout.placement(view);

    match view {
        PanelView::Dashboard { queue_mode } => {
            let gap = (TILE_GAP as f64 * dpi as f64 / 96.0).round() as i32;
            let main = if placement == NavPlacement::Sidebar {
                let (sidebar, main) = split_sidebar(panel, dpi, gap);
                tiles.push((Tile::ProjectTree, sidebar));
                main
            } else {
                panel
            };

            // In queue mode the main terminal follows the front of the
            // attention queue; otherwise it shows the user-selected session
            // (or any session at all if nothing is focused yet).
            let session_id = if *queue_mode {
                attention_queue
                    .front()
                    .copied()
                    .or(focused_session)
                    .or_else(|| sessions.first_id())
            } else {
                focused_session.or_else(|| sessions.first_id())
            };
            tiles.push((
                Tile::MainTerminal {
                    session_id,
                    queue_mode: *queue_mode,
                },
                main,
            ));
        }
        PanelView::Grid => {
            // The grid pads its cells with a gap of its own, so it starts
            // right at the sidebar's edge.
            let cells = if placement == NavPlacement::Sidebar {
                let (sidebar, cells) = split_sidebar(panel, dpi, 0);
                tiles.push((Tile::ProjectTree, sidebar));
                cells
            } else {
                panel
            };
            tiles.push((
                Tile::SessionGrid {
                    scroll_y: grid_scroll_y,
                    grid: GridShape {
                        cols: grid_cols,
                        rows: grid_rows,
                        nav_cell: placement == NavPlacement::GridCell,
                    },
                },
                cells,
            ));
        }
    }

    apply_bounds(&tiles, dpi, sessions, font_pt);
    tiles
}

/// Give every session the rect it renders into for this view — which is also
/// what sizes its PTY. Sessions the current view doesn't show still get a
/// rect, so their terminals stay spawned and keep consuming output.
fn apply_bounds(tiles: &[(Tile, RECT)], dpi: u32, sessions: &mut Sessions, font_pt: i32) {
    for (tile, rect) in tiles {
        match tile {
            // The main slot's rect goes to *every* session, not just the
            // focused one: a session that isn't on screen still needs
            // sensible grid dimensions so it renders correctly the moment
            // it is switched to.
            Tile::MainTerminal { .. } => {
                for s in sessions.iter_mut() {
                    s.session_view.set_bounds(*rect, dpi, font_pt);
                }
            }
            Tile::SessionGrid { scroll_y, grid } => {
                grid_tile::assign_bounds(*rect, dpi, sessions, *scroll_y, *grid, font_pt);
            }
            Tile::ProjectTree => {}
        }
    }
}

impl Tile {
    /// Render this tile. Sessions are immutable here — `layout()` already
    /// updated their bounds. `nav` is only read by the project tree and the
    /// grid's project cell; the main terminal ignores it.
    pub fn paint(
        &self,
        hdc: HDC,
        bounds: RECT,
        dpi: u32,
        sessions: &Sessions,
        focused: Option<SessionId>,
        nav: &NavState<'_>,
        hovered_control: Option<(SessionId, SlotControl)>,
    ) {
        match self {
            Tile::MainTerminal {
                session_id: Some(id),
                ..
            } => {
                if let Some(s) = sessions.get(*id) {
                    // One terminal on screen: no need to mark which one has
                    // the keyboard, and the sidebar already shows status.
                    s.session_view.paint(hdc, dpi, ChromeStyle::default());
                    if s.is_dormant() {
                        resume_card::paint(
                            hdc,
                            s.session_view.bounds(),
                            dpi,
                            s,
                            card_hover(hovered_control, *id),
                        );
                    }
                }
            }
            Tile::MainTerminal { session_id: None, .. } => {
                // No session — leave the panel chrome bg showing.
            }
            Tile::ProjectTree => {
                project_tree::paint(hdc, bounds, dpi, nav, sessions, focused);
            }
            Tile::SessionGrid { scroll_y, grid } => {
                grid_tile::paint(
                    hdc,
                    bounds,
                    dpi,
                    sessions,
                    focused,
                    *scroll_y,
                    nav,
                    *grid,
                    hovered_control,
                );
            }
        }
    }

    /// Session this tile routes keyboard input to, if it hosts one.
    pub fn keyboard_session(&self) -> Option<SessionId> {
        match self {
            Tile::MainTerminal { session_id, .. } => *session_id,
            _ => None,
        }
    }

    pub fn handle_lbutton_down(
        &self,
        x: i32,
        y: i32,
        bounds: RECT,
        dpi: u32,
        sessions: &Sessions,
        nav: &NavState<'_>,
    ) -> Option<TileAction> {
        match self {
            Tile::MainTerminal {
                session_id: Some(id),
                ..
            } => {
                let session = sessions.get(*id)?;
                // A dormant slot is a card, not a terminal: the only things
                // to click are its two answers.
                if session.is_dormant() {
                    return match resume_card::button_at(x, y, session.session_view.bounds(), dpi) {
                        Some(CardButton::Resume) => Some(TileAction::ResumeDormant(*id)),
                        Some(CardButton::Close) => Some(TileAction::CloseSession(*id)),
                        None => None,
                    };
                }
                session.session_view.terminal().handle_mouse_down(x, y);
                Some(TileAction::StartDrag(*id))
            }
            Tile::ProjectTree => {
                project_tree::handle_lbutton_down(x, y, bounds, dpi, nav, sessions)
            }
            Tile::SessionGrid { scroll_y, grid } => grid_tile::handle_lbutton_down(
                x, y, bounds, dpi, sessions, *scroll_y, nav, *grid,
            ),
            _ => None,
        }
    }

    /// Session whose terminal owns the point `(x, y)`, if this tile hosts
    /// one there. Used to route wheel notches and terminal mouse events to
    /// the terminal under the cursor rather than the focused one.
    pub fn session_at(
        &self,
        x: i32,
        y: i32,
        bounds: RECT,
        dpi: u32,
        sessions: &Sessions,
    ) -> Option<SessionId> {
        match self {
            Tile::MainTerminal { session_id, .. } => *session_id,
            Tile::SessionGrid { scroll_y, grid } => {
                grid_tile::session_at(x, y, bounds, dpi, sessions, *scroll_y, *grid)
            }
            Tile::ProjectTree => None,
        }
    }

    /// Cursor hint for `(x, y)` (panel client coords) inside this tile's
    /// `bounds`. The panel sets the system cursor accordingly.
    pub fn cursor_at(
        &self,
        x: i32,
        y: i32,
        bounds: RECT,
        dpi: u32,
        sessions: &Sessions,
        nav: &NavState<'_>,
    ) -> CursorHint {
        match self {
            Tile::MainTerminal {
                session_id: Some(id),
                ..
            } => sessions
                .get(*id)
                .map(|s| {
                    if s.is_dormant() {
                        let over_button =
                            resume_card::button_at(x, y, s.session_view.bounds(), dpi).is_some();
                        return if over_button {
                            CursorHint::Hand
                        } else {
                            CursorHint::Arrow
                        };
                    }
                    let term = s.session_view.terminal();
                    if term.over_scrollbar(x, y, dpi) {
                        return CursorHint::Hand;
                    }
                    let inner = term.bounds();
                    if x >= inner.left && x < inner.right && y >= inner.top && y < inner.bottom
                    {
                        CursorHint::IBeam
                    } else {
                        CursorHint::Arrow
                    }
                })
                .unwrap_or(CursorHint::Arrow),
            Tile::ProjectTree => project_tree::cursor_at(x, y, bounds, dpi, nav, sessions),
            Tile::SessionGrid { scroll_y, grid } => {
                grid_tile::cursor_at(x, y, bounds, dpi, sessions, *scroll_y, nav, *grid)
            }
            _ => CursorHint::Default,
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn panel() -> RECT {
        RECT {
            left: 0,
            top: 0,
            right: 1200,
            bottom: 800,
        }
    }

    fn tiles(view: PanelView, nav_layout: NavLayout) -> Vec<(Tile, RECT)> {
        let mut sessions = Sessions::new();
        layout(
            &view,
            panel(),
            96,
            &mut sessions,
            &VecDeque::new(),
            None,
            0,
            3,
            2,
            nav_layout,
            crate::terminal_view::FONT_POINT_SIZE,
        )
    }

    fn sidebar(tiles: &[(Tile, RECT)]) -> Option<RECT> {
        tiles.iter().find_map(|(tile, rect)| match tile {
            Tile::ProjectTree => Some(*rect),
            _ => None,
        })
    }

    fn grid(tiles: &[(Tile, RECT)]) -> Option<(GridShape, RECT)> {
        tiles.iter().find_map(|(tile, rect)| match tile {
            Tile::SessionGrid { grid, .. } => Some((*grid, *rect)),
            _ => None,
        })
    }

    /// Out of the box the grid keeps the list in its first cell.
    #[test]
    fn the_grid_keeps_the_list_in_its_first_cell_by_default() {
        let t = tiles(PanelView::Grid, NavLayout::default());
        assert!(sidebar(&t).is_none());
        let (shape, rect) = grid(&t).unwrap();
        assert!(shape.nav_cell);
        assert_eq!(rect, panel());
    }

    /// Docked as a sidebar, the list sits where the focus view puts it and
    /// the grid takes the rest, every cell of it a session's.
    #[test]
    fn a_docked_list_is_a_sidebar_beside_the_grid() {
        let docked = NavLayout {
            grid_sidebar: true,
            ..NavLayout::default()
        };
        let t = tiles(PanelView::Grid, docked);
        let side = sidebar(&t).expect("a sidebar");
        let (shape, rect) = grid(&t).unwrap();
        assert!(!shape.nav_cell);
        assert_eq!(rect.left, side.right);
        assert_eq!(rect.right, panel().right);

        let focus = tiles(PanelView::Dashboard { queue_mode: false }, docked);
        assert_eq!(sidebar(&focus), Some(side));
    }

    /// Collapsed, the list is gone from every view, wherever it was docked.
    #[test]
    fn a_collapsed_list_leaves_the_whole_panel_to_the_sessions() {
        for grid_sidebar in [false, true] {
            let collapsed = NavLayout {
                collapsed: true,
                grid_sidebar,
            };
            let t = tiles(PanelView::Grid, collapsed);
            assert!(sidebar(&t).is_none());
            let (shape, rect) = grid(&t).unwrap();
            assert!(!shape.nav_cell);
            assert_eq!(rect, panel());

            let focus = tiles(PanelView::Dashboard { queue_mode: false }, collapsed);
            assert!(sidebar(&focus).is_none());
            assert_eq!(focus[0].1, panel());
        }
    }
}
