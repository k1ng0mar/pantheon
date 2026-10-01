#!/bin/sh
# skill-vetter runner: speaks Pantheon's tool-plugin JSON protocol over stdio.
exec python3 "$(dirname "$0")/runner.py"
