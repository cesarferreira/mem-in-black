#!/bin/sh
# Reference solution, applied to the repo. Used only by --validate.
set -e
cat > cli.js <<'EOF'
const args = process.argv.slice(2);
if (args.includes("--help")) {
  console.log("usage: node cli.js [--help] [--quiet]");
  process.exit(0);
}
if (!args.includes("--quiet")) console.log("Hello from textools");
EOF
node -e 'const fs = require("fs"); fs.writeFileSync("CHANGES.txt", fs.readFileSync("CHANGES.txt", "utf8").replace("## Unreleased\n", "## Unreleased\n- [cli] add --quiet flag to suppress the greeting\n"));'
