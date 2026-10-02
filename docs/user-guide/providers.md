# Providers

Your agent never picks its own model. You set a default, a backup list for when the first one fails, and optionally small specialist models for specific jobs. Swap any of them without touching the agent.

## Default model

```toml
[model]
provider    = "openai"
model       = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"
reasoning   = "high"   # optional: off|minimal|low|medium|high|xhigh|max
```

`pantheon model` shows the built-in catalog (39+ models) and saves keys into `<data_dir>/.env`. The config file holds names, never the keys themselves. `pantheon providers` lists everything available. Switch mid-conversation with `/model <provider> <model>`; your conversation and memory carry over.

## Recommended provider

Nous Research (`nous`) is the recommended provider: the setup wizard
lists it first and `pantheon providers` marks it with ★.

- Endpoint: `https://inference-api.nousresearch.com/v1` (OpenAI-compatible)
- Key: `NOUS_API_KEY` - create one at [portal.nousresearch.com](https://portal.nousresearch.com). The Portal offers an evaluation tier; which models your key can call depends on your account.
- Curated models: `Hermes-4-70B` and `Hermes-4-405B` (Hermes 4, hybrid-reasoning chat models). The Portal also proxies frontier models from other labs - `pantheon provider models nous` lists the live catalog.

## Backups and helpers

```toml
[[model.fallbacks]]
provider = "anthropic"
model    = "claude-sonnet-4-5"
```

Backups kick in only when the main model fails, in order. You can also pin small models for specific jobs (`[judge]`, `[compression]`, `[title_gen]`, `[embeddings]`, `[search_synthesis]`, `[vision]`, `[scheduled]`, `[mcp_synthesis]`). Leave one unset and it just uses the main model, except embeddings, which default to a local one.

## Cloud providers (templated endpoints)

Four cloud providers need account-scoped values before they work
`pantheon model` prompts for them and stores them in `<data_dir>/.env`.
Model lists are empty for these, so type the model id by hand.

**Google Vertex AI** (`vertex`)
- Needs `PANTHEON_VERTEX_PROJECT` and `PANTHEON_VERTEX_LOCATION`
  (e.g. `us-central1`).
- Key: `GOOGLE_ACCESS_TOKEN` - a short-lived GCP OAuth token
  (`gcloud auth print-access-token`, ~1h). There is no refresh flow:
  when it expires, mint a new one and re-run `pantheon model`.
- Model ids are `publisher/model`, e.g. `google/gemini-2.5-flash`.

**AWS Bedrock** (`bedrock`)
- Needs `PANTHEON_BEDROCK_REGION` (e.g. `us-east-1`).
- Key: `AWS_BEARER_TOKEN_BEDROCK` - a Bedrock API key (long-term key, or
  a 12h token minted with `aws-bedrock-token-generator`). Bearer auth
  only; AWS SigV4 signing is not implemented.
- Model ids are inference-profile ids, e.g. `us.anthropic.claude-sonnet-4-6`
  or `global.openai.gpt-5.6-terra` - check `aws bedrock list-inference-profiles`
  for what your account can call.

**Azure AI Foundry** (`azure`)
- Needs `PANTHEON_AZURE_RESOURCE` (your resource name).
- Key: `AZURE_OPENAI_API_KEY`. Sent in the `api-key` header per
  Microsoft's API reference, not as `Authorization: Bearer`.
- Model id = your **deployment name**, not the model catalog id.

**Cloudflare Workers AI** (`cloudflare`)
- Needs `PANTHEON_CLOUDFLARE_ACCOUNT_ID`.
- Key: `CLOUDFLARE_API_TOKEN` - create it from the Cloudflare dashboard
  (AI → Workers AI → Use REST API shows your account id and offers the
  Workers AI token template). Sent as `Authorization: Bearer`.
- Model ids are full `@cf/vendor/model` paths, e.g.
  `@cf/meta/llama-3.1-8b-instruct` - a bare name will not work.

Without real credentials none of the four can be exercised end to end:
endpoint shape, auth headers, and template resolution are covered by
unit tests against the documented API shapes, but the live token
exchange for each vendor remains unverified.

## Custom endpoints

Any OpenAI-compatible URL works as a provider:

```sh
pantheon provider add --name my-llm --base-url http://127.0.0.1:8015/v1
pantheon provider models my-llm
```

Only model names you type by hand are saved in the config. Live model lists are fetched fresh each time, never baked in.

## Reliability

If a provider says "slow down" (429), Pantheon waits as long as asked (up to a minute) and retries. Errors stay structured, so scripts can tell a rate limit from a bad key. Asking for structured JSON output works on both the OpenAI and Anthropic connections.

## See also

- [Agents](agents.md): per-agent model settings
- [Configuration](../reference/configuration.md): every provider field
