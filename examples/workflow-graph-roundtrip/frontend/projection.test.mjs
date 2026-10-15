import test from "node:test";
import assert from "node:assert/strict";
import {
  actionOf,
  startPosition,
  appendPosition,
  recordInput,
  conditionValue,
} from "./projection.mjs";
import { buildDataflow } from "./dataflow.mjs";
const site = (path) => ({ unit: { function: "demo" }, path });
const row = (path, block, statement) => ({
  site: site(path),
  block: site(block),
  statement,
});
const text = (value) => ({ literal: { text: value } });
const input = (name) => ({
  let: {
    name,
    value: { expr: { record: [{ field: "text", value: text("original") }] } },
  },
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
function workflow(statements, body) {
  return {
    view: {
      statements,
      dataflow: {
        bindings: [],
        expressions: [],
        parameters: [],
        returns: [],
        assignments: [],
        actions: statements
          .filter((s) => actionOf(s))
          .map((s) => ({
            site: site([...s.site.path, 0]),
            statement: s.site,
            block: s.block,
            args: [],
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
    row([0, 0, 0, 0], [0, 0, 0], hidden),
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
  const conversion = row([1], [], {
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
  });
  const effect = row([2], [], message("converted"));
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

test("field editing does not capture outer, shared or reassigned bindings", () => {
  const source = row([0], [], input("input"));
  const inner = row([1, 0, 0, 0], [1, 0, 0], message("input"));
  assert.equal(recordInput(workflow([source, inner]), inner), undefined);
  const effect = row([1], [], message("input"));
  assert.equal(
    recordInput(workflow([source, effect]), effect).statement,
    source,
  );
  const shared = row([2], [], message("input"));
  assert.equal(
    recordInput(workflow([source, effect, shared]), effect),
    undefined,
  );
  const write = row([1], [], {
    assign: { place: { variable: "input" }, value: { expr: { record: [] } } },
  });
  assert.equal(
    recordInput(workflow([source, write, shared]), shared),
    undefined,
  );
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
  const unreachable = row([2], [], message("input"));
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
    const hidden = row([0, 1, 1], [0, 1], message("input"));
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
