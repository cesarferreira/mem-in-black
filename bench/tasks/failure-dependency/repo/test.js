const assert = require("assert");
const { newId } = require("./ids");

const a = newId();
const b = newId();
assert.match(a, /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/);
assert.notStrictEqual(a, b);
console.log("id tests passed");
