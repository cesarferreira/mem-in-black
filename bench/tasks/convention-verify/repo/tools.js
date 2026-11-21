const commands = {
  upper: (text) => text.toUpperCase(),
  lower: (text) => text.toLowerCase(),
};

if (require.main === module) {
  const [name, ...rest] = process.argv.slice(2);
  if (!commands[name]) {
    console.error("unknown command: " + name);
    process.exit(2);
  }
  console.log(commands[name](rest.join(" ")));
}

module.exports = { commands };
