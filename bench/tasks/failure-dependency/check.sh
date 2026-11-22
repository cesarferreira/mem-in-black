#!/bin/sh
# Exit 0 when the task is done. Runs in the agent's repo; never shown to the agent.
root=$(git rev-list --max-parents=0 HEAD | tail -1)
git diff --quiet "$root" -- test.js || { echo "test.js was modified"; exit 1; }
! grep -q '"dependencies"' package.json || { echo "a dependency was added"; exit 1; }
[ ! -d node_modules ] || { echo "node_modules exists"; exit 1; }
node test.js || exit 1
