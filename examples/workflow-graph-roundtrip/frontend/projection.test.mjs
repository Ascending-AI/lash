import test from "node:test";
import assert from "node:assert/strict";
import {
  startPosition,
  appendPosition,
  recordInput,
  conditionValue,
} from "./projection.mjs";
import { buildDataflow, planInputEdit } from "./dataflow.mjs";
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
function workflow(statements, body, expressions = []) {
  return {
    view: {
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
    buildDataflow(w, site([])).nodes.map((node) => node.s),
    [rows[0], rows[2]],
  );
  assert.deepEqual(startPosition(w, site([])), {
    block: site([]),
    before: site([0]),
  });
  assert.equal(buildDataflow(w, site([0, 0, 0])).nodes[0].s, rows[1]);
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
  assert.equal(recordInput(w, effect), undefined);
  const projected = buildDataflow(w, site([])).nodes;
  assert.deepEqual(
    projected.filter((node) => node.kind === "action").map((node) => node.s),
    [conversion, effect],
  );
  w.document.manifest.functions.opaque = "unrelated.transform";
  assert.deepEqual(buildDataflow(w, site([])).nodes, projected);
  assert.equal(recordInput(w, effect), undefined);
});

test("an effect's record argument is written in place and edits field by field or whole", () => {
  const argument = site([0, 0, 0]);
  const field = site([0, 0, 0, 0]);
  const record = { record: [{ field: "text", value: text("original") }] };
  const effect = row(
    [0],
    [],
    {
      let: {
        name: "step",
        value: {
          expr: {
            act: {
              kind: { perform: { effect: "display.show_message", result: "null" } },
              args: [record],
            },
          },
        },
      },
    },
    show([{ name: "input", expression: record, site: argument }]),
  );
  const expressions = [
    { site: argument, expression: record, references: [], available: [] },
    { site: field, expression: text("original"), references: [], available: [] },
  ];
  const w = workflow([effect], undefined, expressions);
  assert.deepEqual(recordInput(w, effect), { site: argument, record: record.record });
  const [node] = buildDataflow(w, site([])).nodes;
  assert.equal(node.kind, "action");
  assert.deepEqual(
    node.inputs.map((port) => [port.name, port.target]),
    [["text", { expression: field }]],
  );
  const changed = { record: [{ field: "text", value: text("edited") }] };
  assert.deepEqual(planInputEdit(w, node.wholeInputs[0], changed), [
    { set_argument: { action: effect.actions[0], index: 0, argument: changed } },
  ]);
  const bound = row([1], [], message("input"), shown("input"));
  assert.equal(recordInput(workflow([bound]), bound), undefined);
  const branch = row([2, 0, 0, 0], [2, 0, 0], {
    if: { condition: { variable: "enabled" }, then_block: [] },
  });
  const flag = row([0], [], {
    let: { name: "enabled", value: { expr: { literal: { bool: true } } } },
  });
  assert.equal(conditionValue(workflow([flag, branch]), branch), undefined);
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
    buildDataflow(w, site([])).nodes.map((node) => node.s),
    [branch, terminal],
  );
});

// Kernel unit statements serialize as strings, unlike payload-bearing statements.
test("break and continue end their scope without throwing or appending unreachable actions", () => {
  for (const kind of ["break", "continue"]) {
    const end = row([0, 1, 0], [0, 1], kind);
    const hidden = row([0, 1, 1], [0, 1], message("input"), shown("input"));
    const w = workflow([end, hidden]);
    assert.deepEqual(
      buildDataflow(w, site([0, 1])).nodes.map((node) => node.s),
      [end],
    );
    assert.deepEqual(appendPosition(w, site([0, 1])), {
      block: site([0, 1]),
      before: end.site,
    });
  }
});
