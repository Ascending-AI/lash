// Lash-dialect rendering of Test262's harness/regExpUtils.js. Two spellings
// differ from upstream, neither in what the helpers compute:
// - `buildString` builds the string one code point at a time rather than
//   through `String.fromCodePoint.apply` over chunks: the dialect has no
//   `Function.prototype.apply`, and a spread argument to a builtin is an
//   open defect (FIG-3627).
// - `printCodePoint` pads the hexadecimal digits itself, because the dialect
//   refuses `Number.prototype.toString` with a radix; it only formats
//   failure messages.
function buildString(args) {
  const loneCodePoints = args.loneCodePoints;
  const ranges = args.ranges;
  const parts = [];
  for (const codePoint of loneCodePoints) {
    parts.push(String.fromCodePoint(codePoint));
  }
  for (let i = 0; i < ranges.length; i++) {
    const range = ranges[i];
    const end = range[1];
    let codePoint = range[0];
    while (codePoint <= end) {
      parts.push(String.fromCodePoint(codePoint));
      codePoint = codePoint + 1;
    }
  }
  return parts.join("");
}

function printCodePoint(codePoint) {
  const digits = "0123456789ABCDEF";
  let hex = "";
  let value = codePoint;
  while (hex.length < 6) {
    hex = digits[value % 16] + hex;
    value = Math.floor(value / 16);
  }
  return `U+${hex}`;
}

function printStringCodePoints(string) {
  const buf = [];
  for (const symbol of string) {
    buf.push(printCodePoint(symbol.codePointAt(0)));
  }
  return buf.join(" ");
}

function testPropertyEscapes(regExp, string, expression) {
  if (!regExp.test(string)) {
    for (const symbol of string) {
      const formatted = printCodePoint(symbol.codePointAt(0));
      __test262Assert(
        regExp.test(symbol),
        `\`${expression}\` should match ${formatted} (\`${symbol}\`)`
      );
    }
  }
}

function testPropertyOfStrings(args) {
  const regExp = args.regExp;
  const expression = args.expression;
  const matchStrings = args.matchStrings;
  const nonMatchStrings = args.nonMatchStrings;
  const allStrings = matchStrings.join("");
  if (!regExp.test(allStrings)) {
    for (const string of matchStrings) {
      __test262Assert(
        regExp.test(string),
        `\`${expression}\` should match ${string} (${printStringCodePoints(string)})`
      );
    }
  }
  if (!nonMatchStrings) {
    return;
  }
  const allNonMatchStrings = nonMatchStrings.join("");
  if (regExp.test(allNonMatchStrings)) {
    for (const string of nonMatchStrings) {
      __test262Assert(
        !regExp.test(string),
        `\`${expression}\` should not match ${string} (${printStringCodePoints(string)})`
      );
    }
  }
}

const testExtendedCharacterClass = testPropertyOfStrings;

function matchValidator(expectedEntries, expectedIndex, expectedInput) {
  return function (match) {
    assert["compareArray"](match, expectedEntries, "Match entries");
    assert["sameValue"](match.index, expectedIndex, "Match index");
    assert["sameValue"](match.input, expectedInput, "Match input");
  };
}
