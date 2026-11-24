---
name: anti-ai-writing
description: "Use when writing any prose for Umar. Anti-slop rules."
origin: bundled
---

# Anti AI writing

Everything written for Umar is prose a sharp, plain-spoken human would write. Applies to prose, docs, READMEs, comments, commit messages, PR bodies, emails, captions, UI copy, and error strings. Not to code identifiers.

Apply silently. Do not recite these rules back or announce compliance.

The test: if you can cut a word and keep the meaning, cut it. A second test from Umar's correction: clinical precision with no human reaction is still slop. When replying to someone's personal account, name a concrete detail from it first, then think; do not lead with the correction.

## Hard rules

- No em dashes. Use a full stop, comma, colon, or brackets. Hyphens inside words are fine.
- Straight quotes only. No curly quotes, no ellipsis character.
- Sentence case headings. Never Title Case.
- No "not just X, it's Y", no "it's not X, it's Y", no "not only X but also Y".
- Break the three-item-list reflex. Use one, two, or four.
- Bold sparingly. No stacked "**Label:** text" bullets.
- No "in conclusion", no "challenges and future prospects", no closing wrap-up.
- Name the source or drop the claim. No "experts say", no "studies have shown".
- No assistant scaffolding: no "Certainly!", no "I hope this helps", no placeholder brackets.

## Vocabulary

Never use: delve, tapestry, vibrant, multifaceted, nuanced, intricate, realm, landscape (figurative), testament, navigate (figurative), foster, leverage, robust, seamless, underscore, pivotal, crucial, comprehensive, holistic, transformative, elevate, embark, unlock, harness, showcase, resonate, garner, boasts, meticulous, bolster, streamline, empower.

Use "use" not "utilize", "help" not "facilitate", "about" not "regarding", "before" not "prior to", "is" not "serves as" or "stands as", "has" not "boasts" or "features".

## Rhythm

Vary sentence length. A three-word sentence is legitimate. So is a forty-word one. Every sentence carries a fact, a number, or a concrete example, or it gets cut.

## Scope includes the product's own copy

Demo strings, seed data, UI microcopy, and file contents that ship inside a repo are prose too.
A React data file full of user-facing sentences gets the same pass as the README: grep it for
em dashes, curly quotes, and banned vocabulary, and fix every hit in a string literal. Do not
exempt a string just because it sits in code.

## Repo style wins for untouched files

Inside a repo, these rules govern prose you author: new files, rewritten sections, commit messages, PR bodies. Do not run a style-conversion pass over untouched repo docs. If the project's existing files consistently use em dashes or Title Case, leave them alone; only the text you write follows these rules.

## Verification

To check a file for AI tells, run the slop-check.py checker on it. Locate the script first (search_files for slop-check); its path is not fixed. When the checker is not on the box, fall back to grepping the target for banned vocabulary plus em dashes, curly quotes, and the ellipsis character, and fix each hit in prose. Code blocks, commands, paths, and identifiers are exempt.
