<#
.SYNOPSIS
    Creates a Start Menu (and optionally Desktop) shortcut to the locally built
    claude-manager.exe.

.DESCRIPTION
    The shortcut points straight at the compiled binary — never at `cargo run`, so
    launching it does not need a toolchain or a console window. Re-run this script
    after moving the checkout; the .lnk stores an absolute path.

    claude-manager holds a Global\ClaudeManager mutex, so starting a second copy
    exits silently instead of opening a second dashboard.

.PARAMETER Release
    Point the shortcut at target\release instead of target\debug.

.PARAMETER Desktop
    Also drop a copy of the shortcut on the Desktop.

.PARAMETER Build
    Run `cargo build` for the selected profile before creating the shortcut.

.PARAMETER Remove
    Delete the shortcuts this script creates instead of creating them.

.EXAMPLE
    pwsh -File scripts\install-dev-shortcut.ps1 -Build -Desktop
#>
[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$Desktop,
    [switch]$Build,
    [switch]$Remove
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$profileName = if ($Release) { 'release' } else { 'debug' }
$exePath = Join-Path $repoRoot "target\$profileName\claude-manager.exe"

$shortcutName = if ($Release) { 'Claude Manager.lnk' } else { 'Claude Manager (dev).lnk' }
$startMenuDir = Join-Path ([Environment]::GetFolderPath('Programs')) 'Claude Manager'
$startMenuLink = Join-Path $startMenuDir $shortcutName
$desktopLink = Join-Path ([Environment]::GetFolderPath('Desktop')) $shortcutName

if ($Remove) {
    foreach ($link in @($startMenuLink, $desktopLink)) {
        if (Test-Path -LiteralPath $link) {
            Remove-Item -LiteralPath $link -Force
            Write-Host "Removed $link"
        }
    }
    if ((Test-Path -LiteralPath $startMenuDir) -and -not (Get-ChildItem -LiteralPath $startMenuDir -Force)) {
        Remove-Item -LiteralPath $startMenuDir -Force
    }
    return
}

if ($Build) {
    $cargoArgs = @('build')
    if ($Release) { $cargoArgs += '--release' }
    Write-Host "cargo $($cargoArgs -join ' ')"
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
}

if (-not (Test-Path -LiteralPath $exePath)) {
    throw "$exePath not found. Build it first (cargo build$(if ($Release) { ' --release' })) or pass -Build."
}

if (-not (Test-Path -LiteralPath $startMenuDir)) {
    New-Item -ItemType Directory -Path $startMenuDir | Out-Null
}

$shell = New-Object -ComObject WScript.Shell
try {
    $targets = @($startMenuLink)
    if ($Desktop) { $targets += $desktopLink }

    foreach ($link in $targets) {
        $shortcut = $shell.CreateShortcut($link)
        $shortcut.TargetPath = $exePath
        $shortcut.WorkingDirectory = $repoRoot
        # The icon is embedded in the PE by build.rs, so index 0 of the exe is it.
        $shortcut.IconLocation = "$exePath,0"
        $shortcut.Description = "Claude Manager ($profileName build from $repoRoot)"
        $shortcut.Save()
        Write-Host "Created $link -> $exePath"
    }
} finally {
    [void][Runtime.InteropServices.Marshal]::ReleaseComObject($shell)
}

Write-Host "Press Start and type 'Claude Manager' to launch it."
