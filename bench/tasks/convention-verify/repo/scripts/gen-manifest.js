const fs = require("fs");
const path = require("path");
const { commands } = require("../tools");

const file = path.join(__dirname, "..", "manifest.json");
const next = JSON.stringify({ commands: Object.keys(commands).sort() }, null, 2) + "\n";
if (process.argv.includes("--check")) {
  if (fs.readFileSync(file, "utf8").replace(/\s+/g, "") !== next.replace(/\s+/g, "")) {
    console.error("manifest.json is stale; run node scripts/gen-manifest.js");
    process.exit(1);
  }
} else {
  fs.writeFileSync(file, next);
}
