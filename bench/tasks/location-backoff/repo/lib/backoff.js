const policy = require("../policy/backoff.json");

exports.delays = () =>
  Array.from({ length: policy.attempts - 1 }, (_, i) =>
    Math.min(policy.capMs, policy.baseMs * policy.factor ** i),
  );
