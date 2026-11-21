#!/bin/sh
# Reference solution, applied to the repo. Used only by --validate.
set -e
cat > ids.js <<'EOF'
const { randomUUID } = require("node:crypto");

exports.newId = () => randomUUID();
EOF
