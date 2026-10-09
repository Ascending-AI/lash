// Lash-dialect rendering of Test262's harness/decimalToHexString.js. The
// dialect refuses reassigning a parameter (TS_ASSIGN_CONST), so the shifting
// value lives in a `let`; the arithmetic is upstream's.
function decimalToHexString(n) {
  const hex = "0123456789ABCDEF";
  let value = n >>> 0;
  let s = "";
  while (value) {
    s = hex[value & 0xf] + s;
    value = value >>> 4;
  }
  while (s.length < 4) {
    s = "0" + s;
  }
  return s;
}

function decimalToPercentHexString(n) {
  const hex = "0123456789ABCDEF";
  return "%" + hex[(n >> 4) & 0xf] + hex[n & 0xf];
}
