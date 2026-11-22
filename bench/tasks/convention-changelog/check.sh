#!/bin/sh
# Exit 0 when the task is done. Runs in the agent's repo; never shown to the agent.
root=$(git rev-list --max-parents=0 HEAD | tail -1)
[ "$(node cli.js)" = "Hello from textools" ] || { echo "greeting changed"; exit 1; }
[ -z "$(node cli.js --quiet)" ] || { echo "--quiet still prints"; exit 1; }
awk '/^## /{in_unreleased = ($0 == "## Unreleased")} in_unreleased && /^- \[cli\] .*--quiet/{found=1} END{exit !found}' CHANGES.txt || { echo "no '- [cli] ... --quiet' line under Unreleased in CHANGES.txt"; exit 1; }
