// Adapted from test/language/expressions/logical-and/S11.11.1_A3_T1.js,
// logical-or/S11.11.2_A3_T1.js, and conditional/coalesce-expr-ternary.js.
finish((false && 7) === false && (0 || 9) === 9 && (null ?? 4) === 4 && (true ? 1 : 2) === 1);
