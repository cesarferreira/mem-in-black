const assert = require("assert");
const { commands } = require("./tools");

assert.strictEqual(commands.upper("abc"), "ABC");
assert.strictEqual(commands.lower("ABC"), "abc");
