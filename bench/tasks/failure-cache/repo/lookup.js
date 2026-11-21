const words = require("./data/words.json");
const cache = require("./cache");

const entries = cache.load();
const normalize = (text) => text.normalize("NFD").replace(/[\u0300-\u036f]/g, "").toLowerCase();

exports.lookup = (query) => {
  const key = query.toLowerCase();
  if (key in entries) return entries[key];
  const found = words.find((w) => normalize(w.word) === normalize(query)) ?? null;
  entries[key] = found;
  cache.save(entries);
  return found;
};
