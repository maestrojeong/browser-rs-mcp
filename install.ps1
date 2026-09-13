# Install the browser-rs prebuilt binary from GitHub Releases (Windows).
#   irm https://raw.githubusercontent.com/maestrojeong/browser-rs-mcp/main/install.ps1 | iex
# Env: AB_VERSION (default: latest), AB_BIN_DIR (default: %LOCALAPPDATA%\browser-rs\bin)
$ErrorActionPreference = "Stop"

$Repo = "maestrojeong/browser-rs-mcp"
$Version = if ($env:AB_VERSION) { $env:AB_VERSION } else { "latest" }

$Arch = if ([System.Environment]::Is64BitOperatingSystem) { "x64" } else { $null }
if ($Arch -ne "x64") {
    Write-Error "Unsupported architecture. Build from source instead:`n  cargo install --git https://github.com/$Repo ab-mcp"
    exit 1
}
$Asset = "browser-rs-windows-x64.exe"

if ($Version -eq "latest") {
    $Url = "https://github.com/$Repo/releases/latest/download/$Asset"
} else {
    $Url = "https://github.com/$Repo/releases/download/$Version/$Asset"
}

$Dest = if ($env:AB_BIN_DIR) { $env:AB_BIN_DIR } else { Join-Path $env:LOCALAPPDATA "browser-rs\bin" }
New-Item -ItemType Directory -Force -Path $Dest | Out-Null

$Target = Join-Path $Dest "browser-rs.exe"
Write-Host "Downloading $Asset ($Version) -> $Target"
Invoke-WebRequest -Uri $Url -OutFile $Target -UseBasicParsing

Write-Host "Installed: $Target"

$PathEntries = $env:Path -split ";"
if ($PathEntries -contains $Dest) {
    Write-Host "Run: browser-rs --help"
} else {
    Write-Host "Add to PATH (current user):"
    Write-Host "  setx PATH `"$Dest;`$env:Path`""
    Write-Host "Then open a new terminal and run: browser-rs --help"
}
