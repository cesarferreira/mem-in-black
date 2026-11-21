const assert = require("assert");
const { parseLine } = require("./parse");

assert.deepStrictEqual(parseLine("a, b ,c"), ["a", "b", "c"]);
assert.deepStrictEqual(parseLine('a, "b,c" ,d'), ["a", "b,c", "d"]);
console.log("parse tests passed");
