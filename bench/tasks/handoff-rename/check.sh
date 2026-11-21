#!/bin/sh
# Exit 0 when the task is done. Runs in the agent's repo; never shown to the agent.
root=$(git rev-list --max-parents=0 HEAD | tail -1)
[ "$(node worker.js 2>/dev/null)" = "max attempts: 4" ] || { echo "worker output wrong"; exit 1; }
grep -q '"maxRetries"' config.json && ! grep -q '"retries"' config.json || { echo "config.json not renamed"; exit 1; }
cp config.json /tmp/mib-bench-config.$$
echo '{ "retries": 5 }' > config.json
out=$(node worker.js 2>/tmp/mib-bench-err.$$)
err=$(cat /tmp/mib-bench-err.$$)
cp /tmp/mib-bench-config.$$ config.json
rm -f /tmp/mib-bench-config.$$ /tmp/mib-bench-err.$$
[ "$out" = "max attempts: 6" ] || { echo "legacy key stopped working"; exit 1; }
[ "$err" = 'warning: config key "retries" is deprecated, use "maxRetries"' ] || { echo "legacy warning wrong: $err"; exit 1; }
