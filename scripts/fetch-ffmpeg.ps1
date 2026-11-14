# Downloads an ffmpeg/ffprobe build with NVENC/CUDA support (BtbN FFmpeg-Builds)
# and places them in src-tauri/binaries as Tauri sidecars:
#   ffmpeg-<target-triple>.exe, ffprobe-<target-triple>.exe
#
# Usage:  powershell -ExecutionPolicy Bypass -File scripts/fetch-ffmpeg.ps1 [-Version 9.0] [-Triple x86_64-pc-windows-gnu] [-Force]

param(
    [string]$Version = '9.0',
    [string]$Triple = '',
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$root = Split-Path -Parent $PSScriptRoot
$binDir = Join-Path $root 'src-tauri\binaries'

if (-not $Triple) {
    # The sidecar suffix must match the Rust target the app is built for.
    Push-Location (Join-Path $root 'src-tauri')
    try {
        $hostLine = (rustc -vV | Select-String '^host:').ToString()
        $Triple = $hostLine.Split(':')[1].Trim()
    } finally { Pop-Location }
}

$ffmpegOut = Join-Path $binDir "ffmpeg-$Triple.exe"
$ffprobeOut = Join-Path $binDir "ffprobe-$Triple.exe"
if ((Test-Path $ffmpegOut) -and (Test-Path $ffprobeOut) -and -not $Force) {
    Write-Host "ffmpeg sidecars already present for $Triple (use -Force to re-download)."
    exit 0
}

$asset = "ffmpeg-n$Version-latest-win64-gpl-$Version.zip"
$base = 'https://github.com/BtbN/FFmpeg-Builds/releases/download/latest'
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("ffmpeg-fetch-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force $tmp | Out-Null

try {
    Write-Host "Downloading $asset ..."
    $zip = Join-Path $tmp $asset
    Invoke-WebRequest -Uri "$base/$asset" -OutFile $zip -UseBasicParsing
    Invoke-WebRequest -Uri "$base/checksums.sha256" -OutFile (Join-Path $tmp 'checksums.sha256') -UseBasicParsing

    $expected = (Get-Content (Join-Path $tmp 'checksums.sha256') |
        Where-Object { $_ -match [regex]::Escape($asset) + '$' } |
        Select-Object -First 1)
    if (-not $expected) { throw "No checksum listed for $asset" }
    $expected = $expected.Split(' ')[0].Trim().ToLower()
    $actual = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
    if ($expected -ne $actual) { throw "Checksum mismatch for $asset (expected $expected, got $actual)" }
    Write-Host "Checksum OK."

    $extract = Join-Path $tmp 'x'
    New-Item -ItemType Directory -Force $extract | Out-Null
    # tar.exe ships with Windows 10+ and reads zip; Expand-Archive is unreliable on some systems.
    & "$env:SystemRoot\System32\tar.exe" -xf $zip -C $extract
    if ($LASTEXITCODE -ne 0) { throw "Failed to extract $asset" }
    $bin = Get-ChildItem $extract -Recurse -Directory -Filter bin | Select-Object -First 1
    if (-not $bin) { throw 'bin folder not found in archive' }

    New-Item -ItemType Directory -Force $binDir | Out-Null
    Copy-Item (Join-Path $bin.FullName 'ffmpeg.exe') $ffmpegOut -Force
    Copy-Item (Join-Path $bin.FullName 'ffprobe.exe') $ffprobeOut -Force
    Write-Host "Installed:`n  $ffmpegOut`n  $ffprobeOut"
} finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
