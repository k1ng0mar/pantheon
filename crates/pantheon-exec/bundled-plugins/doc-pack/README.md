# doc-pack

Create and read Microsoft Office documents from the agent: Word (`.docx`),
Excel (`.xlsx`), and PowerPoint (`.pptx`). Real implementation on top of
`python-docx`, `openpyxl`, and `python-pptx` - Pantheon's first
document-production capability.

Opt-in: disabled by default. Enable with `[plugins.doc-pack] enabled = true`
(or `pantheon plugins enable doc-pack`, depending on the surface you use).

## Requirements

Python 3 and the three document libraries:

```
pip install python-docx openpyxl python-pptx
```

The runner checks at startup and prints a clear notice to stderr for any
missing library. A tool whose library is missing fails with
`DOC_PACK_MISSING_DEP` naming the exact pip package - the other tools keep
working, so a partial install is usable.

No network access, no credentials, no state: every call is a pure
file in / file out operation.

## Tools

All paths must be absolute. Parent directories are created as needed.

### docx_create(path, blocks, title?, author?)

Build a Word document from ordered blocks. Block types:

- `{"type": "heading", "text": "...", "level": 1}` - levels 0-9
- `{"type": "paragraph", "text": "...", "style": "Normal"}` - any named style
- `{"type": "bullet", "text": "...", "level": 0}` - levels 0-8 indent
- `{"type": "number", "text": "...", "level": 0}` - numbered list items
- `{"type": "table", "rows": [[...], ...], "header": true, "style": "Table Grid"}`
- `{"type": "page_break"}`

Prefer named styles over ad-hoc formatting so the document stays editable;
table geometry uses explicit widths via the `Table Grid` style (adapted
guidance from the MIT-0 `word-docx` skill by ivangdavila).

### docx_read_text(path)

Returns paragraphs with their style names, tables as row/cell arrays, and a
plain-text concatenation.

### xlsx_create(path, sheets)

`sheets` is a list of `{"name", "rows", "header"?, "col_widths"?, "freeze"?}`.
Cell strings starting with `=` become formulas. `header: true` bolds the
first row with a fill; columns auto-fit unless `col_widths` is given;
`freeze: "A2"` freezes panes.

### xlsx_read(path, sheet?)

Returns sheet names and rows. Formulas are returned as `"=..."` strings
(the stored formula, not a cached value).

### pptx_create(path, slides, title?)

`slides` is a list of `{"layout", "title", "bullets", "notes"?}`. Layouts:
`title_slide`, `title_content`, `section_header`, `two_content`,
`comparison`, `title_only`, `blank`. Bullets are strings or
`{"text", "level"}` with levels 0-8.

### pptx_read_text(path)

Per-slide title, text shapes, and speaker notes.

## Error codes

| Code | Meaning |
|---|---|
| `DOC_PACK_MISSING_DEP` | A document library is not installed; cause names the pip package |
| `DOC_PACK_BAD_ARGS` | Malformed tool arguments |
| `DOC_PACK_BAD_PATH` | Path is not absolute |
| `DOC_PACK_IO` | File not found / unreadable |
| `DOC_PACK_UNKNOWN_TOOL` | Unknown tool name |
| `DOC_PACK_INTERNAL` | Unexpected failure (bug - please report) |
