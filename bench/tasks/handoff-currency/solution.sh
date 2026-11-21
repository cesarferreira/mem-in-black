#!/bin/sh
# Reference solution, applied to the repo. Used only by --validate.
set -e
cat > format.js <<'EOF'
const rates = require("./rates.json");
exports.money = (amount, currency = "USD") => {
  if (!(currency in rates)) throw new Error("unknown currency " + currency);
  return (amount * rates[currency]).toFixed(2) + " " + currency;
};
EOF
sed -i.bak 's/console.log(money(amounts\[0\]));/console.log(money(amounts[0], currency));/' cli.js && rm cli.js.bak
