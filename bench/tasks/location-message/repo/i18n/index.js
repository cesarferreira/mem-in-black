const messages = require("./en.json");

exports.t = (key) => messages[key] ?? key;
