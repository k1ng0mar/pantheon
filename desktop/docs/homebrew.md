# Homebrew

Pantheon has an official tap,
[k1ng0mar/homebrew-pantheon](https://github.com/k1ng0mar/homebrew-pantheon).

```sh
brew install --cask k1ng0mar/pantheon/pantheon   # macOS 12+, Apple silicon or Intel
brew install k1ng0mar/pantheon/pantheon          # Linux x86_64 or aarch64
```

## What it installs

- `Casks/pantheon.rb` installs `Pantheon.app` from the release's
  `Pantheon-macOS-arm64.dmg` or `Pantheon-macOS-x64.dmg`, checked against the
  sha256 in the cask. It sets `auto_updates true` because Pantheon updates itself,
  so `brew upgrade` leaves it alone unless you pass `--greedy`.
  `brew uninstall --zap --cask pantheon` also removes Pantheon's own data under
  `~/Library` (the `ai.pantheon.desktop` folders, caches, preferences and WebKit
  storage). It never touches your projects or `~/Pantheon Chats`.
- `Formula/pantheon.rb` installs `Pantheon-Linux-x86_64.AppImage` or
  `Pantheon-Linux-aarch64.AppImage` as `bin/pantheon`. It is Linux-only
  (`depends_on :linux`); on macOS use the cask. AppImages may need FUSE 2 (`libfuse2`).

The cask has no preflight or postflight scripts and does not remove the quarantine
flag, so macOS shows its usual first-launch prompt for an unsigned app.

## How it stays current

The tap updates itself, so releasing Pantheon needs no change here and no secret in
this repository. Its `.github/workflows/update.yml` runs every 3 hours, on demand
(`workflow_dispatch`) and on a `pantheon-release` repository dispatch. It:

1. reads the latest release tag of `k1ng0mar/pantheon` with `gh api` and stops
   unless it matches `^v\d+\.\d+\.\d+$`;
2. downloads the four macOS and Linux `.sha256` files from that release and checks
   that each holds a 64-character hex digest;
3. rewrites the version and checksums in the cask and formula, and commits and pushes
   only if something changed, using the tap's own `GITHUB_TOKEN`
   (`permissions: contents: write`).

Release values reach the scripts only through environment variables, never inline in
`run:`, and every action is pinned to a commit SHA. `.github/workflows/test.yml` runs
`brew style` and `brew audit --strict` on the cask and formula for every push and pull
request.

The cask and formula name the release assets, so renaming a macOS or Linux asset in
`desktop-release.yml` breaks the tap until it is updated as well.

## Credits

The tap was proposed and first built by Samuel Jędrzejewski
([@238SAMIxD](https://github.com/238SAMIxD)) in issue #85 and PR #90.
