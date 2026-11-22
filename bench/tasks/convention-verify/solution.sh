#!/bin/sh
# Reference solution, applied to the repo. Used only by --validate.
set -e
cat > tools.js <<'EOF'
const commands = {
  reverse: (text) => [...text].reverse().join(""),
  upper: (text) => text.toUpperCase(),
  lower: (text) => text.toLowerCase(),
};

if (require.main === module) {
  const [name, ...rest] = process.argv.slice(2);
  if (!commands[name]) {
    console.error("unknown command: " + name);
    process.exit(2);
  }
  console.log(commands[name](rest.join(" ")));
}

module.exports = { commands };
EOF
cat >> test.js <<'EOF'
assert.strictEqual(commands.reverse("abc"), "cba");
EOF
node scripts/gen-manifest.js
