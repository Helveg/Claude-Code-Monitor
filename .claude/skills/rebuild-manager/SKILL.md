---
name: rebuild-manager
description: Rebuild and relaunch claude-manager so a code change shows up in the running app. Use when the user says "rebuild", "restart the manager", "relaunch it", "fire it", asks to see a change working, or invokes /rebuild-manager. READ THIS BEFORE running cargo build or killing claude-manager — your own claude session is probably running inside claude-manager, so stopping it ends the session, and `cargo build` fails while it is running. Never kill or restart it without asking first.
---

# Rebuilding claude-manager

## The trap

`claude-manager.exe` hosts claude sessions in its terminal tiles. **The
session you are answering from is very likely one of them.** That produces two
failures that look like tooling problems and are not:

1. **`cargo build` fails while the manager is running:**

   ```
   error: failed to remove file `target\debug\claude-manager.exe`
   Caused by: Access is denied. (os error 5)
   ```

   That is the running manager holding its own binary, not a stale lock and
   not a permissions bug. Do not retry it, do not `--target-dir` around it as
   a workaround, and **do not kill the process to get past it.**

2. **You cannot kill it and then build.** The kill takes your own session down
   with it, so nothing survives to run the build. Any "stop it, build, start
   it" sequence you write inline dies at step one.

## Verify without restarting

This is the default. It catches everything short of "does it look right on
screen", and costs the user nothing:

```powershell
cargo check --all-targets
cargo test --lib
```

If linking specifically matters (new crate feature, new extern symbol), link
into a throwaway directory so the running binary is untouched — this is the
one legitimate use of `--target-dir`:

```powershell
cargo build --target-dir "$env:TEMP\claude-manager-linkcheck"
```

Say plainly that the change is verified but not visually confirmed, and that
it lands on the next restart. Do not restart to "check" something a test
could have told you.

## When the user says go

Ask first — "want me to rebuild and relaunch? it'll take this session down
with it" — and wait for an answer. On "yes" / "fire it", launch the script
**detached**, so it outlives the kill:

```powershell
$s = Resolve-Path '.claude\skills\rebuild-manager\rebuild-manager.ps1'
Start-Process pwsh -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File',$s -WindowStyle Hidden
```

Then tell the user what to look at when it comes back, and stop. Your session
ends about ten seconds later; don't schedule follow-up work behind it.

The script waits out a grace period so your last message reaches the user,
stops the manager, runs `cargo build`, and relaunches. It builds whichever
profile was running (debug unless the path says release), preserves flags like
`--diagnose` from the old command line, and strips `NO_COLOR` / `CLAUDE_CODE_*`
out of the environment first — those come from the agent's tool shell, not the
user's, and the manager passes its environment to every session it spawns.

Everything is logged to `%TEMP%\claude-manager-rebuild.log`, including the
cargo output. If the build fails it relaunches the previous binary rather than
leaving the user with nothing.

## Running it without an agent

The script is a normal PowerShell script and takes `-RepoRoot`,
`-GraceSeconds` and `-BuildProfile` (`auto` / `debug` / `release`). From a
terminal **outside** the manager, run it in the foreground:

```powershell
pwsh -NoProfile -File .claude\skills\rebuild-manager\rebuild-manager.ps1 -GraceSeconds 0
```

From a terminal **inside** the manager, it has to be detached or it dies with
its own shell at the kill:

```powershell
$s = Resolve-Path '.claude\skills\rebuild-manager\rebuild-manager.ps1'
Start-Process pwsh -ArgumentList '-NoProfile','-File',$s -WindowStyle Hidden
```

## Coming back

The manager saves its open sessions to `settings.json` once a second, so the
conversation that fired the rebuild returns as a **resume card** in the panel —
the user clicks Resume to pick it up. If it doesn't appear, it is still in the
nav under its project as a history row.

## Hard rules

- **Never `Stop-Process claude`.** `claude.exe` hosts sessions that may be
  running outside the manager entirely; killing it loses the user's work.
  Stopping `claude-manager` is enough — closing its pseudoconsoles is what
  tells the sessions inside it to exit cleanly.
- **Never restart the manager unprompted.** Not to check your work, not to
  "make sure it still runs". Ask, every time.
- **Don't run the script from inside a tool call you then wait on.** It is
  fire-and-forget by design; there is nothing to await.
