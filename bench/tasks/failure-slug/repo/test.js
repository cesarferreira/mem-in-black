const assert = require("assert");
const { slug } = require("./slug");

const cases = [
  ["Hello World", "hello-world"],
  ["Crème Brûlée", "creme-brulee"],
  ["Straße", "strasse"],
  ["Ærø Færgen", "aero-faergen"],
  ["Łódź Café", "lodz-cafe"],
  ["Œuvre d'art", "oeuvre-d-art"],
  ["  --Trim me--  ", "trim-me"],
];
for (const [input, expected] of cases) assert.strictEqual(slug(input), expected, input);
console.log("slug tests passed");
