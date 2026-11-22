const { exportCsv } = require("./csv");
const orders = require("./data.json");

const at = process.argv.indexOf("--format");
const format = at === -1 ? "table" : process.argv[at + 1];
if (format === "table") {
  for (const o of orders) console.log(String(o.id).padEnd(4) + o.name.padEnd(8) + o.total.toFixed(2));
} else if (format === "csv") {
  console.log(exportCsv(orders));
} else {
  console.error("unknown format: " + format);
  process.exit(2);
}
