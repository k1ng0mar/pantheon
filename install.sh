#!/usr/bin/env bash
# Pantheon Agent Runtime Installer
# ============================================================================
# Installs Rust (if missing), builds Pantheon from source, and links the
# binary into ~/.local/bin. Idempotent: re-running skips steps already done.
#
# Usage:
#   curl -fsSL https://pantheon.run/install.sh | bash
#
# Options:
#   --repo URL           Clone from this repo instead of the default
#   --install-dir PATH   Install binary into PATH instead of ~/.local/bin
#   --no-build-cache     Force a clean build (ignores cached cargo artifacts)
# ============================================================================

set -euo pipefail

# --- configuration ---
REPO_URL="https://github.com/pantheon-agent/pantheon.git"
INSTALL_DIR="${PANTHEON_INSTALL_DIR:-$HOME/.local/bin}"
SRC_DIR="${PANTHEON_SRC_DIR:-$HOME/.pantheon-src}"
FORCE_REPO=""

# --- helpers ---
log()   { printf '  \033[0;36m>>\033[0m %s\n' "$*"; }
warn()  { printf '  \033[1;33m!!\033[0m %s\n' "$*" >&2; }
err()   { printf '  \033[0;31mxx\033[0m %s\n' "$*" >&2; }
ok()    { printf '  \033[0;32mok\033[0m %s\n' "$*"; }

# --- argument parsing ---
while [[ $# -gt 0 ]]; do
    case $1 in
        --repo)        REPO_URL="$2"; shift 2 ;;
        --install-dir) INSTALL_DIR="$2"; shift 2 ;;
        --no-build-cache) CLEAN_BUILD=true; shift ;;
        --help)
            sed -n '3,14p' "$0"
            exit 0
            ;;
        *) err "unknown option: $1"; exit 1 ;;
    esac
done

# --- detect OS ---
OS="unknown"
ARCH="$(uname -m)"
if [[ "$OSTYPE" == "linux-gnu"* ]]; then
    OS="linux"
elif [[ "$OSTYPE" == "darwin"* ]]; then
    OS="macos"
fi

if [[ "$OS" == "unknown" ]]; then
    err "unsupported OS: $OSTYPE"
    exit 1
fi

log "detected: $OS ($ARCH)"

# --- check system dependencies ---
check_system_deps() {
    local missing=()
    for cmd in sh git; do
        if ! command -v "$cmd" >/dev/null 2>&1; then
            missing+=("$cmd")
        fi
    done

    if [ ${#missing[@]} -gt 0 ]; then
        err "missing system dependencies: ${missing[*]}"
        case "$OS" in
            linux)
                if command -v apt-get >/dev/null 2>&1; then
                    err "install with: sudo apt-get install -y ${missing[*]}"
                elif command -v dnf >/dev/null 2>&1; then
                    err "install with: sudo dnf install -y ${missing[*]}"
                elif command -v pacman >/dev/null 2>&1; then
                    err "install with: sudo pacman -S --noconfirm ${missing[*]}"
                fi
                ;;
            macos)
                err "install with: brew install ${missing[*]}"
                ;;
        esac
        exit 1
    fi
    ok "system deps: sh, git"
}

# --- ensure Rust toolchain ---
ensure_rust() {
    if command -v rustc >/dev/null 2>&1; then
        ok "rustc $(rustc --version)"
        return
    fi

    warn "Rust not found — installing rustup"
    if ! command -v curl >/dev/null 2>&1; then
        err "curl is required to install Rust"
        exit 1
    fi

    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
    # shellcheck disable=SC1091
    if [ -f "$HOME/.cargo/env" ]; then
        . "$HOME/.cargo/env"
    fi
    ok "rustc $(rustc --version)"
}

# --- clone or update source ---
ensure_source() {
    if [ -d "$SRC_DIR/.git" ]; then
        log "updating source ($SRC_DIR)"
        git -C "$SRC_DIR" pull --ff-only --quiet
    else
        log "cloning pantheon"
        git clone --depth 1 "$REPO_URL" "$SRC_DIR"
    fi
}

# --- build ---
build() {
    if [ "${CLEAN_BUILD:-false}" = "true" ]; then
        log "cargo clean (ignoring build cache)"
        cargo clean --manifest-path "$SRC_DIR/Cargo.toml" 2>/dev/null || true
    fi

    log "building (may take several minutes on first run)"
    cargo build --release --locked --manifest-path "$SRC_DIR/Cargo.toml"

    local bin="$SRC_DIR/target/release/pantheon"
    if [ ! -x "$bin" ]; then
        err "build succeeded but binary not found at $bin"
        exit 1
    fi
    printf '%s' "$bin"
}

# --- install binary ---
install_binary() {
    local bin="$1"
    mkdir -p "$INSTALL_DIR"
    cp "$bin" "$INSTALL_DIR/pantheon"
    chmod +x "$INSTALL_DIR/pantheon"

    case ":$PATH:" in
        *":$INSTALL_DIR:"*)
            ok "installed: $INSTALL_DIR/pantheon"
            ;;
        *)
            warn "$INSTALL_DIR is not on your PATH"
            printf '  add it with:\n'
            printf '    export PATH="%s:$PATH"\n' "$INSTALL_DIR"
            ;;
    esac
}

# --- main ---
main() {
    printf '\n'
    log "pantheon installer"
    printf '\n'

    check_system_deps
    ensure_rust

    if [ -f "$HOME/.cargo/env" ]; then
        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
    fi

    ensure_source

    local bin
    bin=$(build)

    install_binary "$bin"

    # Create the default data directory so first-run doesn't need setup
    local pantheon_home="${PANTHEON_DATA_DIR:-$HOME/.pantheon}"
    mkdir -p "$pantheon_home"

    printf '\n'
    ok "pantheon is ready"
    printf '\nnext steps:\n'
    printf '  pantheon setup --yes --provider openai --model gpt-4o-mini --api-key-env OPENAI_API_KEY\n'
    printf '  export OPENAI_API_KEY=sk-...\n'
    printf '  pantheon              # interactive session\n'
    printf '  pantheon doctor       # verify everything works\n'
    printf '\n'
}

main "$@"
