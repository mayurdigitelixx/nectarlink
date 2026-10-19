# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
  Opens the Windows app's main window for a few seconds and fails on any
  QML warning or error.

.DESCRIPTION
  Runs nectarlink-desktop.exe --ui-check with a new data folder under
  target\ui-check, so the user's own data is never touched, and without
  USB (NECTARLINK_NO_ADB=1). The app shows the main window, quits after a
  few seconds and exits with an error if QML reported a problem, such as a
  component that didn't load. The QML tests can't catch that for the app's
  windows, which need the Rust singletons and NativeWindow.

  -Exe defaults to the staged package (scripts\package.ps1), which has its
  Qt libraries next to it; for target\release, pass -Exe and have Qt's bin
  folder on PATH.

.EXAMPLE
  .\scripts\package.ps1 -StageOnly; .\scripts\ui-check.ps1
#>
param(
    [string]$Exe
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
if (-not $Exe) { $Exe = Join-Path $repo 'target\package\Nectarlink\nectarlink-desktop.exe' }
if (-not (Test-Path $Exe)) { throw "$Exe not found; run scripts\package.ps1 -StageOnly first, or pass -Exe" }

$data = Join-Path $repo 'target\ui-check'
if (Test-Path $data) { Remove-Item -Recurse -Force $data }
New-Item -ItemType Directory -Force $data | Out-Null
$stderr = Join-Path $data 'stderr.txt'

$env:NECTARLINK_NO_ADB = '1'
$app = Start-Process $Exe -ArgumentList '--ui-check', '--data-dir', "`"$data`"" `
    -RedirectStandardError $stderr -PassThru -NoNewWindow
# Without the handle, ExitCode stays empty after the process exits.
$null = $app.Handle
if (-not $app.WaitForExit(120000)) {
    $app.Kill()
    throw "the app didn't quit within 2 minutes"
}
Get-Content $stderr | Write-Host
if ($app.ExitCode -ne 0) { throw "UI check failed (exit code $($app.ExitCode))" }
