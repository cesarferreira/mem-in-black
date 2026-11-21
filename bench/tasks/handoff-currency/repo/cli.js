const { money } = require("./format");

const args = process.argv.slice(2);
let currency = "USD";
const amounts = [];
for (let i = 0; i < args.length; i++) {
  if (args[i] === "--currency") currency = args[++i];
  else amounts.push(Number(args[i]));
}
// WIP(currency): the flag is parsed but not applied yet.
console.log(money(amounts[0]));
