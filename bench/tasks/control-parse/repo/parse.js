exports.parseLine = (line) => {
  const tmp = [];
  let current = "";
  let quoted = false;
  for (const ch of line) {
    if (ch === '"') quoted = !quoted;
    else if (ch === "," && !quoted) {
      tmp.push(current.trim());
      current = "";
    } else current += ch;
  }
  tmp.push(current.trim());
  return tmp;
};
