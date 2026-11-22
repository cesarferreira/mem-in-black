const { delays } = require("./lib/backoff");

const TIMEOUT_MS = 30000; // per-attempt socket timeout

exports.TIMEOUT_MS = TIMEOUT_MS;
exports.callWithRetry = async (fn, sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))) => {
  const schedule = delays();
  let lastError;
  for (let attempt = 0; attempt <= schedule.length; attempt++) {
    try {
      return await fn({ timeout: TIMEOUT_MS });
    } catch (error) {
      lastError = error;
      if (attempt < schedule.length) await sleep(schedule[attempt]);
    }
  }
  throw lastError;
};
