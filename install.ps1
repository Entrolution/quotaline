# install.ps1 — download the latest quotaline release binary and wire it into Claude Code.
#
#   irm https://raw.githubusercontent.com/Entrolution/quotaline/main/install.ps1 | iex
#
# Override the install dir with $env:QUOTALINE_BIN_DIR (default: %LOCALAPPDATA%\quotaline).

$ErrorActionPreference = "Stop"

$repo = "Entrolution/quotaline"
$dir = if ($env:QUOTALINE_BIN_DIR) { $env:QUOTALINE_BIN_DIR } else { Join-Path $env:LOCALAPPDATA "quotaline" }
$target = "x86_64-pc-windows-msvc"
$asset = "quotaline-$target.zip"
$url = "https://github.com/$repo/releases/latest/download/$asset"

New-Item -ItemType Directory -Force -Path $dir | Out-Null
$zip = Join-Path $env:TEMP "quotaline-$target.zip"
Write-Host "downloading $url"
Invoke-WebRequest -Uri $url -OutFile $zip
Expand-Archive -Path $zip -DestinationPath $dir -Force
Remove-Item $zip -ErrorAction SilentlyContinue

$exe = Join-Path $dir "quotaline.exe"
Write-Host "installed -> $exe"
Write-Host ""

# Wire it into %USERPROFILE%\.claude\settings.json (backs up first, idempotent).
& $exe install
