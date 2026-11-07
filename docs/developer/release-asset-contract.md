# Release asset naming contract

`install.sh`, `install.ps1`, and `pantheon update` all construct a download
URL from the same three inputs — release tag, OS id, arch id — and the
release workflow uploads assets under names built from its own matrix.
Nothing in the repository checked that those two sides agree.

`release.yml` says it plainly: *"Rename an asset here and the installer
breaks on every machine."* Until this test existed, that only came true
silently, on a user's first install.

## What is checked

`eval/tests/release_asset_contract.rs` parses the two sides as data and
cross-checks them:

| Side | Source of truth | Extracted |
| --- | --- | --- |
| Installers | `install.sh`, `install.ps1` | the `pantheon-${VERSION}-${OS_ID}-${ARCH_ID}.<ext>` template, and the `case` arms that map `uname` to `OS_ID` / `ARCH_ID` |
| Release | `.github/workflows/release.yml` | every `os` / `arch` pair in the build matrix |

Failure modes it now catches:

- an asset renamed in `release.yml` without updating the installers;
- a new platform added to the matrix but never reachable from an installer;
- an installer that builds a name (`linux` vs `Linux`, `amd64` vs `x86_64`)
  the release workflow never publishes;
- an archive extension changed on one side only (`.tar.gz` vs `.zip`).

## Running it

```sh
cargo test -p pantheon-eval release_asset_contract
```

It reads the repository as text and needs no build of the binary, so it is
cheap enough for the default CI `test` job.

## What it deliberately does not cover

It does not download a release or execute either installer. Actually running
`install.sh` against a published tag needs network access and a real release,
so that stays a manual pre-release step (see the checklist in
`docs/release-signing.md`).
