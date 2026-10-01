#!/usr/bin/env bash
# tw.sh — thin curl wrapper around the X API v2.
# Auth: set X_BEARER_TOKEN. App-only bearer tokens power search/me
# (read-only). Posting needs a user-context OAuth 2.0 token with the
# tweet.write scope — an app-only bearer will get a 403 on post.
# Requires: curl, python3.
set -euo pipefail

need() { command -v "$1" >/dev/null 2>&1 || { echo "tw.sh: missing required tool: $1" >&2; exit 3; }; }
need curl
need python3
: "${X_BEARER_TOKEN:?tw.sh: set X_BEARER_TOKEN first (see SKILL.md Auth)}"

usage() {
  cat >&2 <<'EOF'
usage:
  tw.sh search "<query>" [max]   # recent posts, supports from: lang: -is:retweet etc.
  tw.sh me                       # verify credential, show connected account
  tw.sh post "<text>"            # publish a tweet (needs user-context token)
EOF
  exit 2
}

api() { curl -fsSL --max-time 40 -H "Authorization: Bearer $X_BEARER_TOKEN" "$@"; }

cmd="${1:-}"
case "$cmd" in
  search)
    q="${2:-}"; max="${3:-10}"
    [ -z "$q" ] && usage
    enc=$(python3 -c 'import sys,urllib.parse; print(urllib.parse.quote(sys.argv[1]))' "$q")
    api "https://api.x.com/2/tweets/search/recent?query=$enc&max_results=$max&tweet.fields=created_at,author_id,public_metrics&expansions=author_id&user.fields=username" \
    | python3 - <<'PY'
import sys, json
try:
    d = json.load(sys.stdin)
except json.JSONDecodeError:
    print("tw.sh: could not parse API response"); sys.exit(1)
if 'errors' in d and not d.get('data'):
    print("tw.sh: API error:", json.dumps(d['errors'])[:300]); sys.exit(1)
users = {u['id']: u.get('username', '?') for u in d.get('includes', {}).get('users', [])}
for t in d.get('data', []):
    u = users.get(t.get('author_id'), '?')
    m = t.get('public_metrics', {})
    print(f"@{u} · {t.get('created_at', '')} · ♥{m.get('like_count', 0)} ↻{m.get('retweet_count', 0)}\n  {t.get('text', '')}\n  id={t.get('id', '')}\n")
if not d.get('data'):
    print("(no results — query may be too narrow, or the tier's rate limit was hit)")
PY
    ;;
  me)
    api "https://api.x.com/2/users/me?user.fields=username,name,public_metrics" \
    | python3 -c 'import sys,json; d=json.load(sys.stdin)["data"]; print(f"@{d[\"username\"]} ({d[\"name\"]}) id={d[\"id\"]}")'
    ;;
  post)
    text="${2:-}"; [ -z "$text" ] && usage
    payload=$(python3 -c 'import sys,json; print(json.dumps({"text": sys.argv[1]}))' "$text")
    curl -fsSL --max-time 40 -X POST -H "Authorization: Bearer $X_BEARER_TOKEN" \
      -H 'Content-Type: application/json' -d "$payload" "https://api.x.com/2/tweets"
    echo
    ;;
  *) usage ;;
esac
