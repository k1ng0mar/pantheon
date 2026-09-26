# Provider registry

39 builtins in `crates/pantheon-core/catalog.yaml` plus user customs
(`[custom_providers.*]`, managed by `pantheon provider`). Every row
resolves the same way: base URL (+`{var}` templates) → wire mode →
key env → key header. `pantheon model` discovers models live
(`GET {base}/models`) and falls back to curated rows, then manual entry.

Legend — **discovery**: yes = standard OpenAI-shape `/models` confirmed;
manual = no listing endpoint (or non-standard); type the id.
**auth**: Bearer unless noted.

## Majors + clouds

| Provider | Base URL | Auth / key env | Discovery | Notes |
|---|---|---|---|---|
| openai | `https://api.openai.com/v1` | Bearer · `OPENAI_API_KEY` | yes | Baseline |
| anthropic | `https://api.anthropic.com/v1` | `ANTHROPIC_API_KEY` | manual | Native Messages API; no `/models` — type the id |
| google | `https://generativelanguage.googleapis.com/v1beta/openai` | Bearer · `GEMINI_API_KEY` | yes | OpenAI compatibility layer |
| vertex | `https://aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/endpoints/openapi` | Bearer access token · `GOOGLE_ACCESS_TOKEN` | manual | Template: `project`, `location`; short-lived token |
| bedrock | `https://bedrock-runtime.{region}.amazonaws.com/openai/v1` | Bearer Bedrock key · `AWS_BEARER_TOKEN_BEDROCK` | manual | Template: `region`; SigV4 not yet — Bearer key works; model ids like `anthropic.claude-...:0` |
| azure | `https://{resource}.openai.azure.com/openai/v1/` | Bearer/key · `AZURE_OPENAI_API_KEY` | manual | Template: `resource`; **model id = deployment name** |
| huggingface | `https://router.huggingface.co/v1` | Bearer · `HF_TOKEN` | yes | `org/model:provider` routing |
| openrouter | `https://openrouter.ai/api/v1` | Bearer · `OPENROUTER_API_KEY` | yes | 500+ models, fallback routing |

## Aggregators / inference clouds

| Provider | Base URL | Auth / key env | Discovery | Notes |
|---|---|---|---|---|
| fireworks | `https://api.fireworks.ai/inference/v1` | Bearer · `FIREWORKS_API_KEY` | yes | Open models |
| together | `https://api.together.xyz/v1` | Bearer · `TOGETHER_API_KEY` | yes | Open models |
| groq | `https://api.groq.com/openai/v1` | Bearer · `GROQ_API_KEY` | yes | Fast inference |
| deepinfra | `https://api.deepinfra.com/v1/openai` | Bearer · `DEEPINFRA_API_KEY` | likely, unverified | Note the `/v1/openai` path — completions join correctly |
| novita | `https://api.novita.ai/openai/v1` | Bearer · `NOVITA_API_KEY` | likely, unverified | `provider/model` routing |
| nebius | `https://api.tokenfactory.nebius.com/v1` | Bearer · `NEBIUS_API_KEY` | yes | Token Factory; AI Studio (`api.studio.nebius.ai/v1`) via override |
| cerebras | `https://api.cerebras.ai/v1` | Bearer · `CEREBRAS_API_KEY` | yes | Fast inference |
| sambanova | `https://api.sambanova.ai/v1` | Bearer · `SAMBANOVA_API_KEY` | manual | **No listing endpoint** (confirmed) — type the id |
| nvidia-nim | `https://integrate.api.nvidia.com/v1` | Bearer · `NVIDIA_API_KEY` | yes | GPU-optimized inference |
| cloudflare | `https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1` | Bearer token · `CLOUDFLARE_API_TOKEN` | manual | Template: `account_id` |

## Labs

| Provider | Base URL | Auth / key env | Discovery | Notes |
|---|---|---|---|---|
| deepseek | `https://api.deepseek.com/v1` | Bearer · `DEEPSEEK_API_KEY` | yes | Cheap reasoning |
| mistral | `https://api.mistral.ai/v1` | Bearer · `MISTRAL_API_KEY` | yes | European lab |
| xai | `https://api.x.ai/v1` | Bearer · `XAI_API_KEY` | yes | Grok |
| qwen | `https://dashscope.aliyuncs.com/compatible/v1` | Bearer · `DASHSCOPE_API_KEY` | manual | Compat endpoints expose **no** `/models` (confirmed) — type the id; intl alt: `dashscope-intl.aliyuncs.com/compatible-mode/v1` |
| zhipu | `https://open.bigmodel.cn/api/paas/v4` | Bearer · `ZHIPUAI_API_KEY` | likely, unverified | GLM |
| moonshot | `https://api.moonshot.ai/v1` | Bearer · `MOONSHOT_API_KEY` | likely, unverified | Kimi |
| minimax | `https://api.minimax.io/v1` | Bearer · `MINIMAX_API_KEY` | yes | Agent models |
| upstage | `https://api.upstage.ai/v1` | Bearer · `UPSTAGE_API_KEY` | likely, unverified | Solar |
| cohere | `https://api.cohere.ai/compatibility/v1` | Bearer · `COHERE_API_KEY` | likely, unverified | Command via compatibility layer |
| perplexity | `https://api.perplexity.ai` | Bearer · `PERPLEXITY_API_KEY` | likely, unverified | Search-grounded; has a Models API (shape unconfirmed — fallback covers) |
| mimo | `https://api.xiaomimimo.com/v1` | **`api-key` header, raw key** · `MIMO_API_KEY` | likely, unverified | Non-Bearer auth — the per-provider `key_header` path exists for this |

## Local

| Provider | Base URL | Auth / key env | Discovery | Notes |
|---|---|---|---|---|
| local (Ollama) | `http://127.0.0.1:11434/v1` | none needed | yes | `/v1/models` lists pulled models |
| lmstudio | `http://127.0.0.1:1234/v1` | none needed | yes | `/v1/models` lists loaded models |
| router | `http://127.0.0.1:8015/v1` | Bearer · `PANTHEON_KEY_ROUTER` | yes | Local llm-router pools: `chat` / `code` / `media` |

## Inference clouds

| Provider | Base URL | Auth / key env | Discovery | Notes |
|---|---|---|---|---|
| nous | `https://inference-api.nousresearch.com/v1` | Bearer · `PANTHEON_KEY_NOUS` | likely, unverified | Open models |
| tokenrouter | `https://api.tokenrouter.com/v1` | Bearer · `PANTHEON_KEY_TOKENROUTER` | likely, unverified | Aggregator |
| gmi | `https://api.gmi-serving.com/v1` | Bearer · `PANTHEON_KEY_GMI` | likely, unverified | Inference cloud |
| bai (B.ai) | `https://api.b.ai/v1` | Bearer · `PANTHEON_KEY_BAI` | likely, unverified | Inference cloud |
| experiential | `https://api.experientiallabs.ai/v1` | Bearer · `PANTHEON_KEY_EXPERIENTIAL` | likely, unverified | Inference cloud |
| agnes | `https://apihub.agnes-ai.com/v1` | Bearer · `PANTHEON_KEY_AGNES` | likely, unverified | Inference cloud |
| xkiro | `https://api.xkiro.com/v1` | Bearer · `PANTHEON_KEY_XKIRO` | likely, unverified | Inference cloud |

## Custom endpoints

Anything else: `pantheon provider add` (interactive or flagged), then
`pantheon model` to select it. `:port` expands to
`http://127.0.0.1:port/v1`; `{var}` templates resolve from
`PANTHEON_<PROVIDER>_<VAR>` like builtins.
