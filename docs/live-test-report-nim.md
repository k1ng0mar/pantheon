# Live test pass: Pantheon × NVIDIA NIM

Date: 2026-09-30. Branch: `chloe/tui-redesign` (working tree, nothing committed or pushed).
Model under test: `meta/llama-3.2-11b-vision-instruct` via provider `nvidia-nim`
(`https://integrate.api.nvidia.com/v1`). Key supplied by Umar, used only through the
`NVIDIA_API_KEY` process env var - never written to disk, never printed, never in a URL.

All runs used an isolated data dir (`/tmp/pantheon-live-test`); Umar's real `~/.pantheon`
was untouched.

## Environment caveat (read first)

The sandbox blocks direct egress and TLS-intercepts with its own CA (`Hatch Sandbox
Egress CA`). Pantheon's HTTP client (ureq 2.12) could not reach ANY external HTTPS host
from this VM - verified for both `integrate.api.nvidia.com` and `api.openai.com`.
Two findings, both fixed in the working tree (uncommitted):

1. **ureq ignored the proxy env vars.** ureq 2.12 only honors them with the
   `proxy-from-env` feature, which Pantheon hadn't enabled. Added to 7 crates
   (pantheon-exec, pantheon-gateway, pantheon-mcp, pantheon-memory,
   pantheon-providers, pantheon-tui, pantheon-web). This is a real product gap:
   Pantheon was unusable behind any corporate proxy.
2. **ureq ignored `NO_PROXY` entirely** (no such support in ureq 2.12) and its
   rustls handshake could not complete through the intercepting proxy
   (OpenSSL-based clients - curl, Python - worked fine). So for this pass, Pantheon
   talked plain HTTP to a localhost test relay that forwarded to NIM over working TLS.
   Everything above the TLS layer - provider resolution, key handling, request
   building, response parsing, the runtime agent loop, aux dispatch - is Pantheon's
   real code. The relay is throwaway scaffolding in /tmp, not part of the repo.

## Results

| # | Test | Model | Result | Latency | Notes |
|---|------|-------|--------|---------|-------|
| 1 | `pantheon provider models nvidia-nim` (live `GET /models` through Pantheon's fetch code) | - | PASS | 1.1s | **81 models** live at the endpoint (full list in § Model inventory) |
| 2 | Real agent turn, text answer (`run --deliver session --say "In one short sentence, what is photosynthesis?..."`) | meta/llama-3.2-11b-vision-instruct | PASS | 5.2s | Correct one-sentence answer; run `run_1790762403774_1152` |
| 3 | Real agent turn with tools (`--say "List the files in /tmp... Use a tool to look."`) | same | PASS | 36.2s | 4 model round-trips, 3 tool executions; model tried /tmp, was refused (outside workspace root - sandbox boundary held), recovered and listed the workspace |
| 4 | Title aux (`title_gen`) | same | PASS | - | Fired live: model-generated session titles in the ledger (`SessionTitled ... "NIM live test"`, `"Explaining Photosynthesis Basics"`, ...) |
| 5 | Usage/cost ledger | same | PASS | - | `UsageRecorded` events with real token counts (e.g. 6444 in / 26 out); `cost_usd: null` (no NIM pricing data - expected) |
| 6 | Swarm, n=2 (`pantheon swarm 2 "..." --roles w1,w2`) | same | PASS | 10.2s | `swarm-1790762575506_6293`: complete, 2/2 agents ok; both children ran on NIM |
| 7 | Verify aux (`[verify]` pinned to NIM) | same | NOT EXERCISED | - | `verify_delegation` only fires on delegate-tool child results; the 11b model emitted the delegate call as text instead of a tool call, so the path never ran. Client code verified present in `session.rs`. |
| 8 | Vision aux | - | NO DISPATCH PATH | - | Vision model pin exists (`pantheon model --auxiliary vision`), but no runtime/CLI call site dispatches it in this tree. |
| 9 | Judge / compression / extraction / planner / reflection / consolidation / repair / scheduled aux | - | NOT CLI-DISPATCHABLE | - | Internally triggered only (eval harness, context fit, nightly pass). Not faked. |
| 10 | Embeddings / rerank | - | UNSUPPORTED ON THIS KEY | - | No CLI dispatch; direct probe: `POST /v1/embeddings` and `/v1/ranking` → **404 "Not found for account"** on this key |
| 11 | Dashboard UI (unified listener, :7171) | - | PASS | - | Runs table rendered with the live NIM runs, token counts, model names; mobile viewport renders the PWA-style overview. Screenshots below. |

### Model probes (direct, pre-Pantheon)

- `openai/gpt-oss-20b`: works, but reasoning-style (`content: null`, reasoning in `reasoning_content`).
- `meta/llama-3.2-11b-vision-instruct`: works, clean content - chosen as the workhorse (also vision-capable).
- `nvidia/nemotron-3.5-lightning-30b-a3b`: works, but leaks reasoning chatter into content.
- 404 on this account: `mistralai/mistral-7b-instruct-v0.3`, `moonshotai/kimi-k2.6`, `google/gemma-3-12b-it`, `mistralai/mistral-large-2-instruct`, `nvidia/llama-3.1-nemotron-70b-instruct`, embeddings models, `/v1/ranking`.
- `deepseek-ai/deepseek-v4.1-flash`: connection timeout (twice).

### Behavior notes

- The 11b model reaches for `ask_user` eagerly (3 of the test turns parked awaiting operator input instead of answering). Not a Pantheon bug, but it makes non-interactive `run --deliver session` turns park; the dashboard shows these as "running ... awaiting operator input".
- Swarm reports `complete (2/2 agents ok)` even when a child parked on `ask_user` - worth a product decision.
- **Dashboard bug found during screenshots:** `.gate { display: flex }` in `style.css` overrides the `hidden` attribute, so the token gate can render even with a valid token. (Uncommitted; flagging, not fixing without direction.)

## Screenshots

All under `docs/live-test-shots/` (fresh filenames, no API key in any frame):

- `nim-live-dash-runs-20260930b.png` - dashboard Runs table with the live NIM runs
- `nim-live-dash-mobile-20260930e.png` - dashboard at mobile viewport (PWA-style overview)
- `nim-live-terminal-models-20260930a.png` - verbatim `provider models` output (81 models)
- `nim-live-terminal-turn-20260930a.png` - verbatim agent-turn outputs (text + tool turn)
- `nim-live-terminal-swarm-20260930a.png` - verbatim swarm output

## Out of scope (not tested, not faked)

STT/TTS providers, MCP server launches, Telegram/Discord delivery, cloud browser
backends, nightly repair pass (off by default), memory backends beyond native SQLite.

## Test suite / hygiene

- `cargo test --workspace`: **blocked by concurrent work in the shared tree** (see below).
  Verified green with proxy env unset: `pantheon-providers` 142/142,
  `pantheon-memory` 31/31.
- `cargo fmt --check`: **1 pre-existing diff** in `crates/pantheon-migration/src/lib.rs`
  (import ordering; not from this pass - my changes were Cargo.toml feature lines only).
- Key-leak grep: clean. No `nvapi-` string anywhere in the repo (excluding target/
  and .git); no `.env` was created in the test data dir - the key lived only in the
  process env of the test shells. The report and screenshots contain no key material.
- Git: branch `chloe/tui-redesign`, nothing committed, nothing pushed. The ureq
  `proxy-from-env` + `native-certs` feature additions are uncommitted working-tree
  changes.

### Tree-sharing caveats (another agent is actively editing this tree)

- `pantheon-eval` does not compile: 4 errors in `eval/tests/migration_carry.rs`
  (missing `write_session_import`, `count_jsonl_records`) - other agent's WIP.
- `pantheon-storage` does not compile: 2 × E0004 in `ledger.rs` - the other agent
  added an `Event::UserMessage` variant to `pantheon-api` without updating the
  matches. This broke the workspace build between my test runs.
- The 3 `pantheon-memory` http_backend failures seen WITH proxy env set are caused
  by my `proxy-from-env` change: ureq 2.12 ignores `NO_PROXY`, so test-localhost
  HTTP goes through the egress proxy and times out. **Product implication: the
  proxy fix needs NO_PROXY handling, or proxied users lose localhost services
  (dashboard, gateway, MCP).** With proxy env unset, 31/31 pass.

## Model inventory (81, live from GET /models)

01-ai/yi-large, adept/fuyu-8b, ai21labs/jamba-1.5-large-instruct, aisingapore/sea-lion-7b-instruct, bigcode/starcoder2-15b, databricks/dbrx-instruct, deepseek-ai/deepseek-coder-6.7b-instruct, deepseek-ai/deepseek-v4.1-flash, google/codegemma-1.1-7b, google/codegemma-7b, google/deplot, google/diffusiongemma-26b-a4b-it, google/gemma-2b, google/gemma-3-12b-it, google/gemma-3-4b-it, google/gemma-4-31b-it, google/recurrentgemma-2b, ibm/granite-3.0-3b-a800m-instruct, ibm/granite-3.0-8b-instruct, ibm/granite-34b-code-instruct, ibm/granite-8b-code-instruct, meta/codellama-70b, meta/llama-3.2-11b-vision-instruct, meta/llama-3.2-90b-vision-instruct, meta/llama-guard-4-12b, meta/llama2-70b, meta/muse-glimmer-30b, microsoft/kosmos-2, microsoft/phi-3-vision-128k-instruct, microsoft/phi-3.5-moe-instruct, mistralai/codestral-22b-instruct-v0.1, mistralai/mistral-7b-instruct-v0.3, mistralai/mistral-large, mistralai/mistral-large-2-instruct, mistralai/mixtral-8x22b-v0.1, moonshotai/kimi-k2.6, moonshotai/kimi-k3, nv-mistralai/mistral-nemo-12b-instruct, nvidia/ai-synthetic-video-detector, nvidia/cosmos-reason2-8b, nvidia/embed-qa-4, nvidia/ising-calibration-1.5-31b, nvidia/llama-3.1-nemoguard-8b-content-safety, nvidia/llama-3.1-nemoguard-8b-topic-control, nvidia/llama-3.1-nemotron-51b-instruct, nvidia/llama-3.1-nemotron-70b-instruct, nvidia/llama-3.1-nemotron-safety-guard-8b-v3, nvidia/llama-3.1-nemotron-ultra-253b-v1, nvidia/llama-3.2-nemoretriever-1b-vlm-embed-v1, nvidia/llama-3.2-nv-embedqa-1b-v1, nvidia/llama-nemotron-embed-vl-1b-v2, nvidia/llama3-chatqa-1.5-70b, nvidia/mistral-nemo-minitron-8b-8k-instruct, nvidia/nemotron-3-embed-1b, nvidia/nemotron-3-nano-omni-30b-a3b-reasoning, nvidia/nemotron-3-super-120b-a12b, nvidia/nemotron-3-ultra-550b-a55b, nvidia/nemotron-3.5-content-safety, nvidia/nemotron-3.5-lightning-30b-a3b, nvidia/nemotron-4-340b-instruct, nvidia/nemotron-4-340b-reward, nvidia/nemotron-nano-3-30b-a3b, nvidia/nemotron-parse, nvidia/nemotron-parse-2.0, nvidia/neva-22b, nvidia/nv-embedqa-mistral-7b-v2, nvidia/nvclip, nvidia/riva-translate-4b-instruct, nvidia/riva-translate-4b-instruct-v1.1, nvidia/riva-translate-4b-instruct-v2, nvidia/vila, openai/gpt-oss-20b, poolside/laguna-xs-2.1, snowflake/arctic-embed-l, writer/palmyra-creative-122b, writer/palmyra-fin-70b-32k, writer/palmyra-med-70b, writer/palmyra-med-70b-32k, z-ai/glm-5.3, z-ai/glm-5.3-flash, zyphra/zamba2-7b-instruct
