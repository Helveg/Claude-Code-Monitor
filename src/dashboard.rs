//! The action panel's view + tile abstraction.
//!
//! A `PanelView` describes which arrangement of tiles the panel is rendering
//! (Dashboard / Fullscreen / SidebarGrid). A `Tile` is one piece of that
//! arrangement. `layout()` takes a view + the panel's client rect and
//! produces an ordered `Vec<(Tile, RECT)>` for the painter and the input
//! router to walk.
//!
//! Each tile's `handle_lbutton_down` returns an optional [`TileAction`]
//! the panel applies — focusing a session, starting a terminal drag, or
//! creating a new session.

use std::collections::VecDeque;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::HDC;

use crate::cards_tile;
use crate::notifications_tile;
use crate::sessions::{SessionId, Sessions};
use crate::sidebar_tile;

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // queue_mode lights up in Phase 3.
pub enum Tile {
    /// The big terminal slot.
    MainTerminal {
        session_id: Option<SessionId>,
        queue_mode: bool,
    },
    /// Vertical list of session status widgets + a "+ New session" entry
    /// at the bottom.
    SessionStatusList,
    /// Vertical list of `NeedsAttention` sessions in the top-right area.
    NotificationsList,
    /// Wrapping grid of cards with live mini-terminal previews. When
    /// `include_orphans` is set, also renders read-only cards for claude
    /// projects in `~/.claude/projects/` that don't currently have a live
    /// session pointed at their cwd. `scroll_y` shifts the rendered card
    /// rows up by that many pixels — the panel feeds in the current scroll
    /// offset so the same cache survives across paints.
    SessionCardsGrid {
        include_orphans: bool,
        scroll_y: i32,
    },
}

#[derive(Clone, Debug)]
pub enum TileAction {
    /// Start a mouse drag on the given session's terminal (selection).
    StartDrag(SessionId),
    /// Make the given session the focused one (renders in MainTerminal).
    FocusSession(SessionId),
    /// Spawn a new session with a default name.
    CreateSession,
    /// Spawn a fresh PTY running `claude --resume <session_id>` in `cwd`,
    /// adopting an orphan jsonl into the live panel as a clickable session.
    ResumeSession {
        session_id: String,
        cwd: std::path::PathBuf,
        name: String,
    },
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelView {
    /// Default: sidebar | main terminal | notifications | bottom card grid.
    Dashboard { queue_mode: bool },
    /// Just the main terminal at the entire panel area.
    Fullscreen,
    /// Full-height sidebar on the left, cards grid filling the rest.
    SidebarGrid,
}

/// Layout constants in design pixels at 96 DPI.
const SIDEBAR_WIDTH: i32 = 220;
const NOTIFICATIONS_WIDTH: i32 = 240;
const TILE_GAP: i32 = 8;
const BOTTOM_GRID_HEIGHT_FRAC: f32 = 0.40;

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
    cards_scroll_y: i32,
) -> Vec<(Tile, RECT)> {
    let mut tiles: Vec<(Tile, RECT)> = Vec::new();

    match view {
        PanelView::Dashboard { queue_mode } => {
            let scale = dpi as f64 / 96.0;
            let sidebar_w = (SIDEBAR_WIDTH as f64 * scale).round() as i32;
            let notifications_w = (NOTIFICATIONS_WIDTH as f64 * scale).round() as i32;
            let gap = (TILE_GAP as f64 * scale).round() as i32;

            let panel_h = panel.bottom - panel.top;
            let bottom_h = (panel_h as f32 * BOTTOM_GRID_HEIGHT_FRAC).round() as i32;
            let top_h = panel_h - bottom_h - gap;

            let top = RECT {
                left: panel.left,
                top: panel.top,
                right: panel.right,
                bottom: panel.top + top_h.max(0),
            };
            let bottom = RECT {
                left: panel.left,
                top: top.bottom + gap,
                right: panel.right,
                bottom: panel.bottom,
            };

            let sidebar = RECT {
                left: top.left,
                top: top.top,
                right: top.left + sidebar_w,
                bottom: top.bottom,
            };
            let notifications = RECT {
                left: top.right - notifications_w,
                top: top.top,
                right: top.right,
                bottom: top.bottom,
            };
            let main = RECT {
                left: sidebar.right + gap,
                top: top.top,
                right: notifications.left - gap,
                bottom: top.bottom,
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
            tiles.push((Tile::SessionStatusList, sidebar));
            tiles.push((
                Tile::MainTerminal {
                    session_id,
                    queue_mode: *queue_mode,
                },
                main,
            ));
            tiles.push((Tile::NotificationsList, notifications));
            tiles.push((
                Tile::SessionCardsGrid {
                    include_orphans: true,
                    scroll_y: cards_scroll_y,
                },
                bottom,
            ));
        }
        PanelView::Fullscreen => {
            let session_id = focused_session.or_else(|| sessions.first_id());
            tiles.push((
                Tile::MainTerminal {
                    session_id,
                    queue_mode: false,
                },
                panel,
            ));
        }
        PanelView::SidebarGrid => {
            let scale = dpi as f64 / 96.0;
            let sidebar_w = (SIDEBAR_WIDTH as f64 * scale).round() as i32;
            let gap = (TILE_GAP as f64 * scale).round() as i32;
            let sidebar = RECT {
                left: panel.left,
                top: panel.top,
                right: panel.left + sidebar_w,
                bottom: panel.bottom,
            };
            let grid = RECT {
                left: sidebar.right + gap,
                top: panel.top,
                right: panel.right,
                bottom: panel.bottom,
            };
            tiles.push((Tile::SessionStatusList, sidebar));
            tiles.push((
                Tile::SessionCardsGrid {
                    include_orphans: true,
                    scroll_y: cards_scroll_y,
                },
                grid,
            ));
        }
    }

    apply_bounds(&tiles, dpi, sessions);
    tiles
}

fn apply_bounds(tiles: &[(Tile, RECT)], dpi: u32, sessions: &mut Sessions) {
    // Find the rect a MainTerminal is currently using (if any) and apply
    // it to *every* session — not just the focused one. Each session needs
    // its terminal spawned with sensible grid dimensions so the cards-grid
    // can render live previews even for sessions that aren't currently
    // shown in the main slot.
    let main_rect = tiles.iter().find_map(|(t, r)| {
        if matches!(t, Tile::MainTerminal { .. }) {
            Some(*r)
        } else {
            None
        }
    });
    if let Some(rect) = main_rect {
        for s in sessions.iter_mut() {
            s.session_view.set_bounds(rect, dpi);
        }
    }
}

impl Tile {
    /// Render this tile. Sessions are immutable here — `layout()` already
    /// updated their bounds.
    pub fn paint(
        &self,
        hdc: HDC,
        bounds: RECT,
        dpi: u32,
        sessions: &Sessions,
        focused: Option<SessionId>,
    ) {
        match self {
            Tile::MainTerminal {
                session_id: Some(id),
                ..
            } => {
                if let Some(s) = sessions.get(*id) {
                    s.session_view.paint(hdc, dpi);
                }
            }
            Tile::MainTerminal { session_id: None, .. } => {
                // No session — leave the panel chrome bg showing.
            }
            Tile::SessionStatusList => {
                sidebar_tile::paint(hdc, bounds, dpi, sessions, focused);
            }
            Tile::NotificationsList => {
                notifications_tile::paint(hdc, bounds, dpi, sessions);
            }
            Tile::SessionCardsGrid { include_orphans, scroll_y } => {
                cards_tile::paint(
                    hdc,
                    bounds,
                    dpi,
                    sessions,
                    focused,
                    *include_orphans,
                    *scroll_y,
                );
            }
        }
    }

    pub fn handle_lbutton_down(
        &self,
        x: i32,
        y: i32,
        bounds: RECT,
        dpi: u32,
        sessions: &Sessions,
    ) -> Option<TileAction> {
        match self {
            Tile::MainTerminal {
                session_id: Some(id),
                ..
            } => {
                if let Some(s) = sessions.get(*id) {
                    s.session_view.terminal().handle_mouse_down(x, y);
                }
                Some(TileAction::StartDrag(*id))
            }
            Tile::SessionStatusList => sidebar_tile::handle_lbutton_down(x, y, bounds, dpi, sessions),
            Tile::NotificationsList => {
                notifications_tile::handle_lbutton_down(x, y, bounds, dpi, sessions)
            }
            Tile::SessionCardsGrid { include_orphans, scroll_y } => {
                cards_tile::handle_lbutton_down(
                    x,
                    y,
                    bounds,
                    dpi,
                    sessions,
                    *include_orphans,
                    *scroll_y,
                )
            }
            _ => None,
        }
    }

    pub fn handle_mouse_move(&self, x: i32, y: i32, sessions: &Sessions) {
        if let Tile::MainTerminal {
            session_id: Some(id),
            ..
        } = self
        {
            if let Some(s) = sessions.get(*id) {
                s.session_view.terminal().handle_mouse_move(x, y);
            }
        }
    }

    pub fn handle_lbutton_up(&self, sessions: &Sessions) {
        if let Tile::MainTerminal {
            session_id: Some(id),
            ..
        } = self
        {
            if let Some(s) = sessions.get(*id) {
                s.session_view.terminal().handle_mouse_up();
            }
        }
    }

    pub fn handle_char(&self, code: u32, sessions: &Sessions) -> bool {
        match self {
            Tile::MainTerminal {
                session_id: Some(id),
                ..
            } => sessions
                .get(*id)
                .map(|s| s.session_view.terminal().handle_char(code))
                .unwrap_or(false),
            _ => false,
        }
    }

    pub fn handle_key_down(&self, vk: u32, sessions: &Sessions) -> bool {
        match self {
            Tile::MainTerminal {
                session_id: Some(id),
                ..
            } => sessions
                .get(*id)
                .map(|s| s.session_view.terminal().handle_key_down(vk))
                .unwrap_or(false),
            _ => false,
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
    ) -> CursorHint {
        match self {
            Tile::MainTerminal {
                session_id: Some(id),
                ..
            } => sessions
                .get(*id)
                .map(|s| {
                    let inner = s.session_view.terminal().bounds();
                    if x >= inner.left && x < inner.right && y >= inner.top && y < inner.bottom
                    {
                        CursorHint::IBeam
                    } else {
                        CursorHint::Arrow
                    }
                })
                .unwrap_or(CursorHint::Arrow),
            Tile::SessionStatusList => sidebar_tile::cursor_at(x, y, bounds, dpi, sessions),
            Tile::NotificationsList => {
                notifications_tile::cursor_at(x, y, bounds, dpi, sessions)
            }
            Tile::SessionCardsGrid { include_orphans, scroll_y } => {
                cards_tile::cursor_at(x, y, bounds, dpi, sessions, *include_orphans, *scroll_y)
            }
            _ => CursorHint::Default,
        }
    }
}

