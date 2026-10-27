import test from "node:test";
import assert from "node:assert/strict";
import { highlightLine, readableLinking } from "./codepane.mjs";

test("highlighting escapes markup and marks keywords, strings and numbers", () => {
  assert.equal(
    highlightLine('let x = "<b>" < 2', "kernel"),
    '<span class="tok-keyword">let</span> x = <span class="tok-string">"&lt;b&gt;"</span> &lt; <span class="tok-number">2</span>',
  );
});

// COARSE-SPANS: consume supplied ranges, including shared-line action carriers;
// preserve keyboard selection and refuse a stale or refused readable lens.
test("COARSE-SPANS links and steps carriers only through readable printer ranges", () => {
  const site = (path) => ({unit: {function: "flow"}, path});
  const key = (site) => JSON.stringify([site.unit, site.path]);
  const nodes = [
    {site: site([0]), readable: [{first: 3, last: 5}]},
    {site: site([1, 0, 0]), readable: [{first: 7, last: 7}]},
    {site: site([1, 0]), readable: [{first: 7, last: 7}]},
  ];
  const view = {readable: {kind: "available", source: "é ü\n".repeat(8)}, nodes};
  const linked = readableLinking(view);
  assert.deepEqual(linked.lines(nodes[0].site), {key: key(nodes[0].site), start: 2, end: 4});
  assert.equal(linked.step(undefined, 1), key(nodes[0].site));
  assert.equal(linked.step(key(nodes[0].site), 1), key(nodes[1].site));
  assert.equal(linked.step(key(nodes[1].site), 1), key(nodes[2].site));
  assert.equal(linked.step(key(nodes[2].site), -1), key(nodes[1].site));
  assert.equal(linked.step(key(nodes[2].site), 1), undefined);
  assert.equal(readableLinking(view, "edited"), undefined);
  assert.equal(readableLinking({...view, readable: {kind: "refused"}}), undefined);
});

// EXAMPLE-SOURCE: byte offsets cross non-ASCII and astral characters before
// reaching DOM strings; an edited document must never select through an old map.
test("EXAMPLE-SOURCE converts UTF-8 offsets and links only the mapped revision", async () => {
  const { sourceRanges, sourceLinking } = await import("./codepane.mjs");
  const text = 'const λ = "日本 🦀";\nreturn λ;\n';
  const site = {unit: {function: "demo"}, path: [1]};
  const start = new TextEncoder().encode(text.split("\n")[0] + "\n").length;
  const map = {document: "held", sites: [{site, location: {kind: "spelled", ranges: [{start, end: start + 10}]}}]};
  const ranges = sourceRanges(text, map)[0].ranges[0];
  assert.equal(text.slice(ranges.start, ranges.end), "return λ;");
  const workflow = {identity: "held", authored: {text, sourceMapId: "id", sourceMap: map},
    view: {nodes: [{site, body: {kind: "action", members: []}}]}};
  assert.deepEqual(sourceLinking(workflow).lines(site), {key: JSON.stringify([site.unit, site.path]), start: 1, end: 1});
  assert.equal(sourceLinking({...workflow, identity: "edited"}), undefined);
  assert.equal(sourceLinking({...workflow, authored: {text}}), undefined);
  assert.throws(() => sourceRanges(text, {sites: [{site, location: {kind: "spelled", ranges: [{start: 7, end: 8}]}}]}), /UTF-8 boundary/);
});
