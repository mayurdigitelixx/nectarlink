# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
  Builds the Windows app and packs it into an installer.

.DESCRIPTION
  1. Builds the release app (unless -SkipBuild).
  2. Stages it with the Qt libraries, plugins, QML modules and translations it
     uses (windeployqt), the Microsoft C++ runtime, and the license notices.
  3. Packs the folder into target\package\Nectarlink-<version>-x64-setup.exe
     with NSIS (unless -StageOnly).

  Needs Qt (qmake on PATH, or QT_ROOT_DIR / -QtRoot) and, for the
  installer, NSIS (makensis on PATH or in its usual folder).

.EXAMPLE
  .\scripts\package.ps1
  .\scripts\package.ps1 -StageOnly   # just the folder, to try the app as shipped
#>
param(
    [string]$QtRoot,
    [switch]$SkipBuild,
    [switch]$StageOnly
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Step($name) { Write-Host "`n==> $name" -ForegroundColor Cyan }

# Runs a native command, showing its output; fails on a non-zero exit.
# (Windows PowerShell treats anything on stderr as an error otherwise.)
function Invoke-Native([string]$what, [scriptblock]$command) {
    $saved = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { & $command 2>&1 | ForEach-Object { Write-Host "$_" } }
    finally { $ErrorActionPreference = $saved }
    if ($LASTEXITCODE -ne 0) { throw "$what failed (exit code $LASTEXITCODE)" }
}

# ---- Version, from the workspace manifest ----
$version = (Select-String -Path Cargo.toml -Pattern '^version = "([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
if (-not $version) { throw "can't read the version from Cargo.toml" }
# Windows file versions are numbers only: 0.0.1-beta.2 -> 0.0.1.
$numeric = ($version -split '[-+]')[0]

# ---- Qt ----
if (-not $QtRoot) { $QtRoot = $env:QT_ROOT_DIR }
if (-not $QtRoot) {
    $qmake = Get-Command qmake6, qmake -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($qmake) { $QtRoot = Split-Path -Parent (Split-Path -Parent $qmake.Source) }
}
if (-not $QtRoot) { $QtRoot = 'C:\Qt\6.12.0\msvc2022_64' }
$windeployqt = Join-Path $QtRoot 'bin\windeployqt.exe'
if (-not (Test-Path $windeployqt)) { throw "windeployqt not found under $QtRoot; pass -QtRoot" }

# ---- Build ----
if (-not $SkipBuild) {
    Step "Build"
    $env:PATH = "$(Join-Path $QtRoot 'bin');$env:PATH"
    Invoke-Native "the build" { cargo build --locked --release -p nectarlink-desktop -p nectarlink-vcam }
}
$exe = Join-Path $root 'target\release\nectarlink-desktop.exe'
if (-not (Test-Path $exe)) { throw "no release build at $exe" }
$vcamDll = Join-Path $root 'target\release\nectarlink_vcam.dll'
if (-not (Test-Path $vcamDll)) { throw "no release build at $vcamDll" }

# ---- Stage ----
Step "Stage"
$package = Join-Path $root 'target\package'
$stage = Join-Path $package 'Nectarlink'
if (Test-Path $stage) {
    $stagedVcam = Join-Path $stage 'nectarlink_vcam.dll'
    if (Test-Path $stagedVcam) {
        & regsvr32.exe /s /u $stagedVcam 2>$null | Out-Null
        try {
            Remove-Item $stagedVcam -Force -ErrorAction Stop
        } catch {
            # Windows Frame Server (svchost) may keep a mapped handle to the DLL;
            # NTFS allows moving a mapped file on the same volume so staging stays clean.
            $trashDir = Join-Path $root 'target\package-locked'
            New-Item -ItemType Directory -Force $trashDir | Out-Null
            Move-Item $stagedVcam (Join-Path $trashDir "nectarlink_vcam.$([DateTime]::UtcNow.Ticks).dll") -Force -ErrorAction SilentlyContinue
        }
    }
    Remove-Item $stage -Recurse -Force
}
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item $exe $stage
Copy-Item $vcamDll $stage

Invoke-Native "windeployqt" {
    & $windeployqt --verbose 0 --release --qmldir (Join-Path $root 'desktop\app\qml') `
        --no-system-d3d-compiler --no-opengl-sw --no-compiler-runtime --dir $stage (Join-Path $stage 'nectarlink-desktop.exe')
}

# Translations. windeployqt stages Qt's own .qm files (in translations/, so its
# file dialogs and standard buttons follow the system language). NectarLink's
# are compiled from the .ts files the repository keeps; they land beside the
# executable, which is where the app looks for them.
$translationSources = @()
if (Test-Path (Join-Path $root 'desktop\app\i18n')) {
    $translationSources = @(Get-ChildItem (Join-Path $root 'desktop\app\i18n') -Filter '*.ts')
}
if ($translationSources.Count -gt 0) {
    $lrelease = Join-Path $QtRoot 'bin\lrelease.exe'
    if (-not (Test-Path $lrelease)) { throw "lrelease not found under $QtRoot; pass -QtRoot" }
    foreach ($source in $translationSources) {
        $ts = $source.FullName
        $qm = Join-Path $stage "$($source.BaseName).qm"
        Invoke-Native "lrelease $($source.Name)" { & $lrelease $ts -qm $qm }
    }
}

# QtQuick.Dialogs brings every Controls style along. The file picker is
# Windows' own dialog; only Basic, Windows and FluentWinUI3 (the default
# style on Windows) can ever be loaded, so the rest is left out.
foreach ($style in 'Fusion', 'Imagine', 'Material', 'Universal') {
    Remove-Item (Join-Path $stage "Qt6QuickControls2$style*.dll") -Force
    Remove-Item (Join-Path $stage "qml\QtQuick\Controls\$style") -Recurse -Force -ErrorAction SilentlyContinue
}

# The C++ runtime, next to the app (Microsoft allows this app-local copy),
# so it runs on PCs without the Visual C++ Redistributable.
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
$crt = Get-ChildItem (Join-Path $vs 'VC\Redist\MSVC') -Directory |
    Where-Object { $_.Name -match '^\d' } | Sort-Object { [version]$_.Name } -Descending |
    ForEach-Object { Get-ChildItem (Join-Path $_.FullName 'x64') -Directory -Filter 'Microsoft.VC*.CRT' -ErrorAction SilentlyContinue } |
    Select-Object -First 1
if (-not $crt) { throw "can't find the Visual C++ runtime to ship" }
Copy-Item (Join-Path $crt.FullName '*.dll') $stage

# Licenses.
Copy-Item (Join-Path $root 'LICENSE') (Join-Path $stage 'LICENSE.txt')
Invoke-Native "writing the notices" { cargo xtask notices (Join-Path $stage 'THIRD-PARTY-NOTICES.txt') }

# The icon build.rs drew for the .exe, and Sparse Package assets for Windows Share sheet.
$icon = Get-ChildItem (Join-Path $root 'target\release\build') -Recurse -Filter 'nectarlink.ico' |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $icon) { throw "the app icon isn't built" }
$assetsDir = Join-Path $stage 'Assets'
New-Item -ItemType Directory -Force $assetsDir | Out-Null
Add-Type -AssemblyName System.Drawing
foreach ($spec in @(@{ Name = 'Square44x44Logo.png'; Size = 44 }, @{ Name = 'Square150x150Logo.png'; Size = 150 })) {
    $ico = New-Object System.Drawing.Icon($icon.FullName, $spec.Size, $spec.Size)
    $bmp = New-Object System.Drawing.Bitmap($spec.Size, $spec.Size)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.DrawIcon($ico, (New-Object System.Drawing.Rectangle(0, 0, $spec.Size, $spec.Size)))
    $g.Dispose()
    $bmp.Save((Join-Path $assetsDir $spec.Name), [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    $ico.Dispose()
}
$manifest = Get-Content (Join-Path $root 'desktop\installer\AppxManifest.xml') -Raw
$manifest = $manifest -replace 'Version="\d+\.\d+\.\d+\.\d+"', "Version=`"$numeric.0`""
Set-Content -Path (Join-Path $stage 'AppxManifest.xml') -Value $manifest -Encoding UTF8 -NoNewline

$size = (Get-ChildItem $stage -Recurse -File | Measure-Object Length -Sum).Sum / 1MB
Write-Host ("Staged {0:N1} MB in {1}" -f $size, $stage)
if ($StageOnly) { return }

# ---- Installer ----
Step "Installer"
$makensis = (Get-Command makensis -ErrorAction SilentlyContinue).Source
if (-not $makensis) { $makensis = Join-Path ${env:ProgramFiles(x86)} 'NSIS\makensis.exe' }
if (-not (Test-Path $makensis)) { throw "NSIS (makensis) not found" }
$out = Join-Path $package "Nectarlink-$version-x64-setup.exe"
Invoke-Native "makensis" {
    & $makensis /V2 "/DVERSION=$numeric" "/DDISPLAY_VERSION=$version" /DARCH=x64 "/DSTAGE=$stage" `
        "/DICON=$($icon.FullName)" "/DOUTFILE=$out" (Join-Path $root 'desktop\installer\nectarlink.nsi')
}
Write-Host ("Installer: {0} ({1:N1} MB)" -f $out, ((Get-Item $out).Length / 1MB))
