const fs = require("fs");
const path = require("path");

const lines = fs.readFileSync(path.join(__dirname, "source.txt"), "utf8").split("\n").filter(Boolean);
const messages = {};
for (const line of lines) {
  const at = line.indexOf("=");
  messages[line.slice(0, at)] = line.slice(at + 1);
}
const output = JSON.stringify(messages, null, 2) + "\n";
const target = path.join(__dirname, "en.json");
if (process.argv.includes("--check")) {
  if (fs.readFileSync(target, "utf8") !== output) {
    console.error("en.json is out of date with source.txt");
    process.exit(1);
  }
} else {
  fs.writeFileSync(target, output);
}
