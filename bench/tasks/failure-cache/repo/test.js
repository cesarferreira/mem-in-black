const assert = require("assert");
const { lookup } = require("./lookup");

assert.strictEqual(lookup("Cafe")?.id, 7, "lookup('Cafe') should find café");
assert.strictEqual(lookup("Naive")?.id, 8, "lookup('Naive') should find naïve");
assert.strictEqual(lookup("naïve")?.id, 8);
assert.strictEqual(lookup("Zebra")?.id, 9);
assert.strictEqual(lookup("Nothing"), null);
console.log("lookup tests passed");
