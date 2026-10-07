#!/bin/sh
# cf-ops runner: speaks Pantheon's tool-plugin JSON protocol over stdio.
# One JSON object per line on stdin -> one JSON object per line on stdout.
exec node "$(dirname "$0")/runner.mjs"
