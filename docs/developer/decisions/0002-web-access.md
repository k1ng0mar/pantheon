# 0002. Web access layer: browser + search

Status: **implemented, uncommitted** (September 2026).

## Decision

Two separate systems, not one "web tool":

- `web_search` (new `pantheon-websearch` crate, Tavily provider) - query →
  snippets. For *looking things up*.
- `browser_*` (new `pantheon-browser` crate, gsd-browser subprocess wrapper)
- navigate, snapshot/click_ref/fill_ref, extract, act, screenshot. For
  *doing things on a live site*.

The model differentiates by intent; every browser tool's description teaches
the split.

## Why subprocess, not native CDP

Reimplementing the CDP daemon is 2-4 engineer-months for no capability gain.
The wrapper sits behind a `BrowserBackend` trait, so a native implementation
can replace the subprocess later without changing the agent-facing tool
schema. gsd-browser is MIT/Apache-2.0; attribution only.

## Why the browser runs outside the namespace sandbox

Deliberate: a browser that cannot reach the network or its own Unix socket
cannot browse. The sandbox's whole purpose (restricting egress) contradicts
the tool's purpose. The authorization boundary is the capability gate
(`browser` / `browser.act`), and the binary is user-installed trusted
software - same trust class as the browser itself.

## Why `browser_act` needs approval

Upstream `act` clicks the top semantic-intent candidate with no minimum
score threshold. It carries a dedicated `browser.act` capability (split from
`browser`, mirroring the GitPush/ShellExecute split); default policies mark
it Approval, so the run parks for a human. The tool description steers the
model to the safer pattern: snapshot → verify the ref → `click_ref`.

## Provider choice

Tavily: one REST endpoint, structured JSON, good snippets, generous free
tier. `SearchProvider` trait is the seam for future Brave/Serper backends.
API key resolves via the secrets broker (`TAVILY_API_KEY`); without a key
`web_search` is not registered - a dead tool stays out of the model's list.

## Lifecycle

One gsd-browser daemon session per Pantheon run (`--session` = sanitized run
id). Lazy start on first `browser_*` call; GC stops sessions idle > 900s
(best-effort `daemon stop`; degrades to bookkeeping if the subcommand
differs). Vault key resolves at registration and is injected as
`GSD_BROWSER_VAULT_KEY` per spawn - never logged, never in session state.
