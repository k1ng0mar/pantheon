# CI

Five workflows live in `.github/workflows/`:

| Workflow | Trigger | Jobs |
| --- | --- | --- |
| `ci.yml` | push to `main`/`master`, every PR | `check`, `build`, `cross-platform`, `eval`, `desktop`, `desktop-rust`, `desktop-ui`, `install-smoke` |
| `release.yml` | `v*` tag push | 5-target build matrix, packaging, optional minisign, smoke unpack |
| `desktop-release.yml` | `desktop-v*` tag push | `prepare`, `build`, `sbom`, `publish`, `npm` |
| `pages.yml` | docs changes | documentation site |
| `weekly.yml` | weekly cron | fmt, clippy, test, audit |

## The jobs

**`check`** — `cargo fmt --all --check`, then `cargo clippy --workspace
--all-targets --all-features` under `RUSTFLAGS=-D warnings`, then
`cargo test --workspace`. Runs on the pinned toolchain from
`rust-toolchain.toml`; there is no toolchain input in the action, so local
and CI fmt/clippy resolution cannot drift.

**`build`** — release build plus a smoke run of the artifact
(`pantheon --version`, `pantheon --help`). A green test suite must not be
able to ship a binary that does not start.

**`cross-platform`** — `cargo test -p pantheon-helper -p pantheon-exec
-p pantheon-api` on `windows-latest` and `macos-latest`.

This job is **`continue-on-error: true` on purpose**. Those crates carry the
bulk of the platform branching (`tasklist`/`sc` service enumeration in the
helper, sandbox path confinement, dotenv permissions, oauth flows,
self-update), and before this job existed those paths were compiled by the
release matrix but never executed anywhere but Linux. Treating the first red
run as a merge blocker would freeze every PR on pre-existing breakage that
predates the job.

**Flip `continue-on-error` to `false` once this job has been green for a
full week.** Until then it reports failures without blocking merges, so
check its result when touching platform code.

**`eval`** — builds the CLI (debug) and runs `python3 eval/run.py` with
`PANTHEON_BIN=target/debug/pantheon`. The harness is a stdlib-only
regression suite over the real binary: 22 cases covering the run lifecycle,
audit export, memory round-trip, plugin discovery, setup/config, swarm
spawn refusal, and the send/gateway path. Fifteen of those cases carry
`skip: "needs binary"`, so without the build step they never ran anywhere by
default.

**`desktop`** — typecheck, `npm run build`, and the desktop unit tests.
`desktop/` is excluded from the Cargo workspace and its own workflow file is
inert in a monorepo (GitHub only reads `.github/workflows` at the repo root),
so this job is the only thing that typechecks the desktop TypeScript.

**`desktop-rust`** — `cargo test --lib` and a strict `clippy` over
`desktop/src-tauri`. It sets `RUSTFLAGS: ""` because the workflow-level
`-D warnings` would reach Tauri's own dependencies and fail the job for
warnings this crate does not own.

**`desktop-ui`** — the reliable subset of the desktop Playwright suite
(`npx playwright test --config playwright.ci.config.ts`). The job installs
Playwright's own Chromium with `--with-deps`, because the suite's macOS Chrome
path is host-bound and a runner has neither Chrome nor its OS libraries. It
sets `PW_WORKERS` explicitly: the config default of 4 oversubscribes a small
runner. The Playwright `webServer` starts Vite itself, so no frontend build
step is needed.

The CI config also raises the expect timeout from 5s to 15s, and the job pins
`PW_WORKERS: 1`. The 5s default is calibrated for an idle dev box; on a shared
runner it turns scheduler latency into spurious `toBeVisible` failures. At two
workers the subset fails 29 of 330 tests at 5s and 9 of 330 at 15s, almost all
in `concurrency`, `turn-changes`, and `jev-live`. Every one of those passes at
one worker, so the job uses one. It is slower (about 50 minutes for 330 tests on
a 4-vCPU box) but it is the only green setting.

**`install-smoke`** — runs `install.sh` against the latest release and checks
the installed binary answers `--version` and `--help`. Release notes call the
asset names a three-way contract with the installers and `pantheon update`,
so a green build must not ship an installer that cannot install.

## Playwright UI coverage

`desktop/playwright.config.ts` is the full local suite: 74 spec files, 829
tests. `desktop/playwright.ci.config.ts` imports it and subtracts the specs in
its `testIgnore`. CI runs 42 files, 330 tests.

The 32 excluded specs are not deleted. They stay in the local suite so the work
to repair them stays visible, and each one is listed in `testIgnore` with the
cause. Three causes:

- **stale** — the spec asserts a provider list, a settings shape, or UI text
  from before the rebrand. It fails deterministically against the current app,
  so it reports red for behaviour that is correct. `provider-settings`,
  `settings`, `agent-usage`, `agent-updates`, `opencode-models`, `architect`,
  `ui`, `attachments`, `bots`, `colors`, `compact-menus`,
  `conversation-import`, `design-canvas`, `generative-ui`,
  `message-actions`, `remote`, `simulator`, `skills-library`,
  `slash-commands`, `steering`, `team-profiles`, `wallpaper-settings`.
- **hangs** — the spec waits on a control that never becomes actionable, so it
  burns the per-test timeout. Raising the timeout does not help: `grok` still
  fails on `locator.click` at 60s. `agent-teams`, `browser`, `cloud-agents`,
  `external-links`, `grok`, `layout-preferences`, `sketch`, `streaming`, `vm`.
- **flaky** — the spec depends on wall-clock timing or a module URL that moves
  between runs. Two runs of the same file gave different failures. `overlay`.

A spec is repaired by updating its expectations to the app as it is now, then
removing it from `testIgnore`. Deleting a spec to make CI green is not a
repair, and neither is raising the per-test timeout to hide a hang.

## Not covered yet

- **No coverage gate.** `cargo llvm-cov` appears nowhere in CI.
- **No dependency-audit gate on PRs.** `cargo deny check` appears nowhere;
  dependabot only opens version bumps. `weekly.yml` runs `cargo audit`, but
  only against the pinned toolchain and only on a schedule.
- **`install.ps1` is never executed.** `install-smoke` covers `install.sh`
  against the latest release; the PowerShell installer has no equivalent.
  `eval/tests/release_asset_contract.rs` pins the asset-naming agreement
  statically, but running the Windows installer remains a manual step.
