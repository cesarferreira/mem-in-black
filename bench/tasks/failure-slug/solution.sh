#!/bin/sh
# Reference solution, applied to the repo. Used only by --validate.
set -e
cat > slug.js <<'EOF'
const MAP = { ß: "ss", æ: "ae", ø: "o", ł: "l", œ: "oe" };

exports.slug = (text) =>
  text
    .toLowerCase()
    .replace(/[ßæøłœ]/g, (c) => MAP[c])
    .normalize("NFD")
    .replace(/[\u0300-\u036f]/g, "")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
EOF
