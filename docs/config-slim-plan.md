# Config slim plan - PROPOSAL ONLY

**Status:** Draft. No code changed. Umar approves before any destructive
simplification. The full config document currently lives in
`crates/pantheon-tui/src/config.rs` (moved there mid-consolidation; it was in
`pantheon-api` an hour ago - the plan must be re-based on wherever `Config`
settles before implementation).

## Measured surface (2026-09-29)

- **31 top-level keys** on `Config` (`config.rs:553`), **83 leaf fields**
  across all section structs.
- **14 named `AuxiliaryKind` variants** (`crates/pantheon-api/src/model.rs:48`;
  `Other(String)` catch-all extra), 12 of which have a registered
  `AUX_SLOTS` entry with env-prefix + vault wiring.
- 6 auxiliary slots have **zero call sites** outside resolution:
  `SearchSynthesis`, `McpSynthesis`, `Extraction`, `Rerank`, `Planner`
  (0 referencing crates), and `Vision` (1, no pipeline). The code itself
  says "no call sites yet - the slot exists so a model can be pinned ahead
  of the workload landing."
- `docs/reference/configuration.md` already promises: "Leave one out and it
  uses your main model."

## Target shape

Two first-class choices plus explicit overrides - everything else folds into
sensible defaults:

```toml
[model]
provider    = "openai"
model       = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"
# reasoning = "medium"        # optional, as today

[models.cheap]
provider    = "openai"
model       = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"

# Only when a specific job genuinely needs a different model:
[models.judge]
provider = "anthropic"
model    = "claude-haiku-4-5"
```

Resolution rule, in priority order: per-slot override (`[models.<slot>]`) →
`[models.cheap]` for background/host-orchestrated slots → `[model]` for the
interactive default. Embeddings keeps its special case: absent = local
embedder, never chat. `verify` keeps its special case: absent = OFF entirely
(pin a cheap model to turn it on).

Env overrides survive the merge: `PANTHEON_<PREFIX>_MODEL` / `_PROVIDER`
per slot collapse to `PANTHEON_MODELS_CHEAP_*` for the cheap tier and
`PANTHEON_MODELS_<SLOT>_*` for overrides.

## Collapse / remove / stay

### Collapse into `[models.cheap]` (11 slots)

`judge`, `compression`, `title_gen`, `search_synthesis`, `vision`,
`scheduled`, `mcp_synthesis`, `extraction`, `rerank`, `planner`,
`reflection`/`consolidation` model pins. All are "same shape, different
job" pins; nothing in their semantics requires a distinct section. The
`reflect`/`consolidation` *behavior knobs* (`enabled`, `auto_turns`,
`max_proposals`, `cron`, ...) stay, but the model pin moves to `[models.*]`
and `nightly` becomes the single behavior section (see below).

### Remove outright (5 keys)

- `tools` - already inert (`config.rs`: retained only so old files still
  load; "the keys are inert"). Delete the field; unknown `[tools]` in an
  old file becomes a parse warning, not an error.
- `profile` - informational only, "written by `setup` as \"default\", and
  read by nothing that runs an agent." Delete; `agent` is the real selector.
- `verify` section - keep the *slot* (it has a real fail-closed semantic),
  but as `[models.verify]` under the new scheme with absent = OFF.
- `extraction`, `rerank`, `planner` sections - no call sites; they become
  available as `[models.extraction]` etc. only when someone writes the
  workload. No new user writes a section for a nonexistent feature today.
- `scheduled` section - one referencing crate; fold pin into cheap tier.

### Stay as-is (sections with real behavior)

`model` (+fallbacks), `policy`, `memory`, `server`, `secrets`, `retention`,
`temporal`, `budget`, `goal`, `browser`, `websearch`, `mcp`,
`tui`, `approvals`, `nightly`, `stt`/`tts` (merge into one `voice` pair?
see open questions), `custom_providers`, `agents`, `agent`.

### Merge candidates

- `reflect` + `consolidation` + `nightly`: `nightly` already "merges the old
  `[reflect]` and `[consolidation]` behavior knobs" with legacy fallback.
  Finish the job: one `[nightly]` section, legacy sections read-only for
  migration.
- `stt` + `tts`: both are `VoiceSection`; a single `[voice]` with
  `stt_model`/`tts_model` keys would do, but this is cosmetic - defer.

## Migration path for existing `config.toml` files

1. **Read path stays tolerant for two releases.** `Config` keeps
   `#[serde(default)]` on everything; old section names (`[judge]`,
   `[compression]`, ...) deserialize into a deprecated alias layer that maps
   onto `[models.<slot>]`. `pantheon doctor` reports each remapped section
   with the new spelling.
2. **`pantheon config migrate` (new subcommand).** Rewrites the file in
   place: folds legacy aux sections into `[models.cheap]` or
   `[models.<slot>]`, drops inert keys (`tools`, `profile`), merges
   `reflect`/`consolidation` into `nightly`. Writes a `.bak` first, prints
   a diff, exits non-zero if anything can't be mapped automatically.
3. **Env-var migration.** Old `PANTHEON_JUDGE_MODEL`-style vars keep working
   via the alias layer in (1); `doctor` nudges toward the new names. Remove
   the aliases after two releases.
4. **No silent behavior change.** The alias layer must reproduce today's
   resolution exactly (including embeddings-local and verify-OFF), or the
   migration is a behavior change wearing a refactor's clothes.

## Open questions for Umar

- Is `[models.cheap]` the right name, or `[model.secondary]` / `[aux]`?
  ("cheap" encodes a cost assumption; "background" encodes the scheduling
  reality.)
- `stt`/`tts`: merge into `[voice]` now or leave?
- Keep per-slot env prefixes (`PANTHEON_RERANK_MODEL`) as permanent aliases,
  or sunset them? They're arguably the more ergonomic interface for Docker
  users.
- The vault seeding (`PANTHEON_<PREFIX>_API_KEY` per slot): does the vault
  keep per-slot entries, or collapse to two (default + cheap)?
