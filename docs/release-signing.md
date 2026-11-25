# Release signing (minisign)

`pantheon update` authenticates release archives with a minisign
signature checked offline against a public key pinned in
`crates/pantheon-release-verify/src/lib.rs` (`PINNED_PUBLIC_KEY`).
The same-origin `.sha256` file is a corruption check only: it proves
the bytes match the publisher's checksum file, and that file comes
from the same origin as the binary, so it can never prove who
published the binary. No archive installs without a valid signature
from the pinned key, and there is no bypass flag.

## Current status: provisioned

A single release keypair exists. The public half is pinned in
`PINNED_PUBLIC_KEY`; the private half was generated with `minisign -G`,
uploaded to the repo secret `MINISIGN_SECRET_KEY`, and destroyed on the
generating machine. No copy of the secret key lives in this repo, on
the build host, or in the pinned source.

Releases built before provisioning are unsigned and are still refused by
`pantheon update`. Those must be installed by hand. Every release built
after provisioning carries a `.minisig` and verifies against the pinned
key. Signing is guarded in CI: if the secret is ever missing, the release
ships unsigned with a loud warning rather than failing the build.

## Provisioning (one-time, operator action)

1. Generate a keypair on a trusted machine (never in CI):
   `minisign -G` (accept the default paths, or `-p release.pub -s release.key`).
2. Pin the public key: copy the base64 key line (the second line of
   `release.pub`, starting `RW...`) into `PINNED_PUBLIC_KEY` in
   `crates/pantheon-release-verify/src/lib.rs` and ship it in a
   release. Note the chicken-and-egg: the key ships inside a binary
   the user already trusts (a release installed by hand, or built
   from source); it authenticates every release after that one.
3. Store the private key: copy the ENTIRE secret-key file content
   (both lines: the `untrusted comment` line and the base64 line)
   into a GitHub repository secret named `MINISIGN_SECRET_KEY`.
   Never commit the secret key, never print it in logs.

## What CI does

The `release` workflow signs each asset with `minisign -S` when
`MINISIGN_SECRET_KEY` exists, publishing `<asset>.minisig`
alongside `<asset>.sha256`. When the secret is absent the signing
step skips (it does not fail the release) and the release ships
unsigned - which `pantheon update` then refuses to install. The
release page says so explicitly ("UNSIGNED" prepended to the release
body, plus a workflow warning), and `install.sh` prints the same
limitation, so an unsigned release is visible rather than silent.
The `.minisig` file is a minisign signature over the archive bytes,
fetched by the updater from `{asset_url}.minisig`.

## Verifying an install by hand (installer)

`install.sh` verifies the published `.sha256` by default and, when you pin a
key, also verifies the minisign signature:

```sh
PANTHEON_MINISIGN_PUBKEY='RW...' \
  curl -fsSL https://raw.githubusercontent.com/k1ng0mar/pantheon/master/install.sh | bash
```

`PANTHEON_MINISIGN_PUBKEY_FILE=release.pub` works too (the `RW...` line is
read from it). With a key set, the installer fetches `<asset>.minisig`, runs
`minisign -V` against your key, and refuses to install on a mismatch. It
refuses an unsigned asset unless `PANTHEON_ALLOW_UNVERIFIED=1` is set, so the
opt-out is visible in the command that used it. Without a key the installer
says exactly what it proved (integrity only) rather than implying
authentication.

## Supply-chain inventory

Every CLI release attaches an SPDX JSON SBOM per asset
(`<asset>.sbom.spdx.json`, generated with a pinned syft), matching what the
desktop build already publishes.

## Verifying a release by hand

```
minisign -V -p release.pub -m pantheon-vX.Y.Z-linux-x86_64.tar.gz
```

## Rotating the key

Generate a new keypair, pin the new public key, and provision the
new secret. Releases signed with the old key stop verifying once
the new key ships - plan rotation around a release boundary, and
expect users on the pre-rotation binary to install one release by
hand (the same path as the initial provisioning).
