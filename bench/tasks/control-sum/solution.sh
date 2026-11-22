#!/bin/sh
# Reference solution, applied to the repo. Used only by --validate.
set -e
sed -i.bak 's/xs.reduce((a, b) => a + b)/xs.reduce((a, b) => a + b, 0)/' util.js && rm util.js.bak
