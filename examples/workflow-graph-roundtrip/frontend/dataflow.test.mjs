import test from "node:test";
import assert from "node:assert/strict";
import {
  encodeValue,
  constantValue,
  planEndAfter,
  planConstantPromotion,
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

test("ending at an output preserves the producer and later code in its lexical block", () => {
  const unit = { function: "example" };
  const block = { unit, path: [0, 0, 0] };
  const s = {
    site: { unit, path: [0, 0, 0, 1] },
    block,
    statement: { let: {} },
  };
  const next = {
    site: { unit, path: [0, 0, 0, 2] },
    block,
    statement: { do: {} },
  };
  const source = { binding: 4, expression: { variable: "report" } };
  const node = { s, outputs: [source] };
  const workflow = { view: { statements: [s, next] } };
  const original = structuredClone(workflow);
  for (const operation of ["return", "finish"]) {
    assert.deepEqual(planEndAfter(workflow, node, source, operation), [
      {
        insert_statement: {
          at: { block, before: next.site },
          statement: { [operation]: { value: source.expression } },
        },
      },
    ]);
  }
  assert.deepEqual(workflow, original);
  next.statement = { return: { value: { literal: "null" } } };
  assert.deepEqual(planEndAfter(workflow, node, source, "finish"), [
    {
      replace_statement: {
        statement: next.site,
        with: { finish: { value: source.expression } },
      },
    },
  ]);
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

test("promotion cannot snapshot a connected or computed While condition outside its loop", () => {
  const unit = { function: "demo" };
  const port = {
    target: { expression: { unit, path: [1, 0] } },
    context: { block: { unit, path: [] }, statement: { unit, path: [1] } },
  };
  const workflow = {
    view: { dataflow: { bindings: [{ id: 0, name: "proceed" }] } },
  };
  const original = structuredClone(workflow);
  for (const value of [
    { variable: "proceed" },
    { call: { function: "condition", args: [] } },
  ]) {
    assert.throws(
      () => planConstantPromotion(workflow, port, value, "condition_value"),
      /Only constants/,
    );
  }
  assert.deepEqual(workflow, original);
  const value = { literal: { bool: true } };
  assert.deepEqual(
    planConstantPromotion(workflow, port, value, "condition_value"),
    [
      {
        insert_statement: {
          at: { block: port.context.block, before: port.context.statement },
          statement: {
            let: { name: "condition_value", value: { expr: value } },
          },
        },
      },
      {
        replace_expression: {
          expression: port.target.expression,
          with: { variable: "condition_value" },
        },
      },
    ],
  );
});
