![Windows](https://img.shields.io/badge/platform-Windows-blue)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

# Claude Manager

> **Forked from [Claude Code Usage Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor) by Code Zeno Pty Ltd.** This fork keeps the original taskbar usage widget and adds a multi-session dashboard that runs your Claude Code sessions in embedded terminals.

![Screenshot](.github/animation.gif)

## What it does

**Usage monitor (inherited from upstream):**
- Taskbar widget showing your 5-hour and 7-day usage windows with live countdowns
- System tray icon with a color-coded percentage badge
- Right-click options for refresh frequency, language, "Start with Windows", updates

**Session manager (new in this fork):**
- Dashboard running several `claude` sessions side by side, each in its own embedded terminal
- Project tree of every directory you've used claude in, listing that project's past conversations — start a new one in any of them, or resume an old one
- Live mini-terminal previews on the card grid and a "needs attention" list
- Per-session status (thinking / assistant / human / tool / idle) read from claude's own session transcript rather than guessed from the terminal

The manager drives the sessions it starts. Sessions you launch yourself in your own terminal are left alone — the manager doesn't discover, attach to, or track them, though their transcripts do show up as resumable history in the project tree.

## Requirements

- Windows 10 or Windows 11
- Claude Code (CLI or App) installed, authenticated, and on PATH

## Install

Download the latest `claude-manager-<version>-x86_64.msi` from the [Releases page](https://github.com/Helveg/Claude-Code-Monitor/releases) and run it. The installer:

- Places `claude-manager.exe` in `Program Files\claude-manager\bin`
- Adds a "Claude Code Monitor" Start Menu shortcut
- Registers in Add/Remove Programs (uninstall reverses everything)

The original non-fork version is still available via WinGet:
```powershell
winget install CodeZeno.ClaudeCodeUsageMonitor
```

## Usage

Open the manager from the Start Menu ("Claude Code Monitor"). It starts with no sessions; the nav lists your projects, and the **+** on a project row starts a session in that directory. Each session gets its own terminal, and the dashboard shows every session's status at a glance:

- **project tree** — one row per project (any directory claude has been run in), expanding to its conversations. A filled dot is a session running here; click it to focus its terminal. A hollow dot is a past conversation; click it to resume it. **+** starts a fresh session in that project.
- **terminal grid** — every session as a live terminal, `cols x rows` of your choosing (right-click the view button). Click a cell to type into it; the project tree occupies the first cell so you can start sessions without leaving the grid
- **needs attention** — sessions where claude has finished and is waiting on you

## Architecture, briefly

```
claude-manager.exe
  ├── Terminal 1 ── ConPTY ── cmd.exe /c claude --session-id <uuid-1>
  ├── Terminal 2 ── ConPTY ── cmd.exe /c claude --session-id <uuid-2>
  ├── ClaudeStore ─── reads ~/.claude/projects/**/<uuid-N>.jsonl (tracked ids only)
  └── ProjectStore ── reads ~/.claude/projects/**  (every transcript, head only)
```

- **Terminal** — a hand-rolled ConPTY host plus VT parser producing a `Grid` the panel paints. One per session, spawned by the manager.
- **Session UUIDs** — the manager generates the UUID and passes `--session-id` at spawn, so it always knows which transcript belongs to which terminal without having to discover it after the fact.
- **ClaudeStore** — a 1 Hz scanner over `~/.claude/projects/`. It only ever opens transcripts whose filename matches a session id the manager spawned; everything else is ignored. The transcript's last speaker drives the status buckets, which is far steadier than inferring state from terminal output.
- **ProjectStore** — a 5 s scanner that builds the nav tree. It reads the *head* of every transcript for the two facts a nav row needs: the `cwd` the conversation ran in (which project it belongs to) and its opening prompt (what to call it). Project directories whose transcripts have been pruned are recovered by matching claude's sanitized directory name against the real filesystem, so a project is only listed if we can name a directory that still exists.

## Build from source

Requires the Rust toolchain (`rustup`) and, for the MSI, [WiX Toolset 3.14](https://github.com/wixtoolset/wix3/releases) — the portable binaries zip works (no admin install needed).

```powershell
git clone https://github.com/Helveg/Claude-Code-Monitor
cd Claude-Code-Monitor

# Just the binary
cargo build --release
# → target\release\claude-manager.exe

# Full MSI installer
cargo install cargo-wix --locked
cargo wix
# → target\wix\claude-manager-<version>-x86_64.msi
```

Run the test suite:
```powershell
cargo test --lib
```

Launch a local build without hunting for the exe — this pins a Start Menu (and with
`-Desktop`, a desktop) shortcut to the binary in `target\`:
```powershell
# debug build; add -Release for target\release, -Build to compile first
pwsh -File scripts\install-dev-shortcut.ps1 -Desktop
# undo with -Remove
```

## Diagnostics

```powershell
claude-manager --diagnose
# → %TEMP%\claude-manager.log
```

Settings live in `%APPDATA%\ClaudeManager\settings.json`. The session jsonls written by claude itself live in `%USERPROFILE%\.claude\projects\<encoded-cwd>\<session-id>.jsonl`.

## Privacy

Same posture as upstream: open source, no analytics, no third-party backend. The app reads your Claude Code credentials from `~/.claude/.credentials.json` (or your WSL equivalent), talks to Anthropic's endpoints to read usage, and optionally talks to GitHub for self-update checks. Session transcripts are read from disk to derive status and never leave your machine.

## Credits

The taskbar widget, tray icon, usage polling, and overall app shell are from **[Claude Code Usage Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor)** by Code Zeno Pty Ltd. The session manager in this fork is additive — none of upstream's behavior is removed.

## License

MIT.
