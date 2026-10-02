#!/usr/bin/env bash
# arxiv.sh - query the public arXiv API (https://export.arxiv.org/api/query).
# No API key required. Requires: curl, python3.
# Be polite: arXiv asks for ~3s between requests; this script sleeps 3s
# before each call.
set -euo pipefail

BASE="https://export.arxiv.org/api/query"

usage() {
  cat >&2 <<'EOF'
usage:
  arxiv.sh search "<query>" [max_results]   # e.g. arxiv.sh search "all:electron" 5
  arxiv.sh fetch <arxiv-id>                  # e.g. arxiv.sh fetch 1706.03762
query prefixes: all: ti: au: abs: cat: (e.g. cat:cs.AI)
EOF
  exit 2
}

need() { command -v "$1" >/dev/null 2>&1 || { echo "arxiv.sh: missing required tool: $1" >&2; exit 3; }; }
need curl
need python3

print_entries() {
  # $1 = "short" (truncated abstracts) or "full"
  # NOTE: the program goes in -c because stdin is the API response;
  # a heredoc would steal stdin from the pipe.
  python3 -c '
import sys, xml.etree.ElementTree as ET, textwrap
mode = sys.argv[1]
ns = {"a": "http://www.w3.org/2005/Atom"}
root = ET.fromstring(sys.stdin.read())
entries = root.findall("a:entry", ns)
if not entries:
    print("no results")
    sys.exit(0)
for e in entries:
    aid = e.find("a:id", ns).text.rsplit("/abs/", 1)[-1]
    title = " ".join((e.find("a:title", ns).text or "").split())
    authors = ", ".join((a.find("a:name", ns).text or "") for a in e.findall("a:author", ns))
    pub = (e.find("a:published", ns).text or "")[:10]
    cats = " ".join(c.get("term", "") for c in e.findall("a:category", ns))
    abstract = " ".join((e.find("a:summary", ns).text or "").split())
    if mode == "short":
        abstract = textwrap.shorten(abstract, width=600, placeholder=" [...]")
    pdf = next((l.get("href", "") for l in e.findall("a:link", ns) if l.get("title") == "pdf"), "")
    print(f"[{aid}] {title}\n  authors:   {authors}\n  published: {pub}\n  categories:{cats}\n  abstract:  {abstract}\n  pdf:       {pdf}\n")
' "$1"
}

cmd="${1:-}"
case "$cmd" in
  search)
    q="${2:-}"; max="${3:-5}"
    [ -z "$q" ] && usage
    enc=$(python3 -c 'import sys,urllib.parse; print(urllib.parse.quote(sys.argv[1]))' "$q")
    sleep 3
    curl -fsSL --max-time 50 \
      "$BASE?search_query=$enc&start=0&max_results=$max&sortBy=submittedDate&sortOrder=descending" \
      | print_entries short
    ;;
  fetch)
    aid="${2:-}"; [ -z "$aid" ] && usage
    aid="${aid#https://arxiv.org/abs/}"; aid="${aid#http://arxiv.org/abs/}"
    aid="${aid%%v[0-9]*}"  # strip version suffix for the id_list lookup
    enc=$(python3 -c 'import sys,urllib.parse; print(urllib.parse.quote(sys.argv[1]))' "$aid")
    sleep 3
    curl -fsSL --max-time 50 "$BASE?id_list=$enc" | print_entries full
    ;;
  *) usage ;;
esac
