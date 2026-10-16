import test from "node:test";
import assert from "node:assert/strict";
import {
  planInputEdit,
  encodeValue,
  constantValue,
  constantText,
} from "./dataflow.mjs";
test("editing an integral Float constant preserves its numeric kind, including inside typed records", () => {
  assert.deepEqual(encodeValue(1, "float"), { literal: { float: "1.0" } });
  assert.deepEqual(encodeValue(-0, "float"), { literal: { float: "-0.0" } });
  assert.deepEqual(
    encodeValue(
      { weight: 1 },
      { record: { fields: [{ name: "weight", ty: "float" }] } },
    ),
    { record: [{ field: "weight", value: { literal: { float: "1.0" } } }] },
  );
  assert.deepEqual(constantValue({ literal: { float: "1.0" } }), {
    known: true,
    value: 1,
  });
});

test("constant editor text preserves negative zero through an unchanged scalar or structured save", () => {
  assert.deepEqual(encodeValue(Number(constantText(-0)), "float"), {
    literal: { float: "-0.0" },
  });
  const value = { weight: -0, items: [-0, 2], label: "-0" };
  const read = JSON.parse(constantText(value));
  assert.deepEqual(read, value);
  assert.ok(Object.is(read.weight, -0));
  assert.ok(Object.is(read.items[0], -0));
});

test("saving structured constants preserves numeric kinds when canonical facets leave contents unknown", () => {
  const original = {
    list: [
      { literal: { float: "1.0" } },
      { literal: { text: "text" } },
      {
        record: [
          { field: "weight", value: { literal: { float: "2.0" } } },
          { field: "count", value: { literal: { int: "3" } } },
        ],
      },
    ],
  };
  const read = JSON.parse(constantText(constantValue(original).value));
  assert.deepEqual(encodeValue(read, { list: "any" }, original), original);
  read[2].count = 4;
  const changed = structuredClone(original);
  changed.list[2].record[1].value.literal.int = "4";
  assert.deepEqual(encodeValue(read, { list: "any" }, original), changed);
  assert.deepEqual(encodeValue(1, "int"), { literal: { int: "1" } });
});


// CANVAS-VALUE: host ports address whole arguments and canonical nested data.
test("effect argument edits use closed values and canonical field paths", () => {
  const site = (path) => ({ unit: { function: "demo" }, path });
  const action = site([0, 0]);
  const argument = site([0, 0, 0]);
  const field = site([0, 0, 0, 0]);
  const value = { literal: { text: "edited" } };
  const workflow = {
    identity: "0".repeat(64),
    view: { statements: [], dataflow: {
      actions: [{ site: action, kind: "perform", args: [{ site: argument }] }],
      expressions: [],
    } },
  };
  for (const [port, path] of [
    [{ target: { action, index: 0 } }, []],
    [{ target: { expression: field }, context: { statement: site([0]) } }, [0]],
  ]) {
    assert.deepEqual(planInputEdit(workflow, port, value), [{ set_value: {
      base: workflow.identity,
      target: { tool_argument: { action, index: 0, path } },
      value,
    } }]);
    assert.throws(() => planInputEdit(workflow, port, { closure: { body: [] } }), /source edit/);
  }
  assert.throws(() => planInputEdit(workflow, {}, value), /source edit/);
});
