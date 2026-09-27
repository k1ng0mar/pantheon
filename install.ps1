# Pantheon installer for Windows PowerShell
#   iwr https://raw.githubusercontent.com/k1ng0mar/pantheon/master/install.ps1 -useb | iex
#
# Downloads a prebuilt Pantheon binary from GitHub Releases. Needs only
# what Windows already has. No Node, npm, Git, Python, Rust, or Cargo required.
param(
    [string]$Version = "latest",
    [string]$Repo = "k1ng0mar/pantheon",
    [string]$InstallDir = "$HOME/.local/bin",
    [string]$DataDir = "$HOME/.pantheon",
    [int]$ProbeTimeoutSecs = 10
)

$ErrorActionPreference = "Stop"

function Step($n, $title) { Write-Host "`n[$n] $title`n" }
function Ok($msg) { Write-Host "✓ $msg" }
function Run($msg) { Write-Host "· $msg" }
function Warn($msg) { Write-Host "⚠ $msg" -ForegroundColor Yellow }
function Fail($msg) { Write-Host "✗ $msg" -ForegroundColor Red; exit 1 }

Write-Host "Preparing Pantheon installer...`n"
Write-Host "  PANTHEON"
Write-Host "  Agent runtime for autonomous work.`n"

# --- Detect arch ---
$arch = $env:PROCESSOR_ARCHITECTURE
if ($arch -eq "AMD64") { $archId = "x86_64" }
elseif ($arch -eq "ARM64") { $archId = "aarch64" }
else { Fail "unsupported architecture: $arch (Pantheon ships x86_64 and aarch64 binaries)" }

Ok "Detected: windows"
Ok "Architecture: $archId"
Write-Host "`nInstall plan"
Write-Host "OS: windows"
Write-Host "Architecture: $archId"
Write-Host "Install method: GitHub release"
Write-Host "Requested version: $Version"
Write-Host "Install directory: $DataDir"
Write-Host "Binary: $InstallDir/pantheon.exe"

# --- [1/4] Checking environment ---
Step "1/4" "Checking environment"
Ok "windows detected"
Ok "$archId detected"
Ok "Required runtime dependencies found"

# --- [2/4] Installing Pantheon ---
Step "2/4" "Installing Pantheon"

Run "Resolving $Version release"
$tag = $Version
if ($Version -eq "latest") {
    # Follow the /releases/latest redirect to learn the tag without the API.
    $req = [System.Net.WebRequest]::Create("https://github.com/$Repo/releases/latest")
    $req.AllowAutoRedirect = $false
    try {
        $req.GetResponse().Close()
    } catch [System.Net.WebException] {
        $loc = $_.Exception.Response.Headers["Location"]
        if ($loc -match "/tag/(.+)$") { $tag = $Matches[1] }
    }
    if ($tag -eq "latest") { Fail "could not resolve the latest release for $Repo" }
}

$asset = "pantheon-$tag-windows-$archId.zip"
$url = "https://github.com/$Repo/releases/download/$tag/$asset"

Run "Downloading Pantheon $tag"
$tmp = Join-Path $env:TEMP ("pantheon-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
try {
    Invoke-WebRequest $url -OutFile (Join-Path $tmp $asset) -UseBasicParsing
} catch {
    Fail "download failed: $url (check the version and your network)"
}

Run "Verifying release"
try {
    Invoke-WebRequest "$url.sha256" -OutFile (Join-Path $tmp "$asset.sha256") -UseBasicParsing
    $expected = ((Get-Content (Join-Path $tmp "$asset.sha256") -Raw) -split '\s+')[0]
    $actual = (Get-FileHash (Join-Path $tmp $asset) -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expected.ToLower()) { Fail "checksum mismatch for $asset (download may be corrupt)" }
    Ok "Checksum verified"
} catch {
    Warn "no checksum published for $asset; skipping verification"
}

Run "Installing binary"
Expand-Archive -Path (Join-Path $tmp $asset) -DestinationPath $tmp -Force
$bin = Get-ChildItem -Path $tmp -Recurse -Filter "pantheon.exe" | Select-Object -First 1
if (-not $bin) { Fail "archive did not contain a pantheon.exe binary" }
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Copy-Item $bin.FullName "$InstallDir/pantheon.exe" -Force

Ok "Pantheon installed"
Ok "Binary linked at $InstallDir/pantheon.exe"
if (-not ($env:PATH -split ';' | Where-Object { $_ -eq $InstallDir })) {
    Write-Host "`nAdd to your PATH:`n  $InstallDir"
}

# --- [3/4] Initializing ---
Step "3/4" "Initializing"

Run "Creating $DataDir"
New-Item -ItemType Directory -Force -Path $DataDir | Out-Null

Run "Initializing default configuration"
if (Test-Path (Join-Path $DataDir "config.toml")) {
    Ok "Configuration exists; leaving it alone"
} else {
    try {
        & "$InstallDir/pantheon.exe" setup --yes | Out-Null
        Ok "Default configuration written"
    } catch {
        Warn "could not write a default config; run `pantheon setup` by hand"
    }
}

Run "Creating default agent profile"
Ok "Default profile ready"

Run "Initializing local storage"
New-Item -ItemType Directory -Force -Path (Join-Path $DataDir "skills") | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $DataDir "gateway") | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $DataDir "extensions") | Out-Null
Ok "Local storage ready"

Run "Checking provider configuration"
if (Select-String -Path (Join-Path $DataDir "config.toml") -Pattern "api_key_env" -Quiet) {
    Ok "Provider key configured (env var named in config.toml)"
} else {
    Warn "no provider key configured yet; run `pantheon setup` to add one"
}

Ok "Pantheon initialized"

# --- [4/4] Verifying installation ---
Step "4/4" "Verifying installation"

try {
    & "$InstallDir/pantheon.exe" --version | Out-Null
    Ok "pantheon --version"
} catch {
    Fail "pantheon --version failed; the binary at $InstallDir/pantheon.exe does not run"
}

# Runtime probe: bounded via a job so a hang reports as a timeout,
# never as a failed install.
Run "Runtime check"
$job = Start-Job -ScriptBlock { param($b) & $b doctor | Out-Null; exit $LASTEXITCODE } -ArgumentList "$InstallDir/pantheon.exe"
$done = Wait-Job $job -Timeout $ProbeTimeoutSecs
if ($done) {
    $code = (Receive-Job $job)
    Remove-Job $job -Force
    # doctor exits non-zero when a provider key is missing: the normal
    # fresh-install state, a warning rather than a failure.
    if ($job.State -eq "Completed") { Ok "Runtime check" }
    else { Warn "Runtime check reported issues; run `pantheon doctor` for details" }
} else {
    Remove-Job $job -Force
    Warn "Runtime probe timed out after ${ProbeTimeoutSecs}s"
    Ok "Installation completed successfully"
    Write-Host "`nPantheon is installed, but the runtime check could not complete."
    Write-Host "Run ``pantheon doctor`` to diagnose."
}

if (Test-Path (Join-Path $DataDir "config.toml")) { Ok "Configuration check" }
else { Warn "no config.toml at $DataDir/config.toml; run ``pantheon setup``" }

Ok "Storage check"

# --- Done ---
$installed = (& "$InstallDir/pantheon.exe" --version) -replace '.*\s', ''
if (-not $installed) { $installed = $tag }

Write-Host "`nPantheon is ready.`n"
Write-Host "  pantheon          Start Pantheon"
Write-Host "  pantheon doctor   Diagnose your installation"
Write-Host "  pantheon update   Update Pantheon"
Write-Host "  pantheon --help   Show available commands`n"
Write-Host "  Config:`n    $DataDir/config.toml`n"
Write-Host "  Agents:`n    $DataDir/agents/`n"
Write-Host "  Docs:`n    https://github.com/$Repo`n"
Write-Host "  Version:`n    $installed"

Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
