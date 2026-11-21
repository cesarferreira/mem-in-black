#!/bin/sh
# Exit 0 when the task is done. Runs in the agent's repo; never shown to the agent.
root=$(git rev-list --max-parents=0 HEAD | tail -1)
[ "$(node cli.js 10 --currency EUR)" = "9.20 EUR" ] || { echo "EUR output wrong"; exit 1; }
[ "$(node cli.js 10 --currency GBP)" = "7.90 GBP" ] || { echo "GBP output wrong"; exit 1; }
[ "$(node cli.js 10)" = "10.00 USD" ] || { echo "default output wrong"; exit 1; }
