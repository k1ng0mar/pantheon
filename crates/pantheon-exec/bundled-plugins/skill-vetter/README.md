# skill-vetter

Static security vetting for community plugin/skill installs - the companion
to `/plugins` import-by-URL. Point it at a local directory (or an `http(s)`
URL to a file/archive) and get a risk report with a `pass` / `review` /
`block` verdict.

Opt-in: disabled by default.

## What it checks

All detection logic is static pattern analysis over file contents
(standard library only):

| Check id | Severity | What it flags |
|---|---|---|
| `webhook-exfil` | high | Traffic to relay/exfiltration hosts (Discord/Slack webhooks, webhook.site, ngrok, oastify, ...) |
| `cred-file` | high | Reads of `~/.ssh`, `~/.aws`, `~/.gnupg`, private keys, `.netrc` |
| `cred-env` | high | Environment reads of secret-named vars (`*_KEY`, `*_TOKEN`, `*_SECRET`, ...) |
| `remote-code-exec` | high | `exec`/`eval` of fetched code, `curl ... \| sh`, PowerShell `IEX` on URLs |
| `priv-escalation` | high | `sudo`, `pkexec`, `setuid`/`setgid`, setuid bits |
| `net-listener` | high | `socket.listen`/`bind`, `http.server`, `app.run()`, `0.0.0.0` binds |
| `unknown-host` | medium | HTTP(S) contact with hosts outside a well-known allowlist |
| `cred-env` / `cred-env-bulk` | medium | Plain or bulk environment reads |
| `obfuscated-blob` | medium (high when paired with decode + dynamic exec) | Long base64/hex blobs |
| `dynamic-exec` | medium | Bare `eval`/`exec`/`compile` |

Verdict: any `high` → `block`; any `medium` (no high) → `review`;
otherwise → `pass`.

## Usage

`vet_target(path_or_url)` - one argument:

- Absolute local path to a plugin/skill directory, single file, or
  `.zip` / `.tar.gz` archive.
- `http(s)` URL to a file or archive. Redirects are re-validated to stay
  on `http(s)`; downloads are capped at 10 MB; archives are extracted
  with path-traversal sanitization. Git (`.git`) URLs are refused
  clone locally first, then vet the directory.

Only `http`/`https` URLs are accepted; anything else (including
`file://`) is rejected. URLs with embedded credentials are refused.

Scanning limits: 1000 files, 2 MB per file, 50 MB total; binaries,
`.git/`, `node_modules/`, and virtualenvs are skipped.

## Honest limits

**This is defense-in-depth guidance, not an enforcement gate.** Static
patterns catch known-bad shapes; obfuscated or novel malware can evade
them, and a `pass` verdict is not a safety proof. Review flagged code by
hand before installing anything from a source you do not trust. The
report says this on every run (see `notes` in the result).
