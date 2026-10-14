import test from "node:test";
import assert from "node:assert/strict";
import { approvalControl } from "./authoring-approvals.mjs";

function fixture() {
  const button = { hidden: true, disabled: false, onclick: null };
  const requests = [];
  const errors = [];
  const control = approvalControl(
    button,
    (key) =>
      new Promise((resolve, reject) => requests.push({ key, resolve, reject })),
    (error) => errors.push(error),
  );
  return { button, requests, errors, control };
}

test("every simultaneous approval remains actionable until its own response succeeds", async () => {
  const { button, requests, control } = fixture();
  control.add("A");
  control.add("B");
  control.add("A");
  const first = button.onclick();
  assert.equal(requests[0].key, "A");
  assert.equal(button.disabled, true);
  await button.onclick();
  assert.equal(requests.length, 1);
  requests[0].resolve();
  await first;
  assert.equal(button.hidden, false);
  const second = button.onclick();
  assert.equal(requests[1].key, "B");
  requests[1].resolve();
  await second;
  assert.equal(button.hidden, true);
  assert.equal(button.onclick, null);
  control.add("A");
  control.add("B");
  assert.equal(button.hidden, true);
});

test("an approval response cannot hide a newer wait received while it was pending", async () => {
  const { button, requests, control } = fixture();
  control.add("A");
  const first = button.onclick();
  control.add("B");
  requests[0].resolve();
  await first;
  assert.equal(button.hidden, false);
  const second = button.onclick();
  assert.equal(requests[1].key, "B");
  requests[1].resolve();
  await second;
});

test("reset retires old handlers and in-flight responses even when a key is reused", async () => {
  const { button, requests, control } = fixture();
  control.add("A");
  const stale = button.onclick;
  control.reset();
  const ignored = stale();
  assert.equal(requests.length, 0);
  await ignored;

  control.add("A");
  const old = button.onclick();
  control.reset();
  control.add("A");
  requests[0].resolve();
  await old;
  assert.equal(button.hidden, false);
  const current = button.onclick();
  assert.equal(requests.length, 2);
  requests[1].resolve();
  await current;
});
