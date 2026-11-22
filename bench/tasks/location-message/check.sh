#!/bin/sh
# Exit 0 when the task is done. Runs in the agent's repo; never shown to the agent.
root=$(git rev-list --max-parents=0 HEAD | tail -1)
out=$(node cli.js -5 2>&1 >/dev/null); code=$?
[ "$out" = "Amount must be zero or more." ] || { echo "message wrong: $out"; exit 1; }
node cli.js -5 >/dev/null 2>&1; [ $? -eq 2 ] || { echo "exit code changed"; exit 1; }
node i18n/build.js --check || exit 1
