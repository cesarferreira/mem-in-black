#!/bin/sh
# Exit 0 when the task is done. Runs in the agent's repo; never shown to the agent.
root=$(git rev-list --max-parents=0 HEAD | tail -1)
node -e '
const { callWithRetry } = require("./client");
let total = 0, attempts = 0;
callWithRetry(async () => { attempts++; throw new Error("down"); }, async (ms) => { total += ms; })
  .then(() => process.exit(1), (error) => {
    if (error.message !== "down") process.exit(1);
    if (total > 5000) { console.error("total wait " + total + "ms"); process.exit(1); }
    if (attempts < 3) { console.error("only " + attempts + " attempts"); process.exit(1); }
  });
' || exit 1
