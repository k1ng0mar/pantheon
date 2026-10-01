#!/bin/sh
# Minimal Airtable REST client. Needs AIRTABLE_API_KEY in the environment.
# Usage: airtable.sh <GET|POST|PATCH|DELETE> <base> <table> [json-body] [-- key=value ...]
set -eu
[ -z "${AIRTABLE_API_KEY:-}" ] && { echo "error: AIRTABLE_API_KEY is not set" >&2; exit 1; }
METHOD="${1:?"usage: airtable.sh <GET|POST|PATCH|DELETE> <base> <table> [json] [-- key=value]"}"
BASE="${2:?missing base}"; TABLE="${3:?missing table}"; shift 3
BODY=""; QUERY=""
for arg in "$@"; do
  case "$arg" in
    --) ;;
    --*=*) QUERY="$QUERY&${arg#--}" ;;
    *) BODY="$arg" ;;
  esac
done
URL="https://api.airtable.com/v0/${BASE}/$(python3 -c "import urllib.parse,sys; print(urllib.parse.quote(sys.argv[1]))" "$TABLE")?${QUERY#&}"
if [ -n "$BODY" ]; then
  curl -sf -X "$METHOD" "$URL" \
    -H "Authorization: Bearer $AIRTABLE_API_KEY" -H "Content-Type: application/json" \
    -d "$BODY"
else
  curl -sf -X "$METHOD" "$URL" -H "Authorization: Bearer $AIRTABLE_API_KEY"
fi
echo
