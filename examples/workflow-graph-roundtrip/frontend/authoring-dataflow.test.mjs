import test from "node:test";
import assert from "node:assert/strict";
import {
  encodeValue,
  constantValue,
  planEndAfter,
} from "./authoring-dataflow.mjs";
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
  const workflow = { statements: [s, next] };
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
