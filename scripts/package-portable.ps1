# Builds the release exe and assembles a portable folder + zip:
#   src-tauri/target/portable/putcherfortiktok-<version>/      (run from here)
#   src-tauri/target/portable/putcherfortiktok-<version>-win64.zip
#
# Needs: scripts/fetch-ffmpeg.ps1 already run; optionally rife-ncnn-vulkan
# unpacked into src-tauri/binaries/rife (exe, vcomp140.dll, rife-v4.6).
#
# Usage: powershell -ExecutionPolicy Bypass -File scripts/package-portable.ps1

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$tauri = Join-Path $root 'src-tauri'
# Regex instead of ConvertFrom-Json: the cmdlet is broken on some Windows installs.
$confText = Get-Content (Join-Path $tauri 'tauri.conf.json') -Raw
if ($confText -notmatch '"version"\s*:\s*"([^"]+)"') { throw 'version not found in tauri.conf.json' }
$name = "putcherfortiktok-$($Matches[1])"
$triple = 'x86_64-pc-windows-gnu'

Push-Location $root
try {
    npm run tauri build -- --no-bundle
    if ($LASTEXITCODE -ne 0) { throw 'tauri build failed' }
} finally { Pop-Location }

$out = Join-Path $tauri "target\portable\$name"
if (Test-Path $out) { [System.IO.Directory]::Delete($out, $true) }
New-Item -ItemType Directory -Force $out | Out-Null

$rel = Join-Path $tauri 'target\release'
$bin = Join-Path $tauri 'binaries'
Copy-Item (Join-Path $rel 'putcherfortiktok.exe') $out
Copy-Item (Join-Path $rel 'WebView2Loader.dll') $out
Copy-Item (Join-Path $bin "ffmpeg-$triple.exe") (Join-Path $out 'ffmpeg.exe')
Copy-Item (Join-Path $bin "ffprobe-$triple.exe") (Join-Path $out 'ffprobe.exe')
Copy-Item (Join-Path $root 'packaging\README.txt') $out

$rife = Join-Path $bin 'rife'
if (Test-Path (Join-Path $rife 'rife-ncnn-vulkan.exe')) {
    $rout = Join-Path $out 'rife'
    New-Item -ItemType Directory -Force $rout | Out-Null
    foreach ($f in 'rife-ncnn-vulkan.exe', 'vcomp140.dll', 'LICENSE') { Copy-Item (Join-Path $rife $f) $rout }
    Copy-Item (Join-Path $rife 'rife-v4.6') $rout -Recurse
} else {
    Write-Host 'RIFE not found: packaging without neural frame interpolation.'
}

$zip = Join-Path $tauri "target\portable\$name-win64.zip"
if (Test-Path $zip) { [System.IO.File]::Delete($zip) }
# tar.exe ships with Windows 10+ and writes zip with -a.
& "$env:SystemRoot\System32\tar.exe" -a -c -f $zip -C (Split-Path $out) $name
if ($LASTEXITCODE -ne 0) { throw 'zip failed' }
Write-Host "Portable folder: $out"
Write-Host "Archive:         $zip ($([math]::Round((Get-Item $zip).Length / 1MB)) MB)"
