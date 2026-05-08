![Windows](https://img.shields.io/badge/platform-Windows-blue)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

# Claude Manager

> **Forked from [Claude Code Usage Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor) by Code Zeno Pty Ltd.** This fork keeps the original taskbar usage widget and adds a multi-session dashboard, plus a transparent shim around `claude.exe` that lets multiple terminals (and the manager itself) attach to a single live Claude Code session — like `tmux attach`, but for `claude`.

![Screenshot](.github/animation.gif)

## What it does

**Usage monitor (inherited from upstream):**
- Taskbar widget showing your 5-hour and 7-day usage windows with live countdowns
- System tray icon with a color-coded percentage badge
- Right-click options for refresh frequency, language, "Start with Windows", updates

**Session manager (new in this fork):**
- Dashboard listing every live `claude` session — terminal-launched OR manager-launched
- **Attach from the dashboard** to any session running in any terminal; type and watch in either window with the changes mirrored
- **`claude.exe` shim** transparently wraps the real claude. You don't change how you start sessions; the shim handles ownership, registry registration, and subscriber dispatch silently
- **Refcount-based lifecycle**: closing your terminal doesn't kill an in-progress claude as long as the manager (or another terminal) is still attached. Last subscriber leaves → claude exits cleanly
- **Manager rediscovers sessions on restart** by walking `\\.\pipe\ccmonitor-session-*` — close and reopen the manager without losing track of what's running

## Requirements

- Windows 10 or Windows 11
- Claude Code (CLI or App) installed and authenticated

## Install

Download the latest `claude-manager-<version>-x86_64.msi` from the [Releases page](https://github.com/Helveg/Claude-Code-Monitor/releases) and run it. The installer:

- Places `claude-manager.exe` and `claude.exe` (the shim) in `Program Files\claude-manager\bin`
- Prepends that directory to system PATH so `claude` from any new shell goes through the shim
- Adds a "Claude Code Monitor" Start Menu shortcut
- Registers in Add/Remove Programs (uninstall reverses everything)

The original non-fork version is still available via WinGet:
```powershell
winget install CodeZeno.ClaudeCodeUsageMonitor
```

## Usage

After install, open a new shell so the updated PATH is in effect, then:

```powershell
claude            # transparently goes through the shim
```

The first invocation generates a session UUID, registers with the manager (best-effort), and spawns real claude inside a ConPTY. The shim is invisible — claude works exactly as before.

To attach a second window to the same session:

```powershell
claude --resume <uuid>
```

Both windows mirror the same conversation. Type in either; both update.

The manager (Start Menu → "Claude Code Monitor") shows every live session in its dashboard. Click a card to open it as another subscriber on the same session.

## Architecture, briefly

```
Terminal A  ─── stdin/stdout ───┐
                                ├── claude.exe (shim, OWNER)
                                │      └── real claude (in ConPTY)
                                │      └── \\.\pipe\ccmonitor-session-<uuid>
                                │              ▲           ▲
Terminal B (claude --resume) ───┘              │           │
                                               │           │
                              Manager dashboard  ──────────┘
                              (subscribes via the same pipe)
```

- **Owner shim**: spawns real claude in a ConPTY, fans output to its local terminal + every connected subscriber, merges keystrokes from all subscribers into claude's stdin, manages min-of-all-clients resize.
- **Subscriber shim**: when `claude --resume <uuid>` finds the per-session pipe already open, it skips spawning real claude and becomes a relay client. Its terminal mirrors owner output and forwards keystrokes upstream.
- **Per-session pipe** (`\\.\pipe\ccmonitor-session-<uuid>`) carries length-prefixed framed bytes: `O`utput, `I`nput, `R`esize, `H`ello, `Q`uery, `M`etadata. Single-thread-per-pipe with `PeekNamedPipe` + non-blocking channel multiplexing — Windows synchronous-mode pipe I/O serializes per pipe, so naive multi-threading deadlocks.
- **Registry pipe** (`\\.\pipe\ccmonitor-registry`) tracks live sessions for the dashboard; manager rediscovers by enumerating per-session pipes on startup.
- **Refcount lifecycle**: `(terminal_alive ? 1 : 0) + subscribers.len()`. Hit zero → terminate claude.

## Build from source

Requires the Rust toolchain (`rustup`) and, for the MSI, [WiX Toolset 3.14](https://github.com/wixtoolset/wix3/releases) — the portable binaries zip works (no admin install needed).

```powershell
git clone https://github.com/Helveg/Claude-Code-Monitor
cd Claude-Code-Monitor

# Just the binaries
cargo build --release
# → target\release\claude-manager.exe
# → target\release\claude.exe (shim)

# Full MSI installer
cargo install cargo-wix --locked
cargo wix
# → target\wix\claude-manager-<version>-x86_64.msi
```

Run the test suite:
```powershell
cargo test --lib
```

## Diagnostics

Manager:
```powershell
claude-manager --diagnose
# → %TEMP%\claude-manager.log
```

Shim (always logs when set):
```powershell
$env:CCMONITOR_SHIM_LOG = "$env:TEMP\ccmonitor-shim.log"
claude
```

Settings live in `%APPDATA%\ClaudeManager\settings.json`. The session jsonls written by claude itself live in `%USERPROFILE%\.claude\projects\<encoded-cwd>\<session-id>.jsonl`.

## Privacy

Same posture as upstream: open source, no analytics, no third-party backend. The app reads your Claude Code credentials from `~/.claude/.credentials.json` (or your WSL equivalent), talks to Anthropic's endpoints to read usage, and optionally talks to GitHub for self-update checks. The shim does not read or transmit conversation content; it just routes bytes between processes locally.

## Credits

The taskbar widget, tray icon, usage polling, and overall app shell are from **[Claude Code Usage Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor)** by Code Zeno Pty Ltd. The session manager / shim architecture in this fork is additive — none of upstream's behavior is removed.

## License

MIT.
