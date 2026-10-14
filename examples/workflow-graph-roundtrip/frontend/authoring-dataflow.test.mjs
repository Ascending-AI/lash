import test from "node:test";
import assert from "node:assert/strict";
import { encodeValue, constantValue } from "./authoring-dataflow.mjs";
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
