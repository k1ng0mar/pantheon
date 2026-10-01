#!/usr/bin/env bash
# new-handoff.sh: create a kanban handoff file from the standard template.
# Usage: new-handoff.sh <task-slug>   (writes handoff-<slug>-<timestamp>.md here)
set -u
slug="${1:?usage: new-handoff.sh <task-slug>}"
file="handoff-${slug}-$(date +%Y%m%d-%H%M).md"
if [ -e "$file" ]; then
  echo "refusing to overwrite existing $file" >&2
  exit 1
fi
cat > "$file" <<'MD'
# Handoff: builder -> checker

## Task
<!-- What was asked, in one paragraph, with a pointer to the spec or ticket. -->

## What changed
<!-- Files, commits, or artifacts. Paths, not descriptions. -->

## How to verify
<!-- Exact steps: commands to run, pages to open, behaviors to check. -->

## Spec compliance
<!-- Which parts of the spec each change satisfies, one line per requirement. -->

## Known gaps
<!-- Unfinished, untested, or deliberately out of scope. Write "none" if empty. -->

## Do not re-check
<!-- Already verified, with evidence. The checker may still check it. -->

## Reviewer decision
<!-- For the checker: approve / request changes / escalate, with what was checked. -->
MD
echo "wrote $file"
