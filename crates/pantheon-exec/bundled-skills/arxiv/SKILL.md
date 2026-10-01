---
name: arxiv
description: "Use when you need to find or retrieve academic papers from arXiv. Searches by keyword, subject, or author; fetches metadata, abstracts, and PDF links."
origin: bundled
exec:
  - name: arxiv-search
    description: "Search arXiv and print id, title, authors, date, abstract, PDF link"
    command: scripts/arxiv.sh
    args: "search \"<query>\" [max_results]"
    runtime: shell
    side_effects: read
    timeout_secs: 60
  - name: arxiv-fetch
    description: "Fetch one paper's full metadata and abstract by arXiv id"
    command: scripts/arxiv.sh
    args: "fetch <arxiv-id>"
    runtime: shell
    side_effects: read
    timeout_secs: 60
---

# arXiv

The arXiv API is public HTTP and needs no key. The helper owns the query
mechanics; this skill owns how to use the results honestly.

## Purpose

Find and retrieve papers from arXiv for research, literature review, or
checking what exists on a topic before building.

## Workflow

1. Search with the helper: `arxiv-search` with a query. Prefixes narrow the
   search: `all:`, `ti:` (title), `au:` (author), `abs:` (abstract),
   `cat:` (subject, e.g. `cat:cs.AI`).
2. Scan the returned ids, titles, dates, and abstracts. Pick the ones that
   actually match, not the ones with the catchiest titles.
3. Fetch full metadata for shortlisted papers with `arxiv-fetch`.
4. If you need the full text, download the PDF link the helper prints and
   read it with the file tools. The helper does not do this for you.
5. Cite papers as `arXiv:YYMM.NNNNN` with title and authors, never from
   memory of what the paper "probably" says.

## Output Contract

- Search results: arXiv id, title, authors, publication date, short
  abstract, PDF link.
- Retrieved paper: full metadata plus complete abstract.
- Anything you claim about a paper's contents must come from the abstract
  or the PDF you actually read.

## Operating Rules

1. Use the helper for every query. Do not hand-build API URLs in chat.
2. The helper enforces arXiv's politeness rule (~3s between requests).
   Do not loop it rapidly or wrap it in a retry storm.
3. Default `max_results` is 5. Raise it deliberately, not reflexively.
4. arXiv ids are not DOIs and not peer review. Say "arXiv preprint"
   when that is what it is.
5. If a query returns nothing, broaden the terms before concluding the
   paper does not exist.
