# Credential-error classifier

Provider and channel errors used to report only an HTTP status, which
misleads: a 403 is not always a bad key. The classifier in
`pantheon_providers::credential_error` (mirrored in websearch HTTP
plumbing and in secure-forms verification, because those crates
cannot depend on pantheon-providers) appends an actionable,
secret-free diagnosis to the existing error. Diagnoses are static
text only: they never echo the response body, the request URL, or
any credential. Retryability is unchanged: 429 keeps Retry-After
handling, 5xx stays retryable, other 4xx stays non-retryable.

| Status / body | Diagnosis | Action |
| --- | --- | --- |
| Body contains `error code: 1010` | Blocked at Cloudflare's edge by Browser Integrity Check; the request never reached provider auth | Do NOT replace the key. Send a real User-Agent header (Pantheon now sends `Pantheon/<version>` on provider and search requests) |
| 401 | Key missing, invalid, or revoked | Check the key in the provider account; replace it if revoked or rotated |
| 403 (no 1010 body) | Key accepted but not permitted for this endpoint or model | Check permissions, scope, and plan; do not assume the key is wrong. Provider-specific: some providers also return 403 for an invalid key, so check the body |
| Telegram 404 on a bot path | Token shape is wrong (Telegram tokens are `<digits>:<letters>`) | Check the token format, not just its value |
| Telegram 401 | Bot token wrong or revoked | Check the token in BotFather; replace it if revoked or rotated |
| Telegram 403 | Bot blocked by the user, or not allowed in that chat, or lacking permission there | Check the bot is not blocked and has permission in that chat |

Wired into: provider `send()` errors, websearch `status_error`,
gateway Telegram `send_message`/`get_updates` errors, secure-forms
provider and bot-token verification (save-time and dashboard
verify), and the doctor `providers.model_key` remediation text.
