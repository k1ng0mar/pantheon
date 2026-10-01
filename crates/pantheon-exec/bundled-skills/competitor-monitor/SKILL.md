---
name: competitor-monitor
description: "Use when asked to track companies, products, or topics over time and report what changed. Watchlist plus web_search plus a schedule produces digests."
origin: bundled
---

# Competitor monitor

News monitoring is a loop, not a query: define the watchlist once, run the
same searches on a schedule, and report only what is new since the last
digest. This skill is backed by `web_search` and the scheduler. It does not
scrape sites or bypass paywalls; see `blocked-page-recovery` when a source
resists.

## Purpose

Keep a standing watch on named companies, products, or topics and deliver
periodic digests of what actually changed.

## Workflow

1. Define the watchlist. For each entry: name, why it matters, 2-4 search
   queries that find real news about it (site names, product names, exec
   names, not just the company name).
2. Set a cadence with a scheduled job (weekly is the sane default; daily
   only if the topic moves that fast). The job runs the queries and writes
   the digest.
3. Dedupe against the previous digest. Keep a state file
   (e.g. `workspace/monitor/<topic>/seen.md`) listing reported items by
   URL. If it is already in the file, it is not news.
4. Write the digest: date range, per-entry items, each with source,
   publication date, and one line on why it matters. Items with no date
   get flagged as undated, not silently trusted.
5. Anything unclear (rumor, single anonymous source) is labeled as such.

## Output Contract

- Digest header: period covered, watchlist entries checked.
- Per item: headline, source + date, URL, one-line significance.
- "Nothing new" is a valid digest. Do not pad it.

## Operating Rules

1. One digest per scheduled run. Do not accumulate and dump monthly.
2. Never report an item twice. The seen-file is the memory; check it.
3. Dates come from the source, not from when you ran the search.
4. If a whole watchlist entry goes quiet for a month, say so instead of
   stretching thin items to fill the digest.
5. This skill observes. It does not contact companies, post, or take
   positions.
