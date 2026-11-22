exports.sum = (xs) => xs.reduce((a, b) => a + b);

exports.average = (xs) => {
  if (xs.length === 0) throw new RangeError("average of empty list");
  return exports.sum(xs) / xs.length;
};
