# make-installer.ps1 - T-10 Windows installer via Velopack (vpk).
#
# Packs the release-built panel exe into a Velopack Setup exe + RELEASES
# manifest (the GitHub-Release update channel). Components (syncthing,
# tailscale, docker) are NOT bundled: the panel installs them on demand
# through the T-08 fetch system, so the installer stays one small file.
#
# Usage: powershell -File scripts\make-installer.ps1 [-Version 2026.09.06-HHMM]
# Output: dist/Installer/<version>/*.exe + RELEASES
#
# Requires: vpk dotnet tool (dotnet tool install -g vpk).
# All comments ASCII per repo rules.

param(
    [string]$Version = (Get-Date -Format "yyyy.MM.dd-HHmm")
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$exe  = Join-Path $root "panel\target\release\flygo.exe"
$out  = Join-Path $root "dist\Installer"

if (-not (Test-Path $exe)) {
    throw "panel exe not found at $exe - run: cargo build --release (in panel/)"
}

$vpk = Get-Command vpk -ErrorAction SilentlyContinue
if (-not $vpk) {
    # dotnet global tools live under %USERPROFILE%\.dotnet\tools
    $candidate = Join-Path $env:USERPROFILE ".dotnet\tools\vpk.exe"
    if (Test-Path $candidate) { $vpk = $candidate } else { throw "vpk not found - run: dotnet tool install --global vpk" }
    $vpk = $candidate
}

# Velopack wants strict 3-part SemVer. Our Version file is YYYY.MM.DD-HHMM,
# so fold DD+HHMM into the patch component: 2026.09.06-0430 -> 2026.9.60430
$p = $Version -split '[.-]'   # 2026, 09, 06, 0430
$packVersion = "{0}.{1}.{2}" -f ([int]$p[0]), ([int]$p[1]), ([int]($p[2] + $p[3]))

New-Item -ItemType Directory -Force -Path $out | Out-Null

# Stage ONLY the panel exe: vpk would otherwise swallow the whole cargo
# target/ dir (292MB of build artifacts for a 4.5MB exe). The finished
# install stays "one panel exe + tray", per the product philosophy.
$stage = Join-Path $env:TEMP "flygo-vpk-stage"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force -Path $stage | Out-Null
Copy-Item $exe (Join-Path $stage "flygo.exe")

& $vpk pack `
    --packId FlyGo `
    --packVersion $packVersion `
    --packDir $stage `
    --mainExe flygo.exe `
    --packTitle "FlyGo" `
    --outputDir $out

if ($LASTEXITCODE -ne 0) { throw "vpk pack failed ($LASTEXITCODE)" }

Write-Host ""
Write-Host "installer ready:" Get-ChildItem $out | Select-Object -ExpandProperty FullName
Write-Host "update channel: upload the whole dist/Installer folder to a GitHub Release (Velopack HttpSource)."
