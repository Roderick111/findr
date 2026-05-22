# Findr installer for Windows — downloads pre-built binary from GitHub Releases
$ErrorActionPreference = "Stop"

$Repo = "Roderick111/findr"
$InstallDir = "$env:LOCALAPPDATA\findr"
$Asset = "findr-windows-x86_64.exe"

Write-Host "Installing findr for Windows..."

# Get latest release URL
$Release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest"
$DownloadUrl = ($Release.assets | Where-Object { $_.name -eq $Asset }).browser_download_url

if (-not $DownloadUrl) {
    Write-Host "No pre-built binary found. Falling back to cargo install..."
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Write-Host "Rust not installed. Visit https://rustup.rs to install."
        exit 1
    }
    cargo install --git "https://github.com/$Repo.git"
    Write-Host "Installed via cargo."
    exit 0
}

# Download and install
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Write-Host "Downloading from $DownloadUrl..."
Invoke-WebRequest -Uri $DownloadUrl -OutFile "$InstallDir\findr.exe"

# Add to user PATH if not already present
$UserPath = [Environment]::GetEnvironmentVariable("PATH", "User")
if ($UserPath -notlike "*$InstallDir*") {
    [Environment]::SetEnvironmentVariable("PATH", "$UserPath;$InstallDir", "User")
    $env:PATH = "$env:PATH;$InstallDir"
    Write-Host "Added $InstallDir to user PATH (restart terminal to apply)."
}

Write-Host ""
Write-Host "Installed findr to $InstallDir\findr.exe"
Write-Host "Run: findr search `"test`" (first run auto-builds the index)"
