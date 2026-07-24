import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import vm from "node:vm";

const html = readFileSync(
  new URL("../assets/index.html", import.meta.url),
  "utf8",
);
const source = html.match(
  /\/\/ BEGIN WORKBENCH_PROJECTION_STATE([\s\S]*?)\/\/ END WORKBENCH_PROJECTION_STATE/,
)?.[1];
assert.ok(source, "production projection state block is missing");

const context = { Set };
vm.runInNewContext(
  `${source}
  this.projectionExports = {
    createWorkbenchProjectionState,
    beginStateRecovery,
    recoveryResponseIsCurrent,
    applyProjectionSnapshot
  };`,
  context,
);
const {
  createWorkbenchProjectionState,
  beginStateRecovery,
  recoveryResponseIsCurrent,
  applyProjectionSnapshot,
} = context.projectionExports;

function markedSource(begin, end) {
  const block = html.match(
    new RegExp(`// BEGIN ${begin}([\\s\\S]*?)// END ${end}`),
  )?.[1];
  assert.ok(block, `production ${begin} block is missing`);
  return block;
}

function snapshot(sessionId, cursor, eventIds = []) {
  return {
    settings: { session_id: sessionId },
    observation: { cursor: `observation-${sessionId}-${cursor}` },
    product_events: {
      cursor,
      events: eventIds.map((event_id, index) => ({
        event_id,
        sequence: index + 1,
      })),
    },
  };
}

test("a snapshot overtaken by a live event cannot erase its row", () => {
  const projection = createWorkbenchProjectionState();
  applyProjectionSnapshot(projection, snapshot("session-a", 1, ["initial"]));
  const token = beginStateRecovery(projection);

  const rows = new Set(["initial"]);
  projection.productCursor = 2;
  projection.renderedProductEvents.add("live-row");
  rows.add("live-row");

  const stale = snapshot("session-a", 1, ["initial"]);
  if (recoveryResponseIsCurrent(projection, token, stale)) {
    rows.clear();
    for (const event of stale.product_events.events) rows.add(event.event_id);
    applyProjectionSnapshot(projection, stale, true);
  }

  assert.deepEqual([...rows], ["initial", "live-row"]);
  assert.equal(projection.productCursor, 2);
  assert.equal(projection.renderedProductEvents.has("live-row"), true);
});

test("an older recovery response cannot land after a newer recovery", () => {
  const projection = createWorkbenchProjectionState();
  applyProjectionSnapshot(projection, snapshot("session-a", 1));
  const older = beginStateRecovery(projection);
  const newer = beginStateRecovery(projection);

  assert.equal(
    recoveryResponseIsCurrent(projection, older, snapshot("session-a", 3)),
    false,
  );
  assert.equal(
    recoveryResponseIsCurrent(projection, newer, snapshot("session-a", 3)),
    true,
  );
});

test("authoritative replacement rebuilds both dedup sets and rewinds its cursor", () => {
  const projection = createWorkbenchProjectionState();
  applyProjectionSnapshot(projection, snapshot("session-a", 9, ["old"]));
  projection.appliedObservationEvents.add("old-observation");

  applyProjectionSnapshot(
    projection,
    snapshot("session-a", 4, ["snapshot-event"]),
    true,
  );

  assert.equal(projection.productCursor, 4);
  assert.deepEqual([...projection.renderedProductEvents], ["snapshot-event"]);
  assert.deepEqual([...projection.appliedObservationEvents], []);
});

test("a new session never inherits another session's cursors or identities", () => {
  const projection = createWorkbenchProjectionState();
  applyProjectionSnapshot(projection, snapshot("session-a", 20, ["old"]));
  projection.appliedObservationEvents.add("old-observation");

  applyProjectionSnapshot(projection, snapshot("session-b", 0));

  assert.equal(projection.sessionId, "session-b");
  assert.equal(projection.productCursor, 0);
  assert.equal(projection.observationCursor, "observation-session-b-0");
  assert.deepEqual([...projection.renderedProductEvents], []);
  assert.deepEqual([...projection.appliedObservationEvents], []);
});

test("a Done product event behaviorally retracts the provisional draft", () => {
  let draftRemoved = false;
  let busy = true;
  const reducerContext = {
    Set,
    projectionState: createWorkbenchProjectionState(),
    renderedProductEvents: new Set(),
    assistantDraft: {
      closest() {
        return {
          remove() {
            draftRemoved = true;
          },
        };
      },
    },
    assistantDraftText: "provisional text",
    assistantDraftChunks: [{ text: "provisional text" }],
    pendingTools: [],
    appendTool() {},
    reasoningChunks: [],
    pendingCodeBlock: {},
    reasoning: {},
    renderMessage() {},
    renderIngressReceipt() {},
    setBusy(value) {
      busy = value;
    },
    refreshUsage() {},
  };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_TRANSIENT_SETTLEMENT", "WORKBENCH_TRANSIENT_SETTLEMENT")}
     ${markedSource("WORKBENCH_PRODUCT_EVENT_REDUCER", "WORKBENCH_PRODUCT_EVENT_REDUCER")}
     applyProductEvent({ event_id: "cancel-done", sequence: 1, type: "done" });
     this.result = { assistantDraft, assistantDraftText, assistantDraftChunks };`,
    reducerContext,
  );

  assert.equal(draftRemoved, true);
  assert.equal(busy, false);
  assert.equal(reducerContext.result.assistantDraft, null);
  assert.equal(reducerContext.result.assistantDraftText, "");
  assert.deepEqual([...reducerContext.result.assistantDraftChunks], []);
});
