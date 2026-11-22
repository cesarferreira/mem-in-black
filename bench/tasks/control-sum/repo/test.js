const assert = require("assert");
const { sum, average } = require("./util");

assert.strictEqual(sum([1, 2, 3]), 6);
assert.strictEqual(average([2, 4]), 3);
assert.throws(() => average([]), RangeError);
console.log("util tests passed");
