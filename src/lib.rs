pub mod action_window;
pub mod agent_state;
pub mod claude;
pub mod claude_store;
pub mod dashboard;
pub mod diagnose;
pub mod grid_tile;
pub mod highlight;
pub mod localization;
pub mod models;
pub mod native_interop;
pub mod poller;
pub mod project_tree;
pub mod projects;
pub mod resume_card;
pub mod session_view;
pub mod sessions;
pub mod terminal;
pub mod terminal_view;
pub mod theme;
pub mod tray_icon;
pub mod updater;
pub mod window;

/// Entry point shared between the GUI binary (`src/main.rs`) and any
/// future bins that want to launch the full monitor — keeps the bootstrap
/// in one place so we don't drift if a flag is added.
pub fn run() {
    // Hint to TUI apps spawned in our embedded terminal that they're talking
    // to a real ANSI-capable terminal. Without TERM set, libraries like Ink
    // (used by claude code) fall back to a degraded mode where they render
    // the cursor themselves as a reverse-video cell — which leaks visible
    // "ghost cursor" blocks when the app moves on without cleaning up.
    std::env::set_var("TERM", "xterm-256color");
    std::env::set_var("COLORTERM", "truecolor");

    let args: Vec<String> = std::env::args().collect();
    let diagnose_enabled = args.iter().any(|arg| arg == "--diagnose");
    let debug_render_enabled = args.iter().any(|arg| arg == "--debug-render");
    if diagnose_enabled {
        match diagnose::init() {
            Ok(path) => diagnose::log(format!(
                "startup args={args:?} log_path={}",
                path.display()
            )),
            Err(error) => {
                // Logging may not be available yet, but keep startup behavior unchanged.
                let _ = error;
            }
        }
    }
    if debug_render_enabled {
        highlight::DEBUG_RENDER_ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    if let Some(exit_code) = updater::handle_cli_mode(&args) {
        if diagnose_enabled {
            diagnose::log(format!("cli mode exited with code {exit_code}"));
        }
        std::process::exit(exit_code);
    }

    // Start the project scanner before the UI exists so the nav tree is
    // already populated by the time the user opens the panel. The scan runs
    // on its own thread; this call just spawns it.
    projects::global();

    if diagnose_enabled {
        diagnose::log("entering window::run");
    }
    window::run();
}
