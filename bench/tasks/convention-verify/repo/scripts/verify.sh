#!/bin/sh
set -e
cd "$(dirname "$0")/.."
node test.js
node scripts/gen-manifest.js --check
echo "verify ok"
