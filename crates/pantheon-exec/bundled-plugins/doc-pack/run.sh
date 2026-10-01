#!/bin/sh
# doc-pack runner: speaks Pantheon's tool-plugin JSON protocol over stdio.
# One JSON object per line on stdin -> one JSON object per line on stdout.
exec python3 "$(dirname "$0")/runner.py"
