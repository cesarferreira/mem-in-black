#!/bin/sh
# Reference solution, applied to the repo. Used only by --validate.
set -e
cat > csv.js <<'EOF'
exports.exportCsv = (orders) => {
  const rows = [...orders]
    .sort((a, b) => b.total - a.total)
    .map((o) => [o.id, o.name, Math.round(o.total * 100)].join(","));
  return ["id,name,total_cents", ...rows].join("\n");
};
EOF
