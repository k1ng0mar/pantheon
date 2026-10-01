---
name: airtable
description: "Read and write Airtable bases, tables, and records via the REST API. Trigger when the user mentions an Airtable base, table, or wants rows created/updated/searched."
origin: bundled
prerequisites: ["Airtable API key", "base ID"]
exec:
  - name: airtable
    description: "Call the Airtable REST API (list, get, create, update, delete records)"
    command: scripts/airtable.sh
    args: "<method> <base> <table> [json-data] [--params ...]"
    runtime: shell
    side_effects: write
    timeout_secs: 60
---

# Airtable

> Requires setup: an Airtable personal access token in `AIRTABLE_API_KEY`
> and the base ID (starts with `app`). The table name goes on the command
> line. Without the key and base ID, nothing here works.

Thin curl wrapper over the Airtable REST API. The agent thinks in records;
the script handles auth headers and URL-encoding so the agent does not have
to.

## Tooling

`scripts/airtable.sh <method> <base> <table> [json] [-- view=Grid]`:

```sh
scripts/airtable.sh GET  appXXXX tblYYYYYYYYYYYYYY
scripts/airtable.sh POST appXXXX tblYYYYYYYYYYYYYY '{"fields":{"Name":"Ada"}}'
scripts/airtable.sh PATCH appXXXX tblYYYYYYYYYYYYYY '{"records":[{"id":"recAAA","fields":{"Status":"Done"}}]}'
```

Rate limit is 5 requests/second per base; the helper does not retry for you,
so space out bulk writes or script the loop yourself.

## Operating rules

- List the schema first (`GET <base>/<table>` with no records, or ask the
  user for field names) before writing. A record with wrong field names is
  created silently and wrong.
- Creates and updates are real and immediate. Confirm the payload with the
  user before a bulk write; single-record writes at the user's direction
  are fine.
- Never delete records without explicit approval naming the record IDs.
- Prefer `filterByFormula` on the server over fetching everything and
  filtering locally.
