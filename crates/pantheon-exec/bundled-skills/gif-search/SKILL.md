---
name: gif-search
description: "Search Tenor for GIFs and download matching results. Trigger when the user wants a GIF for a message, a reaction GIF, or GIF URLs for a post."
origin: bundled
prerequisites: ["Tenor API key"]
exec:
  - name: gif-search
    description: "Search Tenor and download the top GIF result"
    command: scripts/gif-search.sh
    args: "<query> [--download <dir>]"
    runtime: shell
    side_effects: read
    timeout_secs: 60
---

# GIF search

> Requires setup: a Tenor API key in the `TENOR_API_KEY` environment
> variable. Get one free from developers.google.com/tenor. Without it, this
> skill cannot search anything.

Thin wrapper over the Tenor v2 search API: find a GIF by query, return
preview and media URLs, optionally download the file.

## Tooling

`scripts/gif-search.sh "<query>"` — prints the top result as JSON
(`title`, `preview_url`, `media_url`). With `--download <dir>`, also saves
the GIF and prints the file path.

```sh
scripts/gif-search.sh "shocked cat" --download /tmp/gifs
```

The helper is plain curl with no dependencies beyond a POSIX shell. It fails
loudly on a bad key or a network error.

## Operating rules

- Keep queries specific ("facepalm", not "funny"). Broad queries return
  whatever Tenor's trending graph feels like that day.
- Default content filter: `high` unless the user asks otherwise.
- Download only when the GIF will actually be used (message, post,
  gateway media). Searching to browse is free; downloading to hoard is not.
- Respect the message context: a professional channel is not the place for
  the first result of a one-word query you have not previewed.
