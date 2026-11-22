const fs = require("fs");

const raw = JSON.parse(fs.readFileSync(__dirname + "/config.json", "utf8"));
// WIP(rename): maxRetries replaces retries, the old key is still read.
exports.load = () => ({ ...raw, maxRetries: raw.maxRetries ?? raw.retries });
