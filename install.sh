#!/usr/bin/env bash
# Pantheon installer — one line, done.
#   curl -fsSL https://pantheon.run/install.sh | bash
#
# Installs Rust (if missing), builds Pantheon from source,
# and puts the binary in ~/.local/bin. Works on Linux and macOS.
set -euo pipefail

REPO="${PANTHEON_REPO_URL:-https://github.com/k1ng0mar/pantheon.git}"
BIN_DIR="${PANTHEON_INSTALL_DIR:-$HOME/.local/bin}"
SRC_DIR="${PANTHEON_SRC_DIR:-$HOME/.pantheon-src}"

log() { printf '  >> %s\n' "$*"; }
ok()  { printf '  ok: %s\n' "$*"; }
warn() { printf '  !! %s\n' "$*" >&2; }

# --- 1. check system deps ---
log "checking system deps..."
missing=()
for cmd in sh git curl; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        missing+=("$cmd")
    fi
done

if [ ${#missing[@]} -gt 0 ]; then
    printf '  missing: %s\n' "${missing[*]}"

    # Try to install them
    if command -v apt-get >/dev/null 2>&1; then
        warn "trying: sudo apt-get install -y ${missing[*]}"
        sudo apt-get update -qq && sudo apt-get install -y "${missing[@]}"
    elif command -v brew >/dev/null 2>&1; then
        warn "trying: brew install ${missing[*]}"
        brew install "${missing[@]}"
    elif command -v dnf >/dev/null 2>&1; then
        warn "trying: sudo dnf install -y ${missing[*]}"
        sudo dnf install -y "${missing[@]}"
    elif command -v pacman >/dev/null 2>&1; then
        warn "trying: sudo pacman -S --noconfirm ${missing[*]}"
        sudo pacman -S --noconfirm "${missing[@]}"
    else
        printf '  install %s manually and re-run\n' "${missing[*]}" >&2
        exit 1
    fi
fi
ok "system deps ready"

# --- 2. install Rust if missing ---
if ! command -v rustc >/dev/null 2>&1; then
    log "installing Rust toolchain..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    . "$HOME/.cargo/env"
else
    ok "rustc $(rustc --version)"
fi

# --- 3. clone or update source ---
if [ -d "$SRC_DIR/.git" ]; then
    log "updating source..."
    git -C "$SRC_DIR" pull --ff-only --quiet
else
    log "cloning pantheon..."
    git clone --depth 1 "$REPO" "$SRC_DIR"
fi

# --- 4. build ---
log "building (first run takes a few minutes)..."
cargo build --release --locked --manifest-path "$SRC_DIR/Cargo.toml"
BIN="$SRC_DIR/target/release/pantheon"
[ -x "$BIN" ] || { printf '  build failed\n' >&2; exit 1; }

# --- 5. install ---
mkdir -p "$BIN_DIR"
cp "$BIN" "$BIN_DIR/pantheon"
chmod +x "$BIN_DIR/pantheon"

if [[ ":$PATH:" != *":$BIN_DIR:"* ]]; then
    printf '\nadd to your shell config (~/.bashrc or ~/.zshrc):\n'
    printf '  export PATH="%s:$PATH"\n' "$BIN_DIR"
fi

printf '\n'
ok "pantheon installed"
printf '  pantheon setup    # configure your API key\n'
printf '  pantheon          # start a session\n'
