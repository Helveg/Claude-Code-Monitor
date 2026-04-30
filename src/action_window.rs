use std::collections::VecDeque;
use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_CONTROL,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::cards_tile;
use crate::dashboard::{self, PanelView};
use crate::native_interop::{self, Color};
use crate::session_view::SessionView;
use crate::sessions::{SessionId, SessionStatus, Sessions};

const PANEL_CLASS: &str = "ClaudeCodeUsageMonitorPanel";

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


pub const WM_APP_TERM_OUTPUT: u32 = WM_APP + 100;

static PANEL_HWND: Mutex<isize> = Mutex::new(0);
static PANEL: Mutex<Option<Panel>> = Mutex::new(None);

/// All panel state lives in one struct so the layout/dispatch pipeline can
/// take a single mutex lock per message.
struct Panel {
    view: PanelView,
    /// Panel-wide "currently selected" session. Persists across view
    /// changes so jumping between Dashboard and Fullscreen keeps the same
    /// session focused.
    focused_session: Option<SessionId>,
    sessions: Sessions,
    /// Cached output of `dashboard::layout` — recomputed when the view
    /// changes or the panel resizes. Used by the input router so it doesn't
    /// have to re-run layout per event.
    layout_cache: Vec<(dashboard::Tile, RECT)>,
    /// Session whose terminal currently owns an in-progress mouse drag.
    dragging_session: Option<SessionId>,
    /// FIFO of sessions whose status flipped to `NeedsAttention`. The front
    /// is what queue mode shows in the main terminal.
    attention_queue: VecDeque<SessionId>,
    /// Vertical scroll offset for the cards grid (Dashboard bottom area /
    /// SidebarGrid main area). In *device* pixels, clamped at paint time.
    cards_scroll_y: i32,
    /// True while the user drags the cards-grid scrollbar thumb. Holds the
    /// y-pixel offset between the thumb origin and the mouse, plus the
    /// last-seen tile bounds so wheel arithmetic still works mid-drag.
    cards_scroll_drag: Option<CardsScrollDrag>,
}

#[derive(Clone, Copy)]
struct CardsScrollDrag {
    grab_offset: i32,
    bounds: RECT,
    content_h: i32,
}

const TIMER_ATTENTION: usize = 1;
const TIMER_ATTENTION_INTERVAL_MS: u32 = 1_000;

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

        // Initial session: one "claude" session filling the panel area,
        // running the claude code CLI in the app's current working directory.
        let claude_cmd = crate::claude::ClaudeArgs::default().build_command_line();
        let cwd = std::env::current_dir().ok();
        let session_view =
            SessionView::new(hwnd, WM_APP_TERM_OUTPUT, "claude", claude_cmd, cwd.clone());
        let mut sessions = Sessions::new();
        let id = sessions.add("claude", session_view, cwd, None);

        let view = PanelView::Dashboard { queue_mode: false };

        let mut panel = Panel {
            view,
            focused_session: Some(id),
            sessions,
            layout_cache: Vec::new(),
            dragging_session: None,
            attention_queue: VecDeque::new(),
            cards_scroll_y: 0,
            cards_scroll_drag: None,
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
        self.layout_cache = dashboard::layout(
            &self.view,
            area,
            dpi,
            &mut self.sessions,
            &self.attention_queue,
            self.focused_session,
            self.cards_scroll_y,
        );
    }

    /// The tile that should receive keyboard input. Always the MainTerminal
    /// tile in the current view.
    fn focused_tile(&self) -> Option<&dashboard::Tile> {
        self.layout_cache
            .iter()
            .find(|(t, _)| matches!(t, dashboard::Tile::MainTerminal { .. }))
            .map(|(t, _)| t)
    }

    /// Apply a [`TileAction`] returned by a tile's input handler.
    /// `hwnd` is the panel window — needed for spawning new sessions.
    fn apply_tile_action(&mut self, action: dashboard::TileAction, hwnd: HWND) {
        match action {
            dashboard::TileAction::StartDrag(id) => {
                self.dragging_session = Some(id);
            }
            dashboard::TileAction::FocusSession(id) => {
                self.set_focused_session(Some(id));
                // Acknowledge the new focused session immediately so the
                // status flips out of NeedsAttention on the next recompute.
                if let Some(s) = self.sessions.get_mut(id) {
                    s.last_acknowledged_ms = crate::terminal::now_ms();
                }
                self.recompute_statuses();
                self.recompute_layout(hwnd);
                unsafe {
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            dashboard::TileAction::CreateSession => {
                let n = self.sessions.iter().count() + 1;
                let name = format!("session {}", n);
                let claude_cmd = crate::claude::ClaudeArgs::default().build_command_line();
                let cwd = std::env::current_dir().ok();
                let view =
                    SessionView::new(hwnd, WM_APP_TERM_OUTPUT, &name, claude_cmd, cwd.clone());
                let id = self.sessions.add(name, view, cwd, None);
                self.set_focused_session(Some(id));
                self.recompute_layout(hwnd);
                unsafe {
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            dashboard::TileAction::ResumeSession { session_id, cwd, name } => {
                // Resuming = spawn a new PTY running `claude --resume <id>`
                // in the orphan jsonl's project cwd. claude wires the
                // stored history into the new conversation. We tag the
                // live session with the original session id so the orphan
                // card backing that jsonl is suppressed from the grid.
                let claude_cmd = crate::claude::ClaudeArgs {
                    resume: Some(session_id.clone()),
                    ..Default::default()
                }
                .build_command_line();
                let view = SessionView::new(
                    hwnd,
                    WM_APP_TERM_OUTPUT,
                    &name,
                    claude_cmd,
                    Some(cwd.clone()),
                );
                let id = self.sessions.add(name, view, Some(cwd), Some(session_id));
                self.set_focused_session(Some(id));
                self.recompute_layout(hwnd);
                unsafe {
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
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

    /// Walk every session, recompute its status from its terminal's
    /// last-output / last-input timestamps and the per-session
    /// `last_acknowledged_ms`, and keep the attention queue in sync.
    /// Returns `true` if any status flipped.
    ///
    /// Before recomputing, the session that is *currently visible* in the
    /// main terminal slot has its `last_acknowledged_ms` bumped — it counts
    /// as continually "checked" while it's on screen, so it won't re-flag
    /// as NeedsAttention until the user moves on AND new output arrives.
    fn recompute_statuses(&mut self) -> bool {
        let now = crate::terminal::now_ms();

        // Determine which session is currently surfaced in MainTerminal —
        // either the user-focused one, or, in queue mode, the queue front.
        let visible_id = match self.view {
            PanelView::Dashboard { queue_mode } if queue_mode => {
                self.attention_queue.front().copied().or(self.focused_session)
            }
            PanelView::Dashboard { .. } | PanelView::Fullscreen => self.focused_session,
            PanelView::SidebarGrid => None,
        };
        if let Some(id) = visible_id {
            if let Some(s) = self.sessions.get_mut(id) {
                s.last_acknowledged_ms = now;
            }
        }

        let mut changed = false;
        let Self {
            sessions,
            attention_queue,
            ..
        } = self;
        let store = crate::claude_store::global();
        for session in sessions.iter_mut() {
            let term = session.session_view.terminal();
            let last_out = term.last_output_ms();
            let last_in = term.last_input_ms();
            let cursor_visible = term
                .grid_arc()
                .map(|g| g.lock().unwrap_or_else(|p| p.into_inner()).cursor_visible)
                .unwrap_or(true);
            // Cross-contamination guard: only feed jsonl signals to a live
            // PTY when we know the specific session UUID it belongs to
            // (i.e. it was launched via `claude --resume <id>`). For
            // freshly-spawned sessions we don't know which jsonl is theirs,
            // and the newest jsonl in the cwd may belong to a totally
            // different running claude process.
            let history = session
                .resumed_from_session_id
                .as_deref()
                .and_then(|id| store.lookup_by_session_id(id));
            let jsonl_mtime_ms = history.as_ref().and_then(|h| {
                h.last_modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_millis() as u64)
            });
            let jsonl_speaker = history.as_ref().map(|h| h.last_speaker);
            let prediction = crate::sessions::predict_status(crate::sessions::StatusInputs {
                last_output_ms: last_out,
                last_input_ms: last_in,
                last_acknowledged_ms: session.last_acknowledged_ms,
                now_ms: now,
                cursor_visible,
                jsonl_mtime_ms,
                jsonl_speaker,
            });
            let new_status = prediction.status;
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
        WM_NCHITTEST => {
            let xs = (lparam.0 & 0xFFFF) as i16 as i32;
            let ys = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut pt = POINT { x: xs, y: ys };
            let _ = ScreenToClient(hwnd, &mut pt);
            let mut client = RECT::default();
            let _ = GetClientRect(hwnd, &mut client);
            let dpi = GetDpiForWindow(hwnd).max(96);
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
                    for (tile, rect) in &panel.layout_cache {
                        if !point_in(rect, pt.x, pt.y) {
                            continue;
                        }
                        let hint = tile.cursor_at(pt.x, pt.y, *rect, dpi, &panel.sessions);
                        let cursor_id = match hint {
                            dashboard::CursorHint::IBeam => IDC_IBEAM,
                            dashboard::CursorHint::Hand => IDC_HAND,
                            dashboard::CursorHint::Arrow => IDC_ARROW,
                            dashboard::CursorHint::Default => {
                                break;
                            }
                        };
                        let cursor =
                            LoadCursorW(HINSTANCE::default(), cursor_id).unwrap_or_default();
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
            // Route to whichever tile contains the click. Cards-grid tiles
            // are special-cased so we can intercept scrollbar interactions.
            let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(panel) = panel_guard.as_mut() {
                let hit = panel
                    .layout_cache
                    .iter()
                    .find(|(_, r)| point_in(r, x, y))
                    .map(|(tile, rect)| (tile.clone(), *rect));
                if let Some((tile, rect)) = hit {
                    match tile {
                        dashboard::Tile::SessionCardsGrid {
                            include_orphans,
                            scroll_y,
                        } => {
                            let click = cards_tile::handle_lbutton_down_ex(
                                x,
                                y,
                                rect,
                                dpi,
                                &panel.sessions,
                                include_orphans,
                                scroll_y,
                            );
                            match click {
                                cards_tile::CardsClick::Card(action) => {
                                    panel.apply_tile_action(action, hwnd);
                                }
                                cards_tile::CardsClick::ScrollThumbGrab {
                                    grab_offset,
                                    bounds,
                                    content_h,
                                } => {
                                    panel.cards_scroll_drag = Some(CardsScrollDrag {
                                        grab_offset,
                                        bounds,
                                        content_h,
                                    });
                                    let _ = SetCapture(hwnd);
                                }
                                cards_tile::CardsClick::ScrollPageJump { delta } => {
                                    panel.cards_scroll_y = cards_tile::clamp_scroll(
                                        rect,
                                        dpi,
                                        &panel.sessions,
                                        include_orphans,
                                        panel.cards_scroll_y + delta,
                                    );
                                    panel.recompute_layout(hwnd);
                                    let _ = InvalidateRect(hwnd, None, false);
                                }
                                cards_tile::CardsClick::None => {}
                            }
                        }
                        _ => {
                            if let Some(action) =
                                tile.handle_lbutton_down(x, y, rect, dpi, &panel.sessions)
                            {
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
                if let Some(drag) = panel.cards_scroll_drag {
                    let dpi = GetDpiForWindow(hwnd).max(96);
                    let new_scroll = cards_tile::scroll_y_from_drag(
                        drag.bounds,
                        dpi,
                        drag.content_h,
                        y,
                        drag.grab_offset,
                    );
                    if new_scroll != panel.cards_scroll_y {
                        panel.cards_scroll_y = new_scroll;
                        panel.recompute_layout(hwnd);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                } else if let Some(_id) = panel.dragging_session {
                    if let Some(tile) = panel.focused_tile() {
                        tile.handle_mouse_move(x, y, &panel.sessions);
                    }
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(panel) = panel_guard.as_mut() {
                if panel.cards_scroll_drag.take().is_some() {
                    let _ = ReleaseCapture();
                }
                if panel.dragging_session.take().is_some() {
                    if let Some(tile) = panel.focused_tile() {
                        tile.handle_lbutton_up(&panel.sessions);
                    }
                }
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            // wparam high word = wheel delta (signed), positive scrolls up.
            let delta = ((wparam.0 >> 16) as i16) as i32;
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
                if let Some((dashboard::Tile::SessionCardsGrid { include_orphans, .. }, rect)) =
                    hit
                {
                    // 120 = WHEEL_DELTA. Translate to ~3 lines, ~card_h /
                    // 4 per notch, but scaled by DPI.
                    let scale = dpi as f64 / 96.0;
                    let line_px = (40.0 * scale).round() as i32;
                    let scroll_delta = -(delta * line_px) / 120;
                    let new_scroll = cards_tile::clamp_scroll(
                        rect,
                        dpi,
                        &panel.sessions,
                        include_orphans,
                        panel.cards_scroll_y + scroll_delta,
                    );
                    if new_scroll != panel.cards_scroll_y {
                        panel.cards_scroll_y = new_scroll;
                        panel.recompute_layout(hwnd);
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

            let width = client.right - client.left;
            let height = client.bottom - client.top;
            let mem_dc = CreateCompatibleDC(hdc);
            let mem_bmp = CreateCompatibleBitmap(hdc, width, height);
            let old_bmp = SelectObject(mem_dc, mem_bmp);

            paint_panel_chrome_bg(mem_dc, &client);
            {
                let panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(panel) = panel_guard.as_ref() {
                    for (tile, rect) in &panel.layout_cache {
                        tile.paint(
                            mem_dc,
                            *rect,
                            dpi,
                            &panel.sessions,
                            panel.focused_session,
                        );
                    }
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
            let panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(panel) = panel_guard.as_ref() {
                if let Some(tile) = panel.focused_tile() {
                    tile.handle_char(code, &panel.sessions);
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let vk = wparam.0 as u32;
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
            // Ctrl+Tab cycles the attention queue regardless of view mode.
            // 0x09 is VK_TAB. Rotates the front to the back (acking it) and
            // sets focused_session to the new front so the main slot
            // updates in both queue mode and regular mode.
            if ctrl_held && vk == 0x09 {
                let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(panel) = panel_guard.as_mut() {
                    let queue_len = panel.attention_queue.len();
                    let advanced = panel.cycle_attention_queue();
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
            // Ctrl+1 / Ctrl+2 / Ctrl+3 switch between the three panel
            // views. (0x31 / 0x32 / 0x33 are the virtual-key codes for
            // the digit keys.)
            if ctrl_held && (0x31..=0x33).contains(&vk) {
                let view = match vk {
                    0x31 => PanelView::Dashboard { queue_mode: false },
                    0x32 => PanelView::Fullscreen,
                    _ => PanelView::SidebarGrid,
                };
                switch_view(hwnd, view);
                return LRESULT(0);
            }
            let panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(panel) = panel_guard.as_ref() {
                if let Some(tile) = panel.focused_tile() {
                    if tile.handle_key_down(vk, &panel.sessions) {
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
            }
            let _ = InvalidateRect(hwnd, None, false);
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
                    if panel.recompute_statuses() {
                        panel.recompute_layout(hwnd);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = KillTimer(hwnd, TIMER_ATTENTION);
            *PANEL_HWND.lock().unwrap_or_else(|e| e.into_inner()) = 0;
            *PANEL.lock().unwrap_or_else(|e| e.into_inner()) = None;
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Cycle through the three panel views in order: Dashboard → Fullscreen →
/// SidebarGrid → Dashboard. Used by both the chrome button click and as the
/// fallback if a Ctrl+digit shortcut isn't recognized.
fn cycle_view(hwnd: HWND) {
    let mut panel_guard = PANEL.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(panel) = panel_guard.as_mut() {
        let next = match panel.view {
            PanelView::Dashboard { .. } => PanelView::Fullscreen,
            PanelView::Fullscreen => PanelView::SidebarGrid,
            PanelView::SidebarGrid => PanelView::Dashboard { queue_mode: false },
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
