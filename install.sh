#!/usr/bin/env bash
# Pantheon installer - GitHub Releases, no toolchain required.
#   curl -fsSL https://raw.githubusercontent.com/k1ng0mar/pantheon/master/install.sh | bash
#
# Downloads a prebuilt Pantheon binary from GitHub Releases and links it
# into ~/.local/bin. Needs only curl and tar. No Node, npm, Git, Python,
# Rust, or Cargo required.
#
# Env overrides:
#   PANTHEON_VERSION    version to install (default: latest). e.g. v1.0.0
#   PANTHEON_REPO       owner/repo (default: k1ng0mar/pantheon)
#   PANTHEON_INSTALL_DIR  binary dir (default: $HOME/.local/bin)
#   PANTHEON_DATA_DIR     data dir (default: $HOME/.pantheon)
#   PANTHEON_NO_INIT      =1 to skip the init stage
#   PANTHEON_NO_VERIFY    =1 to skip the verify stage
#   PANTHEON_NO_PYTHON_DEPS =1 to skip optional Python plugin dependencies
#   PANTHEON_ALLOW_UNVERIFIED =1 to install without a checksum (fail-closed otherwise)
#   PANTHEON_MINISIGN_PUBKEY   base64 minisign public key (`RW...`) to
#                              authenticate the archive; verified when set
#   PANTHEON_MINISIGN_PUBKEY_FILE  file containing that key (alternative)
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
printf 'Install directory: %s\n' "$BIN_DIR"
printf 'Data directory: %s\n' "$DATA_DIR"
printf 'Binary: %s/pantheon\n' "$BIN_DIR"

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

# I-13: re-running the installer for the version already on disk is a
# no-op - skip the download instead of fetching it again.
if [ -x "$BIN_DIR/pantheon" ]; then
  INSTALLED_BEFORE="$("$BIN_DIR/pantheon" --version 2>/dev/null | awk '{print $NF}')"
  norm_ver() { printf '%s' "$1" | sed 's/^v//'; }
  if [ -n "${INSTALLED_BEFORE:-}" ] \
     && [ "$(norm_ver "$INSTALLED_BEFORE")" = "$(norm_ver "$VERSION")" ]; then
    ok "Pantheon $(norm_ver "$VERSION") is already installed at $BIN_DIR/pantheon; nothing to do"
    exit 0
  fi
  [ -n "${INSTALLED_BEFORE:-}" ] && run "Installed $INSTALLED_BEFORE differs from $VERSION; upgrading"
fi

run "Downloading Pantheon $VERSION"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT
if ! curl -fsSL "$URL" -o "$TMPDIR/$ASSET"; then
  fail "download failed: $URL (check the version and your network)"
fi

run "Verifying release"
# Pick the checksum verifier per platform: Linux ships GNU sha256sum,
# macOS ships BSD shasum (I-7: sha256sum does not exist on macOS, and a
# missing tool used to misreport as "checksum mismatch").
# Returns: 0 = verified, 1 = mismatch, 2 = no usable tool.
verify_checksum() {
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$TMPDIR" && sha256sum -c "$ASSET.sha256" >/dev/null 2>&1)
    return $?
  elif command -v shasum >/dev/null 2>&1; then
    (cd "$TMPDIR" && shasum -a 256 -c "$ASSET.sha256" >/dev/null 2>&1)
    return $?
  else
    return 2
  fi
}
# Fail-closed, like `pantheon update` (I-11): a missing checksum file or a
# mismatch aborts the install. Warn-and-continue needs the explicit
# PANTHEON_ALLOW_UNVERIFIED=1 opt-out.
if curl -fsSL "${URL}.sha256" -o "$TMPDIR/$ASSET.sha256" 2>/dev/null; then
  if verify_checksum; then
    ok "Checksum verified"
    # Say what the check actually proves. The .sha256 file comes from the
    # same origin as the archive, so it detects corruption, never
    # authorship. Release authentication needs the pinned minisign key,
    # which is not provisioned yet - `pantheon update` refuses unsigned
    # releases for that reason.
    run "Integrity only: the checksum is published alongside the archive; it is not a signature."
    # Authenticate when the caller pinned a key. The checksum above comes from
    # the same origin as the archive, so it proves corruption only; the
    # signature is the one control that proves authorship (audit F7). The
    # installer used to ignore the .minisig that the release workflow
    # publishes and tell the user to "install by hand" instead.
    PUBKEY="${PANTHEON_MINISIGN_PUBKEY:-}"
    if [ -z "$PUBKEY" ] && [ -n "${PANTHEON_MINISIGN_PUBKEY_FILE:-}" ]; then
      if [ -r "${PANTHEON_MINISIGN_PUBKEY_FILE}" ]; then
        # The key file is `untrusted comment: ...` then the base64 key line.
        PUBKEY="$(grep -E '^RW' "${PANTHEON_MINISIGN_PUBKEY_FILE}" | tail -n 1 || true)"
      else
        fail "PANTHEON_MINISIGN_PUBKEY_FILE is not readable: ${PANTHEON_MINISIGN_PUBKEY_FILE}"
      fi
    fi
    if [ -n "$PUBKEY" ]; then
      command -v minisign >/dev/null 2>&1 \
        || fail "PANTHEON_MINISIGN_PUBKEY is set but `minisign` is not installed (apt/brew install minisign)"
      if ! curl -fsSL "${URL}.minisig" -o "$TMPDIR/$ASSET.minisig" 2>/dev/null; then
        if [ "${PANTHEON_ALLOW_UNVERIFIED:-0}" = "1" ]; then
          warn "no .minisig published for $ASSET; continuing unauthenticated (PANTHEON_ALLOW_UNVERIFIED=1)"
        else
          fail "no signature published for $ASSET - refusing to install unauthenticated (PANTHEON_ALLOW_UNVERIFIED=1 to override)"
        fi
      else
        # minisign's `-p` file is a two-line file: an `untrusted comment:` line
        # then the base64 key. Accept either the whole file or just the `RW...`
        # line in the env var, and always write the two-line form.
        PUBKEY_LINE="$(printf '%s\n' "$PUBKEY" | grep -E '^RW' | tail -n 1 || true)"
        [ -n "$PUBKEY_LINE" ] \
          || fail "PANTHEON_MINISIGN_PUBKEY holds no 'RW...' key line"
        printf 'untrusted comment: pantheon release key\n%s\n' "$PUBKEY_LINE" > "$TMPDIR/minisign.pub"
        if (cd "$TMPDIR" && minisign -V -p minisign.pub -m "$ASSET" >/dev/null 2>&1); then
          ok "Signature verified against the pinned key"
        else
          fail "signature verification FAILED for $ASSET - the archive is not the one signed by the pinned key (do not install)"
        fi
      fi
    else
      warn "Release signing is not provisioned yet, so \`pantheon update\` refuses releases until the key is pinned. Install updates by hand, or build from source (see docs/release-signing.md). Set PANTHEON_MINISIGN_PUBKEY to authenticate this install against a key you trust."
    fi
  else
    code=$?
    if [ "$code" -eq 2 ]; then
      if [ "${PANTHEON_ALLOW_UNVERIFIED:-0}" = "1" ]; then
        warn "no sha256 tool found (sha256sum/shasum); skipping verification (PANTHEON_ALLOW_UNVERIFIED=1)"
      else
        fail "no sha256 tool found (install coreutils on Linux; macOS ships shasum) - refusing to install unverified (PANTHEON_ALLOW_UNVERIFIED=1 to override)"
      fi
    else
      fail "checksum mismatch for $ASSET (download may be corrupt)"
    fi
  fi
else
  if [ "${PANTHEON_ALLOW_UNVERIFIED:-0}" = "1" ]; then
    warn "no checksum published for $ASSET; installing unverified (PANTHEON_ALLOW_UNVERIFIED=1)"
  else
    fail "no checksum published for $ASSET - refusing to install unverified (PANTHEON_ALLOW_UNVERIFIED=1 to override)"
  fi
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

# Adds BIN_DIR to PATH by default (PANTHEON_NO_PATH=1 to skip). Detects the
# user's login shell and appends to the right rc file: bash -> ~/.bashrc,
# zsh -> ~/.zshrc, fish -> ~/.config/fish/config.fish. Works on Linux and
# macOS; the installer only ships binaries for those two anyway.
add_to_path() {
  case ":$PATH:" in
    *":$BIN_DIR:"*)
      ok "$BIN_DIR is already on PATH"
      return 0 ;;
  esac

  if [ "${PANTHEON_NO_PATH:-0}" = "1" ]; then
    warn "PANTHEON_NO_PATH=1; skipping PATH setup"
    printf 'add to your shell config manually:\n  export PATH="%s:$PATH"\n' "$BIN_DIR"
    return 0
  fi

  local shell_name rc_file marker path_line
  if [ -z "${SHELL:-}" ]; then
    warn "SHELL is not set; assuming bash for PATH setup - check the rc file below is the one your shell reads"
  fi
  shell_name="$(basename "${SHELL:-/bin/bash}")"
  case "$shell_name" in
    bash) rc_file="$HOME/.bashrc" ;;
    zsh)  rc_file="$HOME/.zshrc" ;;
    fish) rc_file="$HOME/.config/fish/config.fish" ;;
    *)
      warn "unrecognized shell ($shell_name); could not update PATH automatically"
      printf 'add to your shell config manually:\n  export PATH="%s:$PATH"\n' "$BIN_DIR"
      return 0 ;;
  esac

  marker="# added by the pantheon installer"
  if [ "$shell_name" = "fish" ]; then
    path_line="fish_add_path \"$BIN_DIR\""
  else
    path_line="export PATH=\"$BIN_DIR:\$PATH\""
  fi

  # Already handled only if the actual export line is there - a stale
  # marker with the line deleted must not read as "already present" (I-14).
  if [ -f "$rc_file" ] && grep -qF "$path_line" "$rc_file" 2>/dev/null; then
    ok "PATH entry already present in $rc_file"
    return 0
  fi
  if [ -f "$rc_file" ] && grep -qF "$BIN_DIR" "$rc_file" 2>/dev/null; then
    ok "$BIN_DIR already referenced in $rc_file"
    return 0
  fi

  mkdir -p "$(dirname "$rc_file")"
  printf '\n%s\n%s\n' "$marker" "$path_line" >> "$rc_file"
  ok "added $BIN_DIR to PATH in $rc_file"
  printf 'restart your shell, or run: source %s\n' "$rc_file"
}

add_to_path

step "3/4" "Initializing"

if [ "${PANTHEON_NO_INIT:-0}" = "1" ]; then
  run "Init skipped (PANTHEON_NO_INIT=1)"
else
  run "Creating $DATA_DIR"
  mkdir -p "$DATA_DIR"

  run "Deferring model setup to the user"
  # Pantheon does not pick a provider or model on the user's behalf. No
  # config.toml is written here: the first launch opens the setup wizard,
  # where the user chooses from the built-in providers or a custom endpoint.
  # Documentation and automation can still write a config themselves.
  ok "Run \`pantheon setup\` to choose a provider and model"

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

  run "Checking first-run setup"
  if [ -f "$DATA_DIR/config.toml" ]; then
    ok "Config exists; your provider choice is preserved"
  else
    ok "No config yet; \`pantheon\` will open setup on first launch"
  fi

  ok "Pantheon initialized"
fi

step "4/4" "Verifying installation"

if [ "${PANTHEON_NO_VERIFY:-0}" = "1" ]; then
  run "Verify skipped (PANTHEON_NO_VERIFY=1)"
else
  if "$BIN_DIR/pantheon" --version >/dev/null 2>&1; then
    ok "pantheon --version"
  else
    fail "pantheon --version failed; the binary at $BIN_DIR/pantheon does not run"
  fi

  # Runtime probe: bounded, and a timeout is reported as exactly that
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
        # the normal fresh-install state - a warning, not a failure.
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

# The doc-pack plugin (Word/Excel/PowerPoint) needs python-docx, openpyxl
# and python-pptx. Best-effort only: this never fails the install.
if [ "${PANTHEON_NO_PYTHON_DEPS:-0}" = "1" ]; then
  run "Python plugin dependencies skipped (PANTHEON_NO_PYTHON_DEPS=1)"
elif ! command -v python3 >/dev/null 2>&1; then
  warn "python3 not found; skipping Python plugin dependencies"
  warn "The doc-pack plugin needs: pip install python-docx openpyxl python-pptx"
else
  if python3 -c "import docx, openpyxl, pptx" >/dev/null 2>&1; then
    ok "Python plugin dependencies already installed"
  else
    PIP=(python3 -m pip)
    if ! python3 -m pip --version >/dev/null 2>&1; then
      if command -v pip3 >/dev/null 2>&1; then
        PIP=(pip3)
      else
        PIP=()
      fi
    fi
    if [ "${#PIP[@]}" -eq 0 ]; then
      warn "pip not found; skipping Python plugin dependencies"
      warn "Install pip, then run: python3 -m pip install --user python-docx openpyxl python-pptx"
    else
      run "Installing Python plugin dependencies (python-docx openpyxl python-pptx)"
      PIP_OUT="$("${PIP[@]}" install --user python-docx openpyxl python-pptx 2>&1)" || true
      if echo "$PIP_OUT" | grep -q "externally-managed-environment"; then
        # Debian/Ubuntu-style PEP 668: --user alone is refused. Retrying with
        # --break-system-packages is safe here because --user still confines
        # everything to the user's own site-packages.
        warn "system Python is externally managed; retrying user-local install"
        PIP_OUT="$("${PIP[@]}" install --user --break-system-packages --quiet python-docx openpyxl python-pptx 2>&1)" || true
      fi
      if python3 -c "import docx, openpyxl, pptx" >/dev/null 2>&1; then
        ok "Python plugin dependencies installed"
      else
        warn "Could not install Python packages automatically"
        [ -n "${PIP_OUT:-}" ] && printf '%s\n' "$PIP_OUT" | tail -3 >&2
        warn "Run: python3 -m pip install --user python-docx openpyxl python-pptx"
      fi
    fi
  fi
fi

# Respect PANTHEON_NO_VERIFY=1 here too: the verify stage already ran the
# binary when it wasn't skipped, and re-running it after an explicit skip
# defeats the flag (I-14).
if [ "${PANTHEON_NO_VERIFY:-0}" = "1" ]; then
  INSTALLED_VERSION="$VERSION"
else
  INSTALLED_VERSION="$("$BIN_DIR/pantheon" --version 2>/dev/null | awk '{print $NF}')"
  [ -n "${INSTALLED_VERSION:-}" ] || INSTALLED_VERSION="$VERSION"
fi

printf '\nPantheon is ready.\n\n'
printf '  pantheon          Start Pantheon\n'
printf '  pantheon doctor   Diagnose your installation\n'
printf '  pantheon update   Update Pantheon\n'
printf '  pantheon --help   Show available commands\n\n'
printf '  Config:\n    %s/config.toml\n\n' "$DATA_DIR"
printf '  Agents:\n    %s/agents/\n\n' "$DATA_DIR"
printf '  Docs:\n    https://github.com/%s\n\n' "$REPO"
printf '  Version:\n    %s\n' "$INSTALLED_VERSION"
