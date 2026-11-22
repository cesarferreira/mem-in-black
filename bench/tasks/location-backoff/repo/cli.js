const { callWithRetry } = require("./client");

callWithRetry(async () => {
  throw new Error("API unreachable");
}).catch((error) => {
  console.error(error.message);
  process.exit(1);
});
