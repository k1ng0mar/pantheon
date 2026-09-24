# Pantheon installer for Windows PowerShell
#   iwr https://raw.githubusercontent.com/pantheon-agent/pantheon/main/install.ps1 -useb | iex
#
# Installs Rust (if missing), builds Pantheon from source, and puts the
# binary in $HOME/.local/bin (or $env:USERPROFILE/.local/bin).
param(
    [string]$RepoUrl = "https://github.com/pantheon-agent/pantheon.git",
    [string]$InstallDir = "$HOME/.local/bin",
    [string]$SrcDir = "$HOME/.pantheon-src"
)

function Log($msg) { Write-Host "  >> $msg" }
function Ok($msg) { Write-Host "  ok: $msg" }
function Warn($msg) { Write-Host "  !! $msg" -ForegroundColor Yellow }

# --- 1. check system deps ---
Log "checking system deps..."
$missing = @()
foreach ($cmd in @("git", "curl")) {
    if (-not (Get-Command $cmd -ErrorAction SilentlyContinue)) {
        $missing += $cmd
    }
}
if ($missing.Count -gt 0) {
    Write-Host "  missing: $($missing -join ', ')"
    Write-Host "  install Git and curl manually, then re-run.`nAlternatively: winget install Git.Git"
    exit 1
}
Ok "system deps ready"

# --- 2. install Rust if missing ---
if (-not (Get-Command rustc -ErrorAction SilentlyContinue)) {
    Log "installing Rust toolchain..."
    $url = "https://sh.rustup.rs"
    $output = Join-Path $env:TEMP "rustup-init.exe"
    iwr $url -OutFile $output -UseBasicParsing
    & $output -y
    $env:PATH += ";$HOME/.cargo/bin"
} else {
    Ok "rustc $(rustc --version)"
}

# Ensure cargo is on PATH for this session
$env:PATH += ";$HOME/.cargo/bin"

# --- 3. clone or update source ---
if (Test-Path "$SrcDir/.git") {
    Log "updating source..."
    git -C $SrcDir pull --ff-only --quiet
} else {
    Log "cloning pantheon..."
    git clone --depth 1 $RepoUrl $SrcDir
}

# --- 4. build ---
Log "building (first run takes a few minutes)..."
cargo build --release --locked --manifest-path "$SrcDir/Cargo.toml"
$bin = "$SrcDir/target/release/pantheon.exe"
if (-not (Test-Path $bin)) {
    Write-Host "  build failed" -ForegroundColor Red
    exit 1
}

# --- 5. install ---
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Copy-Item $bin "$InstallDir/pantheon.exe" -Force

# Warn if not on PATH
if (-not ($env:PATH -split ';' | Where-Object { $_ -eq $InstallDir })) {
    Write-Host "`nAdd to your PATH:`n  `$InstallDir"
}

Write-Host ""
Ok "pantheon installed"
Write-Host "  pantheon setup    # configure your API key"
Write-Host "  pantheon          # start a session"
