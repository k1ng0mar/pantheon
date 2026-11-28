# syntax=docker/dockerfile:1.7
#
# Pantheon in a container: one listener (the dashboard + AG-UI surface), one
# data volume, no build toolchain at runtime.
#
#   docker build -t pantheon .
#   docker run --rm -p 127.0.0.1:18789:18789 \
#     -e PANTHEON_SERVE_TOKEN=... -v pantheon-data:/data pantheon
#
# The data dir is a volume because it holds the ledger (every transcript), the
# memory store, secrets and generated images; a container without it starts
# empty every time and loses all of that.

# ---- build -------------------------------------------------------------------
FROM rust:1-bookworm AS build

# rquickjs (Code Mode) shells out to clang for its bindgen step; the rest is
# plain C. No OpenSSL: the HTTP client is rustls, which keeps the runtime stage
# free of libssl.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pkg-config clang libclang-dev cmake perl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
# Copy the manifests first so a source-only change reuses the dependency layer.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY eval ./eval
COPY contrib ./contrib

# Only the binary the runtime stage needs. `--locked` keeps the build
# reproducible against the committed lockfile.
# The binary lives in the `pantheon-tui` crate (bin name `pantheon`); the root
# `pantheon` package is the library aggregator.
RUN cargo build --release --locked -p pantheon-tui --bin pantheon \
    && strip target/release/pantheon

# ---- runtime -----------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    # bubblewrap is the sandbox the shell tool runs commands under; without it
    # every shell call is refused (fail-closed) unless the operator opts into
    # the unsandboxed fallback.
    && apt-get install -y --no-install-recommends ca-certificates curl tini bubblewrap \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 --shell /usr/sbin/nologin pantheon \
    && mkdir -p /data \
    && chown pantheon:pantheon /data && chmod 700 /data

COPY --from=build /src/target/release/pantheon /usr/local/bin/pantheon

# The container has no interactive terminal; the CLI's own paths still work
# (`docker exec -it pantheon pantheon doctor`), but the default command is the
# server surface so `docker run` gives you a listening service.
ENV PANTHEON_DATA_DIR=/data \
    HOME=/home/pantheon \
    RUST_BACKTRACE=1

USER pantheon
WORKDIR /home/pantheon
VOLUME ["/data"]
EXPOSE 18789

# /agui/health is the one unauthenticated route, which makes it the right probe.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS http://127.0.0.1:18789/agui/health >/dev/null || exit 1

# tini reaps the agent/subprocess children Pantheon spawns (PTY sessions, MCP
# servers); without it a long-running container accumulates zombies.
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["pantheon", "serve", "--host", "0.0.0.0", "--port", "18789"]
