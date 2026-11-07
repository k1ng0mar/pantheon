# CI

Three workflows live in `.github/workflows/`:

| Workflow | Trigger | Jobs |
| --- | --- | --- |
| `ci.yml` | push to `main`/`master`, every PR | `check`, `build`, `cross-platform`, `eval`, `desktop` |
| `release.yml` | tag push | 5-target build matrix, packaging, optional minisign, smoke unpack |
| `pages.yml` | docs changes | documentation site |

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
spawn refusal, and the send/gateway path. Twelve of those cases carry
`skip: "needs binary"`, so without the build step they never ran anywhere by
default.

**`desktop`** — typecheck, `npm run build`, and the desktop unit tests.
`desktop/` is excluded from the Cargo workspace and its own workflow file is
inert in a monorepo (GitHub only reads `.github/workflows` at the repo root),
so this job is the only thing that typechecks the desktop TypeScript.

## Not covered yet

- **No scheduled workflow.** Nothing re-runs the suite against a newer
  `rustc` than the pin, so toolchain drift is discovered when someone next
  bumps the pin. A weekly cron on stable, alert-only, would close that.
- **No coverage or dependency-audit gate.** `cargo llvm-cov` and
  `cargo deny check` appear nowhere in CI; dependabot only opens version
  bumps.
- **`install.sh` / `install.ps1` are never executed.** Release notes call the
  asset names a three-way contract with the installers and `pantheon update`,
  and `eval/tests/release_asset_contract.rs` now pins the naming agreement
  statically, but actually running an installer against a published tag
  remains a manual pre-release step.
