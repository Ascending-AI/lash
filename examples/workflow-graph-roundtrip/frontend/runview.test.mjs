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
  counts: counts(coverage),
  ended: false,
  ...fields,
});

test("a node is active, waiting or failed by its actions' statuses, else done once it ran", () => {
  const statuses = new Map([[key(urgent.actions[0]), "started"]]);
  assert.deepEqual(nodeState(urgent, live({ statuses })), {
    state: "active",
    runs: 2,
    error: undefined,
  });
  statuses.set(key(urgent.actions[0]), "waiting");
  assert.equal(nodeState(urgent, live({ statuses })).state, "waiting");
  const errors = new Map([[key(urgent.actions[0]), "mock failure"]]);
  statuses.set(key(urgent.actions[0]), "failed");
  assert.deepEqual(nodeState(urgent, live({ statuses, errors })), {
    state: "failed",
    runs: 2,
    error: "mock failure",
  });
  assert.equal(nodeState(choice, live()).state, "done", "counted runs mean done");
  // A nested action is part of its statement's node.
  statuses.set(key(fetched), "waiting");
  assert.equal(nodeState(shown, live({ statuses })).state, "waiting");
  statuses.set(key(fetched), "succeeded");
  statuses.set(key(shown.actions[1]), "started");
  assert.equal(nodeState(shown, live({ statuses })).state, "active");
  errors.set(key(fetched), "fetch failed");
  statuses.set(key(fetched), "failed");
  assert.equal(nodeState(shown, live({ statuses, errors })).error, "fetch failed");
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
