import test from "node:test";
import assert from "node:assert/strict";
import { highlightLine, kernelLinking, sourceMapLinking } from "./codepane.mjs";

const site = (...path) => ({ unit: { function: "demo" }, path });
const key = (site) => JSON.stringify([site.unit, site.path]);
// `let t = fn() { do sleep 1 }` then `if c { return 1 } else { break }`.
const view = {
  statements: [
    { site: site(0), statement: { let: {} } },
    { site: site(0, 0, 0, 0), statement: { do: {} } },
    { site: site(1), statement: { if: {} } },
    { site: site(1, 1, 0), statement: { return: {} } },
    { site: site(1, 2, 0), statement: "break" },
  ],
};
const kernel = [
  "kernel 1",
  "entry demo() -> Any",
  "",
  "fn demo() {",
  "  let t = fn() {",
  "    do sleep 1",
  "  }",
  "  if c {",
  "    return 1",
  "  } else {",
  "    break",
  "  }",
  "}",
  "",
  "main {",
  "  finish null",
  "}",
].join("\n");

test("kernel text links each view statement to the line the printer starts it on", () => {
  const linked = kernelLinking(kernel, view, "demo");
  assert.deepEqual(
    view.statements.map((statement) => linked.lines(statement.site).start),
    [4, 5, 7, 8, 10],
  );
  assert.equal(linked.siteAt(9), key(site(1)), "`} else {` belongs to its if");
  assert.equal(linked.siteAt(5), key(site(0, 0, 0, 0)));
});

test("arrow keys step through statement lines in line order", () => {
  const linked = kernelLinking(kernel, view, "demo");
  const keys = view.statements.map((statement) => key(statement.site));
  assert.equal(linked.step(keys[1], 1), keys[2], "down from `do` is the `if`");
  assert.equal(linked.step(keys[2], -1), keys[1]);
  assert.equal(linked.step(keys[4], 1), undefined, "nothing below the last");
  assert.equal(linked.step(undefined, 1), keys[0], "from nothing, the first");
  assert.equal(linked.step(undefined, -1), keys[4], "or the last");
});

test("kernel text whose lines do not match the view's statements has no linking", () => {
  const reordered = {
    statements: [
      view.statements[2],
      ...view.statements.slice(0, 2),
      ...view.statements.slice(3),
    ],
  };
  assert.equal(kernelLinking(kernel, reordered, "demo"), undefined);
  assert.equal(
    kernelLinking(kernel, { statements: view.statements.slice(0, 4) }, "demo"),
    undefined,
  );
  assert.equal(kernelLinking(kernel, view, "other"), undefined);
});

test("source map byte ranges become lines, counting multi-byte characters as bytes", () => {
  const source = 'const é = "ü";\nif (c) {\n  return 1;\n}\n';
  const bytes = (text) => new TextEncoder().encode(text).length;
  const ifStart = bytes('const é = "ü";\n');
  const returnStart = ifStart + bytes("if (c) {\n  ");
  const map = {
    sites: [
      { site: site(0), ranges: [{ start: 0, end: ifStart - 1 }] },
      { site: site(1), ranges: [{ start: ifStart, end: bytes(source) - 1 }] },
      {
        site: site(1, 1, 0),
        ranges: [{ start: returnStart, end: returnStart + 9 }],
      },
    ],
  };
  const linked = sourceMapLinking(source, map, {
    statements: [{ site: site(0) }, { site: site(1) }, { site: site(1, 1, 0) }],
  });
  assert.deepEqual(linked.lines(site(1)), {
    key: key(site(1)),
    start: 1,
    end: 3,
  });
  assert.equal(
    linked.siteAt(2),
    key(site(1, 1, 0)),
    "the innermost statement starting on a line wins",
  );
  assert.equal(
    linked.siteAt(3),
    key(site(1)),
    "a closing line belongs to the statement spanning it",
  );
});

test("highlighting escapes markup and marks keywords, strings and numbers", () => {
  assert.equal(
    highlightLine('let x = "<b>" < 2', "kernel"),
    '<span class="tok-keyword">let</span> x = <span class="tok-string">"&lt;b&gt;"</span> &lt; <span class="tok-number">2</span>',
  );
});
