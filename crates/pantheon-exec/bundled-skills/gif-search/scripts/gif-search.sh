#!/bin/sh
# Search Tenor v2 for GIFs. Needs TENOR_API_KEY in the environment.
# Usage: gif-search.sh "<query>" [--download <dir>]
set -eu
if [ -z "${TENOR_API_KEY:-}" ]; then
  echo "error: TENOR_API_KEY is not set" >&2; exit 1
fi
QUERY="${1:?"usage: gif-search.sh \"<query>\" [--download <dir>]"}"
DOWNLOAD=""
if [ "${2:-}" = "--download" ]; then DOWNLOAD="${3:?"missing download dir"}"; fi

RESP=$(curl -sf --get "https://tenor.googleapis.com/v2/search" \
  --data-urlencode "q=$QUERY" \
  --data-urlencode "key=$TENOR_API_KEY" \
  --data-urlencode "limit=1" \
  --data-urlencode "contentfilter=high" \
  --data-urlencode "media_filter=gif") || { echo "error: Tenor request failed" >&2; exit 1; }

TITLE=$(printf '%s' "$RESP" | python3 -c 'import json,sys; d=json.load(sys.stdin); r=d["results"][0]; print(r.get("content_description") or r.get("title") or "")' 2>/dev/null || echo "")
PREVIEW=$(printf '%s' "$RESP" | python3 -c 'import json,sys; print(json.load(sys.stdin)["results"][0]["media_formats"]["tinygif"]["url"])' 2>/dev/null || echo "")
MEDIA=$(printf '%s' "$RESP" | python3 -c 'import json,sys; print(json.load(sys.stdin)["results"][0]["media_formats"]["gif"]["url"])' 2>/dev/null || echo "")
if [ -z "$MEDIA" ]; then echo "error: no results for '$QUERY'" >&2; exit 1; fi

echo "{\"query\":\"$QUERY\",\"title\":\"$TITLE\",\"preview_url\":\"$PREVIEW\",\"media_url\":\"$MEDIA\"}"
if [ -n "$DOWNLOAD" ]; then
  mkdir -p "$DOWNLOAD"
  SAFE=$(printf '%s' "$QUERY" | tr ' ' '_' | tr -cd 'A-Za-z0-9_.-')
  OUT="$DOWNLOAD/${SAFE}.gif"
  curl -sfL "$MEDIA" -o "$OUT" || { echo "error: download failed" >&2; exit 1; }
  echo "saved: $OUT"
fi
