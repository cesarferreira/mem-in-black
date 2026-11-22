#!/bin/sh
# Runs in the repo after the fixture is committed. The stale cache is git-ignored
# state (.gitignore lists .cache/), so it cannot live in repo/: a checkout would not have it.
set -e
mkdir -p .cache
printf '%s' '{"version":1,"entries":{"cafe":null,"naive":null}}' > .cache/index.json
