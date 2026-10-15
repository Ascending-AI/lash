import test from "node:test";
import assert from "node:assert/strict";
import { NOTHING, close, escape, open, select } from "./selection.mjs";

test("a click selects without opening, and a click elsewhere closes what was open", () => {
  const selected = select(NOTHING, "a");
  assert.deepEqual(selected, { selected: "a", open: false });
  const opened = open(selected);
  assert.deepEqual(opened, { selected: "a", open: true });
  assert.equal(select(opened, "a"), opened, "re-clicking the open item keeps it open");
  assert.deepEqual(select(opened, "b"), { selected: "b", open: false });
});

test("a double-click or Enter opens; Escape closes first, then clears", () => {
  const opened = open(NOTHING, "a");
  assert.deepEqual(opened, { selected: "a", open: true });
  const closed = escape(opened);
  assert.deepEqual(closed, { selected: "a", open: false });
  assert.equal(escape(closed), NOTHING);
  assert.deepEqual(close(opened), closed);
});
