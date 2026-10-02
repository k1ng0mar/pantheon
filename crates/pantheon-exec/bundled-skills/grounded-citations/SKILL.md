---
name: grounded-citations
description: "Use for research answers where factual claims must trace to real sources. Every non-obvious claim gets a numbered citation you actually opened."
origin: bundled
---

# Grounded citations

A research answer without sources is an opinion with formatting. This skill
is the discipline: search, open, read, then write, with every factual claim
anchored to something you can point at.

## Purpose

Produce research where each non-obvious claim carries a citation to a
source the agent actually opened and read.

## Workflow

1. Break the question into checkable claims before searching.
2. Search with `web_search`. Prefer primary sources: the paper, the docs,
   the filing, the official announcement. Press releases about a study
   are not the study.
3. Open the promising sources and read them. A snippet is not reading.
4. Write the answer with numbered citations inline, e.g. `[1]`. One
   citation per claim, placed at the claim.
5. End with a Sources section: number, title, publisher, publication date,
   URL. If you could not verify something, say so in the text instead of
   citing around it.

## Output contract

- Answer body with inline numbered citations on every statistic, date,
  quote, and non-obvious factual claim.
- Sources section with title, publisher, date, URL per number.
- An explicit "could not verify" note wherever a claim resisted
  verification. Absence of a citation means the claim is the agent's own
  reasoning, and the text should read that way.

## Operating rules

1. No citation, no claim. If you cannot source it, cut it or label it.
2. Quotes must be verbatim and short. Paraphrase the rest.
3. Check the date on every source. A 2021 article does not establish a
   2026 fact.
4. One strong source beats three weak ones saying the same thing.
5. Never cite a source you did not open. Search-result snippets are
   leads, not citations.
