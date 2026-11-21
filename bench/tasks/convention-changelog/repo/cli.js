const args = process.argv.slice(2);
if (args.includes("--help")) {
  console.log("usage: node cli.js [--help]");
  process.exit(0);
}
console.log("Hello from textools");
