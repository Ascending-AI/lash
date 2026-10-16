import test from "node:test";
import assert from "node:assert/strict";
import { startPosition, appendPosition, actionOf } from "./projection.mjs";
import { buildDataflow } from "./dataflow.mjs";
const site = (path) => ({ unit: { function: "demo" }, path });

test("COARSE-EVAL renders every supplied action carrier without choosing a representative statement action", () => {
  const statement = { site: site([0]), block: site([]), statement: { let: {} }, actions: [site([0, 0, 0]), site([0, 0])] };
  const nodes = statement.actions.map((at) => ({ kind: "action", site: at, statement: statement.site, block: site([]), members: [], bindings: [], reads: [], writes: [] }));
  const actions = nodes.map((node) => ({ site: node.site, kind: "call", args: [], available: [], result_type: "any" }));
  const workflow = { view: { nodes, statements: [statement], dataflow: { actions, bindings: [], expressions: [], parameters: [], returns: [], assignments: [] } } };
  const projected = buildDataflow(workflow, site([])).nodes;
  assert.equal(projected.length, 2);
  assert.deepEqual(projected.map((node) => actionOf(workflow, node.s).site), statement.actions);
  assert.equal(actionOf(workflow, statement), undefined);
});

test("empty scopes are insertable and additions precede an exact terminal statement", () => {
  const workflow = { view: { statements: [] } };
  assert.deepEqual(startPosition(workflow, site([0])), { block: site([0]) });
  workflow.view.statements = [{ site: site([1]), block: site([]), statement: { return: { value: { literal: "null" } } } }];
  assert.deepEqual(appendPosition(workflow, site([])), { block: site([]), before: site([1]) });
});

test("COARSE-PARTITION exposes supplied declarations and writes as code-group ports", () => {
  const owner = {site: site([2]), block: site([]), statement: {assign: {}}};
  const workflow = {view: {nodes: [{kind: "code", site: owner.site, statement: owner.site, block: owner.block,
    members: [owner.site], bindings: [0], writes: [0, 1], reads: [1]}], statements: [owner],
    dataflow: {bindings: [{id: 0, name: "created", type: "any"}, {id: 1, name: "written", type: "any"}],
      actions: [], expressions: [], parameters: [], returns: [], assignments: []}}};
  const node = buildDataflow(workflow, site([])).nodes[0];
  assert.deepEqual(node.outputs.map((port) => port.binding), [0, 1]);
  assert.deepEqual(node.inputs.map((port) => port.references), [[1]]);
});
