#!/bin/sh
# Reference solution, applied to the repo. Used only by --validate.
set -e
echo '{ "baseMs": 400, "factor": 2, "attempts": 4, "capMs": 60000 }' > policy/backoff.json
