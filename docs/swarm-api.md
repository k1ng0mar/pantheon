# Swarm API

Multi-agent fan-out for Pantheon: one task, N subagents working in
parallel, a judge pass over their transcripts, and bounded retries
with judge feedback. The dashboard exposes the whole lifecycle over
HTTP; the runtime crate owns the orchestration logic. The mobile app's
Swarm page is built against this contract.

## Endpoints

All paths are under the dashboard's API root (`/api`).

### `POST /api/swarm` - create a swarm

Body (JSON):

```json
{ "task": "Research three SSO providers and compare pricing",
  "mode": "count",
  "subagent_count": 4,
  "judge": true }
```

or, to fan out to specific agent profiles:

```json
{ "task": "Draft the launch post",
  "mode": "profiles",
  "profiles": ["nyx", "atlas"],
  "judge": true }
```

- `task` (required, non-empty): the shared task every agent receives.
- `mode` (optional, default `"count"`): `"count"` spawns N generic
  subagents; `"profiles"` spawns one agent per named profile. Any
  other value is a `400`.
- `subagent_count` (optional, default 4): how many subagents to spawn
  in `"count"` mode. Integer in 1..=8. (The legacy `agents` key is
  accepted as an alias.)
- `profiles` (required in `"profiles"` mode): 1..=8 profile names.
  Every name must be declared in the config's `[agents]` table - an
  unknown profile is a `400`, never a silent fallback.
- `judge` (optional, default true): whether a judge model reviews the
  combined transcripts when the round settles. Requires a configured
  `[judge]` aux model (see Config below); `judge: true` with no judge
  transport is a `400` (`SWARM_VALIDATION`).

Response: `201`

```json
{ "swarm_id": "sw_1",
  "id": "sw_1",
  "agents": ["subagent-1", "subagent-2"],
  "status_view": { "...full first status view..." } }
```

`swarm_id` is the documented key; `id` is an alias kept for
compatibility. `status_view` is the same shape as
`GET /api/swarm/status` (below) - it carries run ids, per-agent
profiles, and live summaries for clients that want them without a
second round-trip.

Errors: `400` (`SWARM_VALIDATION`) for an empty task, a bad `mode`,
an out-of-range or non-integer `subagent_count`, an empty/too-long or
unknown `profiles` list, or `judge: true` with no judge transport.

### `GET /api/swarm/status?swarm=<swarm_id>` - poll the swarm

Refreshes every agent's state from the worker, runs the judge when the
round settles, then returns the status view:

```json
{ "swarm_id": "sw_1",
  "id": "sw_1",
  "task": "Research three SSO providers and compare pricing",
  "status": "running",
  "round": 1,
  "judge": true,
  "agents": [
    { "name": "subagent-1", "profile": "default", "run_id": "r_0",
      "status": "working", "round": 1, "summary": null },
    { "name": "nyx", "profile": "nyx", "run_id": "r_1",
      "status": "done", "round": 1, "summary": "result: compared SSO pricing" }
  ],
  "verdict": null }
```

- `swarm_id` is the documented key; `id` is an alias.
- The query param accepts `swarm` or `id` (`swarm` is documented).
- `task` echoes the original task so clients don't have to keep it.
- `summary` is a live one-line pulse per agent: the last non-empty
  line of its transcript, truncated to 200 chars, or `null` when there
  is nothing to show yet (agent still `waiting`, or the worker has no
  transcript).

`verdict` is `{"done": true|false, "notes": "..."}` once the judge has
run, otherwise `null`. Poll this endpoint until `status` leaves
`running` - the refresh-on-read means no separate settle step is needed.

Errors: `400` when the id param is missing; `404`
(`SWARM_NOT_FOUND`) for an unknown swarm id.

### `GET /api/swarm/transcript?swarm=<swarm_id>[&agent=<name>]` - transcripts

Without `agent`, the combined transcript:

```json
{ "swarm_id": "sw_1",
  "id": "sw_1",
  "transcript": "=== subagent-1 (r_0) round 1 [done] ===\n...\n\n=== judge verdict: not done ===\n..." }
```

One section per agent per round, headed by name, run id, round, and
status; the latest judge verdict is appended when one exists. (A retry
clears the verdict into the next round's task, so a mid-round-2
transcript shows no verdict section.)

With `&agent=<name>`, one agent's live transcript:

```json
{ "swarm_id": "sw_1",
  "id": "sw_1",
  "agent": "subagent-1",
  "profile": "default",
  "status": "working",
  "transcript": "task: ...\nassistant: ..." }
```

Errors: `400` when the id param is missing; `404`
(`SWARM_NOT_FOUND`) for an unknown swarm id or (with `agent`) an
unknown agent name in the current round.

### `POST /api/swarm/<id>/retry` - start a new round

Only incomplete swarms can be retried, at most 3 rounds total, and only
when a judge verdict exists with `done: false`. The new round's agents
receive the original task plus the judge's feedback appended
(`Judge feedback from round N (address every point): ...`), so they
address what was missing instead of repeating round 1. Profiles-mode
swarms keep their profiles across rounds.

Response: `200`

```json
{ "swarm_id": "sw_1", "id": "sw_1", "round": 2 }
```

Errors: `404` (`SWARM_NOT_FOUND`); `400` (`SWARM_RETRY_REFUSED`) when
the swarm isn't incomplete, rounds are exhausted, no verdict exists
(judge was disabled), or the verdict is already done.

## Status enums

Swarm `status` (lowercase in JSON):

| value        | meaning                                              |
|--------------|------------------------------------------------------|
| `running`    | at least one agent still working                     |
| `judging`    | transitional - the judge is being consulted          |
| `complete`   | all agents done (and the judge, if enabled, said done)|
| `incomplete` | settled but not done: an agent failed, or the judge said not done |

Agent `status`:

| value     | meaning                                              |
|-----------|------------------------------------------------------|
| `waiting` | admitted, not yet spawned (only visible at creation) |
| `working` | turn in flight                                       |
| `done`    | run completed                                        |
| `failed`  | run failed/canceled, the child died without settling, or the worker lost the run |

## Error codes

| code                  | HTTP | when                                                        |
|-----------------------|------|-------------------------------------------------------------|
| `SWARM_VALIDATION`    | 400  | empty task; bad `mode`; `subagent_count` outside 1..=8; bad/unknown `profiles`; `judge: true` with no judge transport |
| `SWARM_RETRY_REFUSED` | 400  | retry on a non-incomplete swarm, past round 3, no verdict, or verdict already done |
| `SWARM_NOT_FOUND`     | 404  | unknown swarm id on status / transcript / retry; unknown agent on per-agent transcript |
| `SWARM_WORKER`        | 500  | the agent worker failed (ledger I/O, spawn failure)         |

## Config

### `[swarm]` section (config.toml)

Read by `pantheon-dashboard`'s config handler and surfaced in the
dashboard config editor. Keys (all with defaults; the section itself
is optional):

- `max_subagents` (default 4) - per-agent spawn cap: how many
  subagents one agent may spawn before further spawns are refused
  (`SWARM_PER_AGENT_CAP`). Also settable per profile via
  `AgentProfile::swarm_max_subagents`.
- `max_depth` (default 2) - maximum delegation depth (primary = 0).
- `max_concurrent` (default 4) - maximum live subagents across the swarm.
- `allow_child_spawn` (default true) - when false, a child agent
  (depth ≥ 1) that attempts to delegate gets a structured refusal
  (`SWARM_CHILD_SPAWN_DENIED`) instead of a grandchild.

These govern the agent loop's delegate path (in-process subagents via
`SubagentRegistry`), not the dashboard's HTTP swarm fan-out - the HTTP
swarm's agent count is validated 1..=8 at the API layer.

### `[judge]` aux section (config.toml)

Standard aux section (`provider`, `model`, `api_key_env`, `timeout`).
`provider: "default"` (or empty) inherits `[model]`'s provider. The
dashboard builds a one-shot `JudgeTransport` from it at startup
(`swarm::judge_transport_for`); swarms created with `judge: true` and
no resolvable transport are rejected with `SWARM_VALIDATION`.

The judge contract: the model must answer with a `VERDICT: done` or
`VERDICT: not done` line plus free-form notes. Parsing is fail-closed
an unparseable answer or a transport error counts as *not done*, never
as approval.

## How agents run (production)

Each swarm agent is a real Pantheon turn, not a simulation:

1. A fresh run id is admitted to the ledger and titled
   `swarm <swarm_id> · <agent_name>` (profiles-mode agents append the
   profile: `swarm <id> · <name> (<profile>)`).
2. A detached `pantheon run --taskID <run_id> --say <task>
   [--agent <profile>] --deliver session` child is spawned in its own
   process group (the same turn path as `POST /api/runs`). Profiles-mode
   agents pass `--agent <profile>`: the child resolves the profile from
   config `[agents]` and runs the turn as that profile - its SOUL.md /
   USER.md / AGENTS.md ride the session prompt. Unknown profiles fail
   closed at the child with a clear error. Count mode's `"default"`
   sentinel only passes `--agent` when a profile is actually declared
   under that name, so undeclared installs keep the anonymous behavior.
3. Status polling reads the ledger run status; a dead child PID with a
   non-terminal ledger status maps to `failed` so the swarm judges on
   what's there instead of polling forever.
4. Transcripts are folded from the run's ledger events
   (`user:` / `assistant:` / `tool:` lines).

In tests, `SwarmWorker` is replaced by `ScriptedWorker` (deterministic
complete/fail/settling) and the judge by a canned transport, so no
test launches a subprocess.
