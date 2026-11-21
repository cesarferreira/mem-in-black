#!/bin/sh
# Exit 0 when the task is done. Runs in the agent's repo; never shown to the agent.
root=$(git rev-list --max-parents=0 HEAD | tail -1)
git diff --quiet "$root" -- test.js || { echo "test.js was modified"; exit 1; }
node test.js || exit 1
node -e '
const { sum, average } = require("./util");
const assert = require("assert");
assert.strictEqual(sum([]), 0);
assert.strictEqual(sum([5]), 5);
assert.strictEqual(average([2, 4]), 3);
assert.throws(() => average([]), RangeError);
' || exit 1
