# Executable skills

Skills are markdown plus, optionally, **executables**: scripts or npx
packages the agent can run. There are two worlds, and they are deliberately
separate.

## World 1: declared executables (`exec:` frontmatter + `skill_exec`)

A skill declares executables in its frontmatter. This is the first-class
Pantheon path: declared, validated at discovery, sandboxed at run time,
attributed on the timeline.

```yaml
exec:
  - name: fetch-transcript # slug: [a-z0-9-], 1..=64
    description: "Fetch a YouTube video transcript as plain text"
    command: scripts/fetch-transcript.sh # relative path (shell/python3/node) OR npx spec when runtime: npx
    args: "<youtube-url>" # usage hint
    runtime: shell # shell | python3 | node | npx (default: shell)
    side_effects: read # read | write (default: write; fail-closed)
    timeout_secs: 60 # default 60, max 600
```

Rules, enforced fail-closed:

- `name` must match `[a-z0-9-]{1,64}`.
- `command` for `shell`/`python3`/`node` is a **relative path** inside the
  skill dir: no absolute paths, no `..`, no `.`, no empty segments, no
  backslashes, no NUL. At invocation the path is canonicalized and
  re-proven to be inside the skill dir, so a symlink planted after
  discovery cannot escape (`SKILL_EXEC_ESCAPE`).
- `command` for `npx` is a package spec: `[@scope/]name[/subpath][@version]`,
  lowercase ASCII only. It is **never** resolved to a local file.
- `timeout_secs` must be 1..=600. The sandbox wall clock is set from it.
- `side_effects` defaults to `write`. `npx` is always `write`, even when
  declared `read` — fetching and running remote code is a side effect.
- One bad entry fails the **whole skill** at discovery (`SKILL_BAD_EXEC`,
  naming the skill and the entry index). A half-declared executable is
  worse than none.

### The `skill_exec` tool

Registered whenever at least one skill is installed. Arguments:

```json
{ "skill": "design-references", "name": "fetch-transcript", "args": ["<url>"] }
```

- Each entry of `args` becomes a separate **argv element** — never
  shell-interpolated. `["a;b"]` is data, not a command separator.
- Working directory is the skill dir, so scripts resolve bundled
  `scripts/` and `references/` relatively.
- `${CLAUDE_SKILL_DIR}`, `${SKILL_DIR}` and `$SKILL_DIR` in args expand to
  the skill's absolute directory (the de-facto third-party convention;
  expansion is pure string substitution, still argv, never a shell).
- Runs under the HIGH sandbox profile (`bwrap`), wall clock from
  `timeout_secs`. Output capped at 256 KiB with a truncation marker; the
  exit code rides in the result text (`(exit 3)`), timeouts are errors
  (`SANDBOX_TIMEOUT`).
- If the sandbox binary is unavailable the call fails closed
  (`SANDBOX_UNAVAILABLE`), unless the operator explicitly opted into the
  direct-spawn fallback — which is then labeled honestly in the output.

### Capability gating

`skill_exec` is statically `FilesystemRead`. Per call it additionally
requires `ShellExecute` when the named executable declares
`side_effects: write`, so write executables flow through the normal
approval path and `read` executables behave like read-only tools. An
unresolvable skill/executable adds no capabilities — the executor
rejects the call itself.

### Plan mode

Plan mode blocks mutating tools. For `skill_exec` the classification is
args-aware: **write → blocked, read → allowed, unknown → blocked**
(fail closed). The check uses the skills cached at registry build, so it
always agrees with the registered tool.

### Timeline attribution

> **Deviation, stated loudly:** `pantheon-api` is out of scope for this
> workstream, so there is no structured event field for skill attribution.
> Instead, `ToolStarted` events for `skill_exec` carry
> `Provenance::system("skill:<name>")` — same trust tier as any system
> tool call, only the source string names the skill. The args JSON also
> carries the skill name natively. A future structured field can replace
> this without changing the tool.

## World 2: third-party prose invocation (`${CLAUDE_SKILL_DIR}` + `shell`)

Real third-party skills (observed in the wild: `impeccable`,
`ui-ux-pro-max`) declare **nothing** in frontmatter. Their SKILL.md body
tells the agent to run something like:

```sh
"${CLAUDE_SKILL_DIR}/scripts/impeccable" context
```

The agent runs these via the regular `shell` tool, expanding the
placeholder itself. To make that possible, discovery exposes each skill's
absolute directory:

- `Skill.dir` in the discovery metadata (populated from the SKILL.md
  parent; falls back to the parent for older serialized metadata).
- The `skill_exec` tool description lists every installed skill as
  `"name": /absolute/dir`, under a "Skill directories" section that states
  the convention explicitly.

`skill_exec` **only** runs `exec:`-declared entries. It never runs
prose-invoked scripts: those never passed validation, and silently
upgrading them into the declared path would erase the distinction the
agent (and the audit trail) relies on.

## Supply-chain notes

- **npx**: runs remote code at invocation time. Always `write`
  side-effects, always approval-gated, always sandboxed. Prefer vendored
  scripts for anything the skill runs routinely.
- **First-run download**: some third-party skills ship a launcher that
  downloads its real payload on first run (observed: `impeccable`). Treat
  this as the npx caveat wearing a different hat — a network fetch at
  runtime, outside anything discovery validated. It runs through the
  `shell` tool (World 2), not `skill_exec`, and deserves the same
  suspicion: know what it downloads before approving.
- **Bundled skills** ship inside the binary and are seeded to
  `<data_dir>/skills/` on first use. SKILL.md seeding is write-if-missing
  with a content stamp (a binary update refreshes only skills the user
  never edited); `scripts/` and `references/` use a `.pantheon-bundled.json`
  hash manifest with the same rule — user edits are never overwritten.
  Every non-npx `exec:` command must point at a bundled file, checked at
  seed time.

## Bundled registry

`crates/pantheon-exec/bundled-skills/<name>/` holds `SKILL.md` plus
optional `scripts/` and `references/` trees. `build.rs` generates the
registry from those directories (shared logic in
`build-support/gen.rs`, unit-tested against fixture trees); only UTF-8
text files are embedded, anything else is skipped with a build warning.
**Content workers add skills by adding directories — no code changes.**
