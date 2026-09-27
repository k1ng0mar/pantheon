#!/usr/bin/env bash
# Pantheon installer — GitHub Releases, no toolchain required.
#   curl -fsSL https://raw.githubusercontent.com/k1ng0mar/pantheon/master/install.sh | bash
#
# Downloads a prebuilt Pantheon binary from GitHub Releases and links it
# into ~/.local/bin. Needs only curl and tar. No Node, npm, Git, Python,
# Rust, or Cargo required.
#
# Env overrides:
#   PANTHEON_VERSION    version to install (default: latest). e.g. v0.1.0
#   PANTHEON_REPO       owner/repo (default: k1ng0mar/pantheon)
#   PANTHEON_INSTALL_DIR  binary dir (default: $HOME/.local/bin)
#   PANTHEON_DATA_DIR     data dir (default: $HOME/.pantheon)
#   PANTHEON_NO_INIT      =1 to skip the init stage
#   PANTHEON_NO_VERIFY    =1 to skip the verify stage
set -euo pipefail

REPO="${PANTHEON_REPO:-k1ng0mar/pantheon}"
REQUESTED_VERSION="${PANTHEON_VERSION:-latest}"
BIN_DIR="${PANTHEON_INSTALL_DIR:-$HOME/.local/bin}"
DATA_DIR="${PANTHEON_DATA_DIR:-$HOME/.pantheon}"
PROBE_TIMEOUT_SECS="${PANTHEON_PROBE_TIMEOUT_SECS:-10}"

TICK="✓"
DOT="·"
WARN="⚠"
FAIL="✗"

step() { printf '\n[%s] %s\n\n' "$1" "$2"; }
ok()   { printf '%s %s\n' "$TICK" "$*"; }
run()  { printf '%s %s\n' "$DOT" "$*"; }
warn() { printf '%s %s\n' "$WARN" "$*" >&2; }
fail() { printf '%s %s\n' "$FAIL" "$*" >&2; exit 1; }

printf 'Preparing Pantheon installer...\n\n'
printf '  PANTHEON\n'
printf '  Agent runtime for autonomous work.\n\n'

# --- Detect OS / arch (no assumptions, explicit failure) ---
OS="$(uname -s 2>/dev/null || echo unknown)"
ARCH="$(uname -m 2>/dev/null || echo unknown)"

case "$OS" in
  Linux)  OS_ID="linux" ;;
  Darwin) OS_ID="darwin" ;;
  *) fail "unsupported OS: $OS (Pantheon ships linux and darwin binaries)" ;;
esac

case "$ARCH" in
  x86_64|amd64) ARCH_ID="x86_64" ;;
  arm64|aarch64) ARCH_ID="aarch64" ;;
  *) fail "unsupported architecture: $ARCH (Pantheon ships x86_64 and aarch64 binaries)" ;;
esac

ok "Detected: $OS_ID"
ok "Architecture: $ARCH_ID"
printf '\nInstall plan\n'
printf 'OS: %s\n' "$OS_ID"
printf 'Architecture: %s\n' "$ARCH_ID"
printf 'Install method: GitHub release\n'
printf 'Requested version: %s\n' "$REQUESTED_VERSION"
printf 'Install directory: %s\n' "$DATA_DIR"
printf 'Binary: %s/pantheon\n' "$BIN_DIR"

# --- [1/4] Checking environment ---
step "1/4" "Checking environment"

for cmd in curl tar; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    fail "missing required tool: $cmd (install it and re-run)"
  fi
done

ok "$OS_ID detected"
ok "$ARCH_ID detected"
ok "curl found"
ok "tar found"
ok "Required runtime dependencies found"

# --- [2/4] Installing Pantheon ---
step "2/4" "Installing Pantheon"

run "Resolving $REQUESTED_VERSION release"
if [ "$REQUESTED_VERSION" = "latest" ]; then
  # GitHub redirect gives the latest tag without needing the API or jq.
  VERSION="$(curl -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" | sed 's#.*/tag/##')"
  [ -n "$VERSION" ] || fail "could not resolve the latest release for $REPO"
else
  VERSION="$REQUESTED_VERSION"
fi

ASSET="pantheon-${VERSION}-${OS_ID}-${ARCH_ID}.tar.gz"
URL="https://github.com/$REPO/releases/download/${VERSION}/${ASSET}"

run "Downloading Pantheon $VERSION"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT
if ! curl -fsSL "$URL" -o "$TMPDIR/$ASSET"; then
  fail "download failed: $URL (check the version and your network)"
fi

run "Verifying release"
# Checksum file is published alongside the asset; absence is a warning,
# not a silent skip.
if curl -fsSL "${URL}.sha256" -o "$TMPDIR/$ASSET.sha256" 2>/dev/null; then
  (cd "$TMPDIR" && sha256sum -c "$ASSET.sha256" >/dev/null 2>&1) \
    || fail "checksum mismatch for $ASSET (download may be corrupt)"
  ok "Checksum verified"
else
  warn "no checksum published for $ASSET; skipping verification"
fi

run "Installing binary"
tar -xzf "$TMPDIR/$ASSET" -C "$TMPDIR"
# Release tarballs contain a single `pantheon` binary.
BIN_SRC="$(find "$TMPDIR" -maxdepth 2 -name pantheon -type f | head -n 1)"
[ -n "${BIN_SRC:-}" ] || fail "archive did not contain a pantheon binary"
mkdir -p "$BIN_DIR"
cp "$BIN_SRC" "$BIN_DIR/pantheon"
chmod +x "$BIN_DIR/pantheon"

ok "Pantheon installed"
ok "Binary linked at $BIN_DIR/pantheon"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) printf '\nadd to your shell config (~/.bashrc or ~/.zshrc):\n  export PATH="%s:$PATH"\n' "$BIN_DIR" ;;
esac

# --- [3/4] Initializing ---
step "3/4" "Initializing"

if [ "${PANTHEON_NO_INIT:-0}" = "1" ]; then
  run "Init skipped (PANTHEON_NO_INIT=1)"
else
  run "Creating $DATA_DIR"
  mkdir -p "$DATA_DIR"

  run "Initializing default configuration"
  if [ -f "$DATA_DIR/config.toml" ]; then
    ok "Configuration exists; leaving it alone"
  elif [ -x "$BIN_DIR/pantheon" ]; then
    # Non-interactive defaults only. Never prompts, never overwrites,
    # never writes a key: provider keys are configured with `pantheon setup`.
    if "$BIN_DIR/pantheon" setup --yes >/dev/null 2>&1; then
      ok "Default configuration written"
    else
      warn "could not write a default config; run \`pantheon setup\` by hand"
    fi
  fi

  run "Creating default agent profile"
  # The default profile is the [model]/policy written above; per-agent
  # [agents.*] tables are opt-in via config, so there is nothing to seed.
  ok "Default profile ready"

  run "Initializing local storage"
  # ledger.db / memory.db are created lazily on first open; touch the
  # directories the runtime writes into so a read-only parent fails here,
  # not on first run.
  mkdir -p "$DATA_DIR/skills" "$DATA_DIR/gateway" "$DATA_DIR/extensions"
  ok "Local storage ready"

  run "Checking provider configuration"
  if grep -q 'api_key_env' "$DATA_DIR/config.toml" 2>/dev/null; then
    ok "Provider key configured (env var named in config.toml)"
  else
    warn "no provider key configured yet; run \`pantheon setup\` to add one"
  fi

  ok "Pantheon initialized"
fi

# --- [4/4] Verifying installation ---
step "4/4" "Verifying installation"

if [ "${PANTHEON_NO_VERIFY:-0}" = "1" ]; then
  run "Verify skipped (PANTHEON_NO_VERIFY=1)"
else
  if "$BIN_DIR/pantheon" --version >/dev/null 2>&1; then
    ok "pantheon --version"
  else
    fail "pantheon --version failed; the binary at $BIN_DIR/pantheon does not run"
  fi

  # Runtime probe: bounded, and a timeout is reported as exactly that —
  # never as a failed install.
  if command -v timeout >/dev/null 2>&1; then
    if timeout "$PROBE_TIMEOUT_SECS" "$BIN_DIR/pantheon" doctor >/dev/null 2>&1; then
      ok "Runtime check"
    else
      code=$?
      if [ $code -eq 124 ]; then
        warn "Runtime probe timed out after ${PROBE_TIMEOUT_SECS}s"
        ok "Installation completed successfully"
        printf '\nPantheon is installed, but the runtime check could not complete.\n'
        printf 'Run `pantheon doctor` to diagnose.\n'
      else
        # doctor exits non-zero when a provider key is missing, which is
        # the normal fresh-install state — a warning, not a failure.
        warn "Runtime check reported issues; run \`pantheon doctor\` for details"
      fi
    fi
  else
    if "$BIN_DIR/pantheon" doctor >/dev/null 2>&1; then
      ok "Runtime check"
    else
      warn "Runtime check reported issues; run \`pantheon doctor\` for details"
    fi
  fi

  if [ -f "$DATA_DIR/config.toml" ]; then
    ok "Configuration check"
  else
    warn "no config.toml at $DATA_DIR/config.toml; run \`pantheon setup\`"
  fi

  # Storage check: the databases open (created lazily, so absence is fine
  # as long as the directory is writable).
  if [ -w "$DATA_DIR" ]; then
    ok "Storage check"
  else
    warn "Storage check: $DATA_DIR is not writable"
  fi
fi

# --- Done ---
INSTALLED_VERSION="$("$BIN_DIR/pantheon" --version 2>/dev/null | awk '{print $NF}')"
[ -n "${INSTALLED_VERSION:-}" ] || INSTALLED_VERSION="$VERSION"

printf '\nPantheon is ready.\n\n'
printf '  pantheon          Start Pantheon\n'
printf '  pantheon doctor   Diagnose your installation\n'
printf '  pantheon update   Update Pantheon\n'
printf '  pantheon --help   Show available commands\n\n'
printf '  Config:\n    %s/config.toml\n\n' "$DATA_DIR"
printf '  Agents:\n    %s/agents/\n\n' "$DATA_DIR"
printf '  Docs:\n    https://github.com/%s\n\n' "$REPO"
printf '  Version:\n    %s\n' "$INSTALLED_VERSION"
