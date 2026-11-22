#!/bin/sh
# Exit 0 when the task is done. Runs in the agent's repo; never shown to the agent.
root=$(git rev-list --max-parents=0 HEAD | tail -1)
expected="id,name,total_cents
3,Grace,1999
1,Ada,1250
2,Linus,700"
[ "$(node report.js --format csv)" = "$expected" ] || { echo "csv output wrong"; node report.js --format csv; exit 1; }
[ "$(node report.js | head -1)" = "3   Grace   19.99" ] || { echo "table output changed"; exit 1; }
