#!/bin/sh
# Reference solution, applied to the repo. Used only by --validate.
set -e
cat > config.js <<'EOF'
const fs = require("fs");

const raw = JSON.parse(fs.readFileSync(__dirname + "/config.json", "utf8"));
if (raw.retries !== undefined && raw.maxRetries === undefined) {
  console.error('warning: config key "retries" is deprecated, use "maxRetries"');
}
exports.load = () => ({ ...raw, maxRetries: raw.maxRetries ?? raw.retries });
EOF
cat > worker.js <<'EOF'
const { load } = require("./config");

const { maxRetries } = load();
console.log("max attempts: " + (maxRetries + 1));
EOF
echo '{ "maxRetries": 3, "queue": "orders" }' > config.json
