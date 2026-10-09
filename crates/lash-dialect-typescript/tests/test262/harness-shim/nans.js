// Lash-dialect rendering of Test262's harness/nans.js. `Number.NaN` is
// outside the dialect's standard-library surface (TS_METHOD_UNSUPPORTED); the
// specification defines it as NaN, which stands in its place, so the array
// keeps upstream's nine entries.
var NaNs = [
  NaN,
  NaN,
  NaN * 0,
  0/0,
  Infinity/Infinity,
  -(0/0),
  Math.pow(-1, 0.5),
  -Math.pow(-1, 0.5),
  Number("Not-a-Number"),
];
