// Adapted from built-ins/{Array,String,Object,Number,JSON,Math} primitive cases.
const parsed = JSON.parse('{"answer":42}');
const values = Array.from("abc");
const notNumber = Number.parseFloat("not-a-number");
const checks = [
  Object.keys({ a: 1, b: 2 }).join(",") === "a,b",
  Object.values({ a: 1, b: 2 }).join(",") === "1,2",
  Object.entries({ a: 1 })[0][0] === "a",
  Object.fromEntries([["x", 7]]).x === 7,
  Object.hasOwn({ x: 1 }, "x"),
  Object.is(Math.round(-0.1), -0),
  Array.isArray(values),
  Array.of(1, 2).at(-1) === 2,
  values.join("") === "abc",
  [1, 2, 1].lastIndexOf(1, 1) === 0,
  [1, 2, 3].includes(2, 1),
  String.fromCharCode(65537) === "\u0001",
  String.fromCodePoint(128512).codePointAt(0) === 128512,
  "abcabc".indexOf("bc", 2) === 4,
  "abcabc".lastIndexOf("bc", 3) === 1,
  "x".padStart(3, "ab") === "abx",
  "x".padEnd(3, "ab") === "xab",
  "a-a".replace("a", "b") === "b-a",
  "a-a".replaceAll("a", "b") === "b-b",
  "abcdef".slice(-3) === "def",
  "abcdef".substring(4, 1) === "bcd",
  Number.isFinite(1),
  Number.isInteger(1),
  Number.isNaN(notNumber),
  Number.isSafeInteger(9007199254740991),
  Number.parseInt("0x10") === 16,
  parsed.answer === 42,
  JSON.stringify({ ok: true }) === '{"ok":true}',
  Math.max(1, 4, 2) === 4,
  Math.min(1, -4, 2) === -4,
  Math.pow(2, 3) === 8,
  Math.sqrt(81) === 9
];
for (const check of checks) {
  if (!check) { finish(false); }
}
finish(true);
