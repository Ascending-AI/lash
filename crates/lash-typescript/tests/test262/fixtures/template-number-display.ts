// Adapted from test/language/expressions/template-literal/tv-no-substitution.js
// and test/language/expressions/division primitive Number edge cases.
const nan = 0 / 0;
finish(`${nan},${1 / 0},${-1 / 0},${-0}` === "NaN,Infinity,-Infinity,0");
