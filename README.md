# Pantheon

Small at the center, huge at the edges.

The model is not the runtime. The model is one replaceable component inside it. Runtime owns lifecycle, state, policy, execution, capabilities, recovery, and events. Agents never pick models.

Technical spec: [ARCHITECTURE.md](./ARCHITECTURE.md).

## Layout

```
crates/*          Rust runtime (the thing we own)
eval/             regression harness (stdlib Python + cases.json)
vendor/           ported Hermes plugins used as fixtures
ARCHITECTURE.md   full technical architecture
```

## Model policy (locked)

No model routing.

- **default model** — the run’s model
- **fallback models** — policy-ordered list, used only when the default fails. Runtime-controlled, never agent-chosen
- **auxiliary models** — task-scoped helpers (embeddings, rerank, STT/TTS, vision, extraction, search synthesis), selected by runtime capability need

## Build

```sh
cargo build
cargo test --workspace
./target/debug/pantheon
```

## AG-UI and channels

The local AG-UI server exposes a small SSE web client at `/`, JSON-RPC at
`/agui/rpc`, and signed generative-UI artifacts at `/agui/blob/<task_id>`:

```sh
pantheon serve --host 127.0.0.1 --port 18789
pantheon stream <run_id>
pantheon sign <task_id> --mime text/plain --ttl 3600000
```

Discord and Telegram adapters implement the `Channel` seam with platform
message limits, Unicode-safe chunking, and approval actions. Their REST
transports are enabled by the existing gateway `ureq` dependency.

## CLI

```
pantheon run [--id ID] [--say TEXT] [--tool NAME] [--fail CODE] [--ext]
pantheon explain <run_id>
pantheon status <run_id>
pantheon extensions
pantheon hook <name> [--session S]
pantheon doctor <plugin_dir>
```

Optional env: `PANTHEON_DATA_DIR`, `PANTHEON_EXT_DIR`.

## Eval

```sh
cargo build -p pantheon-cli
python3 eval/run.py
python3 eval/run.py --cargo-tests
```

Exit 0 only when every active case passes.
