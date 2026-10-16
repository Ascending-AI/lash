import test from "node:test";
import assert from "node:assert/strict";
import { armsTaken, counts, nodeState, replayAt } from "./runview.mjs";

const site = (...path) => ({ unit: { function: "demo" }, path });
const key = (site) => JSON.stringify([site.unit, site.path]);
// `for t in ts { if t.urgent { do sleep 1 } else { do sleep 2 } }` then
// `do display`, which never ran.
const loop = { site: site(0) };
const choice = { site: site(0, 0, 0) };
const urgent = { site: site(0, 0, 0, 0, 0), actions: [site(0, 0, 0, 0, 0, 0)] };
const later = { site: site(0, 0, 0, 1, 0), actions: [site(0, 0, 0, 1, 0, 0)] };
const display = { site: site(1), actions: [site(1, 0)] };
// `let shown = perform show(perform fetch() as Any) as Any`: the fetch is
// nested in the show's argument and runs first.
const fetched = site(2, 0, 0);
const shown = { site: site(2), actions: [fetched, site(2, 0)] };
const view = {
  scopes: [
    { block: site(0, 0), kind: "for", statement: site(0) },
    { block: site(0, 0, 0, 0), kind: "then", statement: site(0, 0, 0) },
    { block: site(0, 0, 0, 1), kind: "else", statement: site(0, 0, 0) },
  ],
};
const coverage = {
  observation: "counts",
  statements: [
    { site: loop.site, runs: 1 },
    { site: choice.site, runs: 2 },
    { site: urgent.site, runs: 2 },
    { site: later.site, runs: 0 },
    { site: display.site, runs: 0 },
  ],
  scopes: [
    { block: site(0, 0), entries: 2 },
    { block: site(0, 0, 0, 0), entries: 2 },
    { block: site(0, 0, 0, 1), entries: 0 },
  ],
};
const live = (fields = {}) => ({
  statuses: new Map(),
  errors: new Map(),
  counts: counts(coverage, coverage.statements),
  ended: false,
  ...fields,
});

test("COARSE-COUNTS keeps a waiting operand distinct from its unissued parent", () => {
  const operand = { site: fetched, node_kind: "action" };
  const parent = { site: shown.actions[1], node_kind: "action" };
  const run = live({ counts: counts({ actions: [{ site: fetched, runs: 1 }, { site: parent.site, runs: 0 }], statements: [{site: shown.site, runs: 1}] }, [{ site: fetched, runs: 1 }, { site: parent.site, runs: 0 }]),
    statuses: new Map([[key(fetched), "waiting"]]) });
  assert.deepEqual(nodeState(operand, run), { state: "waiting", runs: 1, error: undefined });
  assert.deepEqual(nodeState(parent, run), { state: undefined, runs: 0, error: undefined });
  const off = live({ ended: true, counts: counts({observation: "off"}, [{site: parent.site, runs: null}]) });
  assert.deepEqual(nodeState(parent, off), { state: undefined, runs: undefined, error: undefined });
});

test("a node that did not run is dimmed only once the run has ended", () => {
  assert.equal(nodeState(display, live()).state, undefined, "it may still run");
  assert.equal(nodeState(display, live({ ended: true })).state, "skipped");
  const unobserved = live({ counts: counts(undefined), ended: true });
  assert.deepEqual(nodeState(display, unobserved), {
    state: undefined,
    runs: undefined,
    error: undefined,
  });
});

test("a taken branch is the arm lash counted entering", () => {
  assert.deepEqual(armsTaken(view, live(), choice.site), { then: true, else: false });
  assert.deepEqual(armsTaken(view, live({ counts: counts(undefined) }), choice.site), {});
});

test("replaying the path counts each start up to the step and marks its statement active", () => {
  const steps = [
    { kind: "statement", site: loop.site },
    { kind: "scope", block: site(0, 0) },
    { kind: "statement", site: choice.site },
    { kind: "scope", block: site(0, 0, 0, 0) },
    { kind: "statement", site: urgent.site },
    { kind: "scope", block: site(0, 0) },
    { kind: "statement", site: choice.site },
  ];
  const run = live({ ended: true, replay: replayAt(steps, 4) });
  assert.deepEqual(nodeState(urgent, run), { state: "active", runs: 1 });
  assert.deepEqual(nodeState(choice, run), { state: "done", runs: 1 });
  assert.deepEqual(nodeState(later, run), { state: "skipped", runs: 0 });
  assert.deepEqual(armsTaken(view, run, choice.site), { then: true, else: false });
  const end = live({ ended: true, replay: replayAt(steps, steps.length - 1) });
  assert.deepEqual(nodeState(choice, end), { state: "active", runs: 2 });
  assert.deepEqual(nodeState(urgent, end), { state: "done", runs: 1 });
});
