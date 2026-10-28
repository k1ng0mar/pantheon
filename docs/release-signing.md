# Release signing (minisign)

`pantheon update` authenticates release archives with a minisign
signature checked offline against a public key pinned in
`crates/pantheon-release-verify/src/lib.rs` (`PINNED_PUBLIC_KEY`).
The same-origin `.sha256` file is a corruption check only: it proves
the bytes match the publisher's checksum file, and that file comes
from the same origin as the binary, so it can never prove who
published the binary. No archive installs without a valid signature
from the pinned key, and there is no bypass flag.

## Current status: key NOT provisioned

No release keypair exists yet, so `PINNED_PUBLIC_KEY` is `None` and
verification fails closed with a clear message: `pantheon update`
refuses to install, and the operator installs the release by hand
until provisioning lands. That is the intended state, not a bug to
work around with a placeholder key - a placeholder would look like
authentication while verifying against a key nobody controls.

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
`.minisig` file is a minisign signature over the archive bytes,
fetched by the updater from `{asset_url}.minisig`.

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
