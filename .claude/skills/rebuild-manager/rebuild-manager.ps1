<#
.SYNOPSIS
    Rebuild and relaunch claude-manager from outside its own process tree.

.DESCRIPTION
    claude-manager hosts claude sessions in its terminal tiles, so the agent
    asking for the rebuild is usually running inside the very process step 1
    kills. That is why this exists as a detached script rather than a few
    inline commands: launched with Start-Process, it survives the kill and
    carries on building and relaunching after its caller is gone.

    Steps: wait out a grace period so the caller's last message reaches the
    user, stop claude-manager, cargo build, relaunch. Everything is logged to
    $env:TEMP\claude-manager-rebuild.log.

.PARAMETER RepoRoot
    Repository to build. Defaults to the repo this script is checked into.

.PARAMETER GraceSeconds
    Delay before the kill, so the caller can finish its turn.

.PARAMETER BuildProfile
    'auto' (default) builds whichever profile is currently running, falling
    back to debug when nothing is.
#>
[CmdletBinding()]
param(
    [string]$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..')).Path,
    [int]$GraceSeconds = 10,
    [ValidateSet('auto', 'debug', 'release')]
    [string]$BuildProfile = 'auto'
)

$ErrorActionPreference = 'Stop'
$log = Join-Path $env:TEMP 'claude-manager-rebuild.log'

function Write-Log($msg) {
    "$((Get-Date).ToString('HH:mm:ss'))  $msg" | Tee-Object -FilePath $log -Append | Out-Null
}

"=== rebuild started $(Get-Date) ===" | Set-Content -Path $log
Write-Log "repo: $RepoRoot"

# Read the running instance before killing it: its path tells us which
# profile to build, and its command line carries flags like --diagnose that
# the relaunch should keep.
$extraArgs = ''
$running = $null
try {
    $running = Get-CimInstance Win32_Process -Filter "Name = 'claude-manager.exe'" |
        Select-Object -First 1
} catch {
    Write-Log "could not query running process: $_"
}

if ($running) {
    Write-Log "running: $($running.ExecutablePath) (pid $($running.ProcessId))"
    if ($BuildProfile -eq 'auto') {
        $BuildProfile = if ($running.ExecutablePath -match '\\release\\') { 'release' } else { 'debug' }
    }
    if ($running.CommandLine) {
        # Strip the exe token; whatever follows is the flag list.
        if ($running.CommandLine -match '^\s*"([^"]+)"\s*(.*)$') { $extraArgs = $Matches[2].Trim() }
        elseif ($running.CommandLine -match '^\s*(\S+)\s*(.*)$') { $extraArgs = $Matches[2].Trim() }
    }
} else {
    Write-Log 'claude-manager was not running'
}
if ($BuildProfile -eq 'auto') { $BuildProfile = 'debug' }
Write-Log "profile: $BuildProfile$(if ($extraArgs) { "  args: $extraArgs" })"

$exe = Join-Path $RepoRoot "target\$BuildProfile\claude-manager.exe"

# Give the caller's last message time to reach the user before their
# terminal disappears.
Start-Sleep -Seconds $GraceSeconds

if ($running) {
    Write-Log 'stopping claude-manager'
    # Terminating the manager closes its pseudoconsole handles, which is what
    # tells the claude sessions inside it to exit. claude.exe is never killed
    # directly — that would take down sessions running outside the manager.
    Stop-Process -Name claude-manager -Force -ErrorAction SilentlyContinue
    for ($i = 0; $i -lt 20; $i++) {
        if (-not (Get-Process claude-manager -ErrorAction SilentlyContinue)) { break }
        Start-Sleep -Milliseconds 250
    }
}

Set-Location $RepoRoot
$buildArgs = @('build')
if ($BuildProfile -eq 'release') { $buildArgs += '--release' }
Write-Log "cargo $($buildArgs -join ' ')"
& cargo @buildArgs 2>&1 | Tee-Object -FilePath $log -Append
$built = $LASTEXITCODE
Write-Log "cargo build exit=$built"

if ($built -ne 0 -and -not (Test-Path $exe)) {
    Write-Log 'build failed and no binary to fall back on — not relaunching'
    exit 1
}
if ($built -ne 0) {
    Write-Log 'build failed — relaunching the previous binary'
}

# The tool shell these vars came from is not the user's environment, and the
# manager passes its own environment to every session it spawns: a leaked
# NO_COLOR would strip colour out of every terminal in the panel.
Remove-Item Env:NO_COLOR -ErrorAction SilentlyContinue
Get-ChildItem Env: | Where-Object { $_.Name -like 'CLAUDE_CODE_*' } | ForEach-Object {
    Remove-Item "Env:$($_.Name)" -ErrorAction SilentlyContinue
}

Write-Log "launching $exe"
if ($extraArgs) {
    Start-Process -FilePath $exe -ArgumentList $extraArgs -WorkingDirectory $RepoRoot
} else {
    Start-Process -FilePath $exe -WorkingDirectory $RepoRoot
}

Start-Sleep -Seconds 2
$now = Get-Process claude-manager -ErrorAction SilentlyContinue
if ($now) {
    Write-Log "claude-manager running (pid $($now.Id -join ', '))"
} else {
    Write-Log 'claude-manager did not come up — a second instance may hold the single-instance mutex'
}
Write-Log '=== done ==='
