const fs = require("fs");
const path = require("path");

const CACHE_VERSION = 1;
const file = path.join(__dirname, ".cache", "index.json");

exports.load = () => {
  try {
    const data = JSON.parse(fs.readFileSync(file, "utf8"));
    return data.version === CACHE_VERSION ? data.entries : {};
  } catch {
    return {};
  }
};

exports.save = (entries) => {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, JSON.stringify({ version: CACHE_VERSION, entries }));
};
