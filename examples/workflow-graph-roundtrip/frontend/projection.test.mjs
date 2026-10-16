import test from "node:test";
import assert from "node:assert/strict";
import {
  startPosition,
  appendPosition,
  recordInput,
  actionOf,
} from "./projection.mjs";
import { buildDataflow } from "./dataflow.mjs";
const site = (path) => ({ unit: { function: "demo" }, path });
// A statement as lash's view lists it; `action` is its own action's view,
// at the statement's first child.
const row = (path, block, statement, action) => ({
  site: site(path),
  block: site(block),
  statement,
  actions: action ? [site([...path, 0])] : [],
  action,
});
const text = (value) => ({ literal: { text: value } });
const input = (name) => ({
  let: {
    name,
    value: { expr: { record: [{ field: "text", value: text("original") }] } },
  },
});
const show = (args) => ({
  kind: "perform",
  callee: { effect: "display.show_message" },
  args,
});
const message = (name) => ({
  do: {
    action: {
      perform: {
        effect: "display.show_message",
        args: [{ variable: name }],
        result: "null",
      },
    },
  },
});
const shown = (name) => show([{ name: "input", expression: { variable: name } }]);
const sameSite = (left, right) => JSON.stringify(left) === JSON.stringify(right);
const terminal = (statement) => {
  const kind = typeof statement === "string" ? statement : Object.keys(statement)[0];
  return ["return", "throw", "finish", "fail", "break", "continue"].includes(kind);
};
function workflow(statements, body, expressions = []) {
  return {
    view: {
      nodes: statements
        .filter(
          (s, index) =>
            !statements
              .slice(0, index)
              .some(
                (prior) =>
                  sameSite(prior.block, s.block) && terminal(prior.statement),
              ),
        )
        .map((s) => ({
          kind: s.action ? "action" : "code",
          site: s.action ? s.actions[0] : s.site,
          statement: s.site,
          block: s.block,
          members: s.action ? [] : [s.site],
          bindings: [],
          reads: [],
          writes: [],
        })),
      statements,
      dataflow: {
        bindings: [],
        expressions,
        parameters: [],
        returns: [],
        assignments: [],
        actions: statements
          .filter((s) => s.action)
          .map((s) => ({
            ...s.action,
            site: s.actions[0],
            statement: s.site,
            block: s.block,
            available: [],
            result_type: "any",
          })),
      },
    },
    document: {
      functions: { demo: { body: body ?? statements.map((s) => s.statement) } },
    },
  };
}

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

test("an unused closure is not executed or chosen as the entry insertion scope", () => {
  const hidden = message("input");
  const declaration = {
    let: {
      name: "unused",
      value: { expr: { closure: { params: ["input"], body: [hidden] } } },
    },
  };
  const rows = [
    row([0], [], declaration),
    row([0, 0, 0, 0], [0, 0, 0], hidden, shown("input")),
    row([1], [], { return: { value: { literal: "null" } } }),
  ];
  const w = workflow(rows, [declaration, rows[2].statement]);
  assert.deepEqual(
    buildDataflow(w, site([])).nodes.map((node) => node.s.statement_site),
    [rows[0].site, rows[2].site],
  );
  assert.deepEqual(startPosition(w, site([])), {
    block: site([]),
    before: site([0]),
  });
  const nestedAction = buildDataflow(w, site([0, 0, 0])).nodes[0];
  assert.equal(nestedAction.kind, "action");
  assert.equal(nestedAction.s.statement_site, rows[1].site);
});

test("a library call is opaque regardless of its display name", () => {
  const source = row([0], [], input("source"));
  const conversion = row(
    [1],
    [],
    {
      let: {
        name: "converted",
        value: {
          action: {
            call: {
              callee: { library: "opaque" },
              args: [{ variable: "source" }],
            },
          },
        },
      },
    },
    {
      kind: "call",
      callee: { library: { id: "opaque", name: "ts.boundary" } },
      args: [{ expression: { variable: "source" } }],
    },
  );
  const effect = row([2], [], message("converted"), shown("converted"));
  const w = workflow([source, conversion, effect]);
  w.document.manifest = { functions: { opaque: "ts.boundary" } };
  const effectNode = buildDataflow(w, site([])).nodes.find(
    (node) => node.s.statement_site === effect.site,
  );
  assert.ok(effectNode);
  assert.equal(recordInput(w, effectNode.s), undefined);
  const projected = buildDataflow(w, site([])).nodes;
  assert.deepEqual(
    projected
      .filter((node) => node.kind === "action")
      .map((node) => node.s.statement_site),
    [conversion.site, effect.site],
  );
  w.document.manifest.functions.opaque = "unrelated.transform";
  assert.deepEqual(buildDataflow(w, site([])).nodes, projected);
  assert.equal(recordInput(w, effectNode.s), undefined);
});

test("empty scopes are insertable and global additions precede a terminal statement", () => {
  assert.deepEqual(startPosition(workflow([]), site([0, 0, 0])), {
    block: site([0, 0, 0]),
  });
  const branch = row([0], [], {
    if: {
      condition: { literal: { bool: true } },
      then_block: [],
      else_block: [],
    },
  });
  const terminal = row([1], [], { return: { value: { literal: "null" } } });
  const unreachable = row([2], [], message("input"), shown("input"));
  const w = workflow([branch, terminal, unreachable]);
  assert.deepEqual(appendPosition(w, site([])), {
    block: site([]),
    before: terminal.site,
  });
  assert.deepEqual(
    buildDataflow(w, site([])).nodes.map((node) => node.s.statement_site),
    [branch.site, terminal.site],
  );
});

// Kernel unit statements serialize as strings, unlike payload-bearing statements.
test("break and continue end their scope without throwing or appending unreachable actions", () => {
  for (const kind of ["break", "continue"]) {
    const end = row([0, 1, 0], [0, 1], kind);
    const hidden = row([0, 1, 1], [0, 1], message("input"), shown("input"));
    const w = workflow([end, hidden]);
    assert.deepEqual(
      buildDataflow(w, site([0, 1])).nodes.map((node) => node.s.statement_site),
      [end.site],
    );
    assert.deepEqual(appendPosition(w, site([0, 1])), {
      block: site([0, 1]),
      before: end.site,
    });
  }
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
