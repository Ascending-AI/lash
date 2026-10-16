import test from "node:test";
import assert from "node:assert/strict";
import { highlightLine, kernelLinking, readableLinking } from "./codepane.mjs";

test("highlighting escapes markup and marks keywords, strings and numbers", () => {
  assert.equal(
    highlightLine('let x = "<b>" < 2', "kernel"),
    '<span class="tok-keyword">let</span> x = <span class="tok-string">"&lt;b&gt;"</span> &lt; <span class="tok-number">2</span>',
  );
});

// COARSE-SPANS: consume supplied ranges, including shared-line action carriers;
// preserve keyboard selection and refuse a stale canonical/readable lens.
test("COARSE-SPANS links and steps canonical carriers only through supplied printer ranges", () => {
  const site = (path) => ({unit: {function: "flow"}, path});
  const key = (site) => JSON.stringify([site.unit, site.path]);
  const nodes = [
    {site: site([0]), canonical: {first: 2, last: 4}, readable: [{first: 3, last: 5}]},
    {site: site([1, 0, 0]), canonical: {first: 6, last: 6}, readable: [{first: 7, last: 7}]},
    {site: site([1, 0]), canonical: {first: 6, last: 6}, readable: [{first: 7, last: 7}]},
  ];
  const view = {canonical_source: "é ü\n".repeat(8), readable: {kind: "available"}, nodes};
  const linked = kernelLinking(view.canonical_source, view);
  assert.deepEqual(linked.lines(nodes[0].site), {key: key(nodes[0].site), start: 1, end: 3});
  assert.equal(linked.step(undefined, 1), key(nodes[0].site));
  assert.equal(linked.step(key(nodes[0].site), 1), key(nodes[1].site));
  assert.equal(linked.step(key(nodes[1].site), 1), key(nodes[2].site));
  assert.equal(linked.step(key(nodes[2].site), -1), key(nodes[1].site));
  assert.equal(linked.step(key(nodes[2].site), 1), undefined);
  assert.equal(kernelLinking("edited", view), undefined);
  assert.deepEqual(readableLinking(view).lines(nodes[0].site), {key: key(nodes[0].site), start: 2, end: 4});
  const refused = {...view, readable: {kind: "refused"}};
  assert.equal(readableLinking(refused), undefined);
  assert.deepEqual(kernelLinking(view.canonical_source, refused).lines(nodes[0].site), linked.lines(nodes[0].site));
});
