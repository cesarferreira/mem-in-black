const { load } = require("./config");

const { retries } = load();
console.log("max attempts: " + (retries + 1));
