---
name: x-twitter
description: "Use for reading or posting on X/Twitter: search recent posts, check the account, publish tweets. Requires X API credentials."
origin: bundled
prerequisites: ["X API credentials"]
exec:
  - name: tw-search
    description: "Search recent posts on X (read-only)"
    command: scripts/tw.sh
    args: "search \"<query>\" [max_results]"
    runtime: shell
    side_effects: read
    timeout_secs: 30
  - name: tw-post
    description: "Post a tweet from the connected X account"
    command: scripts/tw.sh
    args: "post \"<text>\""
    runtime: shell
    side_effects: write
    timeout_secs: 30
---

# X / Twitter

> **Requires setup.** This skill needs X API credentials before anything
> works. Without them, stop and say so; do not fake results.

X's API is paid beyond a tiny free allowance, and the tiers are honest
about what you get. Read `references/api-tiers.md` before promising any
volume of work.

## Purpose

Search recent public posts on X, inspect the connected account, and publish
tweets under explicit direction.

## Tooling

All calls go through `scripts/tw.sh`, a thin curl wrapper. It reads the
credential from the `X_BEARER_TOKEN` environment variable and needs nothing
else installed besides `curl` and `python3`.

- `tw.sh search "<query>" [max]` - recent search, newest first. Supports
  X search operators (`from:`, `lang:`, `-is:retweet`, etc.).
- `tw.sh me` - verifies the credential and shows the connected account.
- `tw.sh post "<text>"` - publishes one tweet.

## Auth

Two different tokens, two different powers:

- **App-only bearer token** (from the X developer portal): powers `search`
  and `me`. Read-only. This is the minimum to set up.
- **User-context OAuth 2.0 token** with `tweet.write` scope: required for
  `post`. An app-only bearer posting will fail with 403; that is the API
  telling you the token type is wrong, not a bug in the script.

Set `X_BEARER_TOKEN` to whichever token the task needs. Never print the
token, never write it into a file, never put it in a URL.

## Operating rules

1. `tw.sh me` first on any new session. If it fails, the credential is
   the problem, not the query.
2. Posting is a write action: it needs explicit user approval naming the
   exact text, every time. No paraphrasing their draft into something
   punchier.
3. Respect rate limits. A 429 means back off and stop for this run; it
   does not mean retry harder.
4. Search returns recent posts (last ~7 days on most tiers). It is not an
   archive and not a firehose. Say so when the results are thin.
5. DMs are out of scope for this skill: the script does not implement
   them, and DM access needs a higher-tier approval from X.
