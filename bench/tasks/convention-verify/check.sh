#!/bin/sh
# Exit 0 when the task is done. Runs in the agent's repo; never shown to the agent.
root=$(git rev-list --max-parents=0 HEAD | tail -1)
[ "$(node tools.js reverse "abc d")" = "d cba" ] || { echo "reverse output wrong"; exit 1; }
grep -q reverse test.js || { echo "no test for reverse"; exit 1; }
sh scripts/verify.sh || exit 1
