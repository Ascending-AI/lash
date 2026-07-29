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
    applyProjectionSnapshot,
    busyAfterStateSnapshot
  };`,
  context,
);
const {
  createWorkbenchProjectionState,
  beginStateRecovery,
  recoveryResponseIsCurrent,
  applyProjectionSnapshot,
  busyAfterStateSnapshot,
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

test("an authoritative settled snapshot clears a busy projection even when Done is pre-applied", () => {
  const settled = {
    ...snapshot("session-a", 3, ["turn-done"]),
    active_turns: [],
  };

  assert.equal(busyAfterStateSnapshot(settled, true, true), false);
  assert.equal(
    busyAfterStateSnapshot({ ...settled, active_turns: [{ turn_id: "active" }] }, true, false),
    true,
  );
});

test("trigger registration controls use the payload subscription key", () => {
  assert.doesNotMatch(html, /registration\.handle/);
  assert.match(html, /registration\.subscription_key/);
  assert.match(html, /dataset\.triggerSubscriptionKey/);
});

test("trigger registration rows separate display name, identity, and trigger key", () => {
  const subscriptionIdA =
    "trigger-subscription:v1:sha256:1bab983f42000000000000000000000000000000000000000000000000000000";
  const subscriptionIdB =
    "trigger-subscription:v1:sha256:9c4d0a71ee000000000000000000000000000000000000000000000000000000";
  const rowContext = {};
  vm.runInNewContext(
    `${markedSource(
      "WORKBENCH_TRIGGER_REGISTRATION_PROJECTION",
      "WORKBENCH_TRIGGER_REGISTRATION_PROJECTION",
    )}
     const shared = {
       name: "shared-blue-watch",
       source_type: "cron.Schedule",
       source: {
         $lash_host_descriptor_type: "cron.Schedule",
         $lash_host_descriptor_value: { expr: "*/2 * * * * *", tz: "UTC" }
       },
       target: { label: "mirror_job", identity: { label: "ignored-fallback" } },
       subscription_key: "derived/v1/content-address",
       incarnation: "incarnation-a"
     };
     this.rows = [
       triggerRegistrationRowModel({
         ...shared,
         subscription_id: ${JSON.stringify(subscriptionIdA)},
         registrant_scope: "session:session-a"
       }),
       triggerRegistrationRowModel({
         ...shared,
         subscription_id: ${JSON.stringify(subscriptionIdB)},
         registrant_scope: "session:session-b"
       })
     ];`,
    rowContext,
  );

  assert.match(subscriptionIdA, /^trigger-subscription:v1:sha256:[0-9a-f]{64}$/);
  assert.match(subscriptionIdB, /^trigger-subscription:v1:sha256:[0-9a-f]{64}$/);
  assert.equal(rowContext.rows[0].name, "mirror_job ← cron.Schedule (every 2s)");
  assert.equal(rowContext.rows[1].name, rowContext.rows[0].name);
  assert.equal(
    rowContext.rows[0].detail,
    "id sha256:1bab983f42… · trigger key derived/v1/c… · scope session:session-a · incarnation incarnation-…",
  );
  assert.equal(
    rowContext.rows[0].title,
    `id ${subscriptionIdA} · trigger key derived/v1/content-address · scope session:session-a · alias shared-blue-watch · incarnation incarnation-a`,
  );
  assert.doesNotMatch(rowContext.rows[0].name, /shared-blue-watch/);
  assert.notEqual(
    rowContext.rows[0].detail.match(/^id ([^·]+)/)?.[1],
    rowContext.rows[1].detail.match(/^id ([^·]+)/)?.[1],
    "same-name, same-key registrations must render visibly distinct ids",
  );
});

test("trigger registration names preserve raw fallbacks and omit empty summaries", () => {
  const fallbackContext = {};
  vm.runInNewContext(
    `${markedSource(
      "WORKBENCH_TRIGGER_REGISTRATION_PROJECTION",
      "WORKBENCH_TRIGGER_REGISTRATION_PROJECTION",
    )}
     const base = {
       subscription_id: "trigger-subscription:v1:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
       subscription_key: "literal-key",
       registrant_scope: "host:calendar",
       incarnation: "9f2a5950-7fe8-4b51-b1d4-c47e43697b89"
     };
     this.raw = triggerRegistrationRowModel({
       ...base,
       source_type: "future.Schedule",
       source: { $lash_host_descriptor_value: { expr: "rate(5m)" } },
       target: { identity: { label: "fallback_job" } }
     });
     this.empty = triggerRegistrationRowModel({
       ...base,
       source_type: "ui.button.pressed",
       source: {},
       target: {}
     });
     this.generic = triggerRegistrationRowModel({
       ...base,
       source_type: "future.Source",
       source: { first: 1, second: true, third: "visible-truncation" },
       target: { label: "generic_job" }
     });`,
    fallbackContext,
  );

  assert.equal(
    fallbackContext.raw.name,
    "fallback_job ← future.Schedule (rate(5m))",
  );
  assert.match(fallbackContext.raw.detail, /scope host:calendar/);
  assert.equal(fallbackContext.empty.name, "process ← ui.button.pressed");
  assert.doesNotMatch(fallbackContext.empty.name, /\(/);
  assert.equal(
    fallbackContext.generic.name,
    "generic_job ← future.Source (first 1 · second true · …)",
  );
});

test("renderTriggers wires the projected name and detail into the rail", () => {
  function element(tagName) {
    return {
      tagName,
      children: [],
      dataset: {},
      append(...children) {
        this.children.push(...children);
      },
      appendChild(child) {
        this.children.push(child);
      },
      addEventListener() {},
    };
  }

  const triggerCount = element("span");
  const triggerRegistrations = element("div");
  const renderContext = {
    document: { createElement: element },
    triggerCount,
    triggerRegistrations,
    setTriggerEnabled() {},
    deleteTrigger() {},
  };
  vm.runInNewContext(
    `${markedSource(
      "WORKBENCH_TRIGGER_REGISTRATION_PROJECTION",
      "WORKBENCH_TRIGGER_REGISTRATION_PROJECTION",
    )}
     ${markedSource(
       "WORKBENCH_TRIGGER_REGISTRATION_RENDERING",
       "WORKBENCH_TRIGGER_REGISTRATION_RENDERING",
     )}
     renderTriggers([{
       enabled: true,
       source_type: "cron.Schedule",
       source: { $lash_host_descriptor_value: { expr: "*/2 * * * * *" } },
       target: { label: "wired_job" },
       subscription_id: "trigger-subscription:v1:sha256:1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
       subscription_key: "wired-key",
       registrant_scope: "session:wired-session",
       incarnation: "wired-incarnation"
     }]);`,
    renderContext,
  );

  const row = triggerRegistrations.children[0];
  assert.equal(triggerCount.textContent, "1");
  assert.equal(row.children[0].textContent, "wired_job ← cron.Schedule (every 2s)");
  assert.match(row.children[1].textContent, /^id sha256:1234567890…/);
  assert.match(row.children[1].title, /trigger key wired-key/);
});

test("settled transcript rendering consumes durable reasoning and code disclosure", () => {
  const rendered = [];
  const renderContext = {
    renderMessage(message) {
      rendered.push(["message", message.id]);
    },
    appendReasoning(text, id, turnId) {
      rendered.push(["reasoning", id, text, turnId]);
    },
    appendCodeBlock(row) {
      rendered.push(["code_block", row.id, row.code, row.output]);
    },
  };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_SETTLED_TRANSCRIPT", "WORKBENCH_SETTLED_TRANSCRIPT")}
     renderStateTranscript({
       transcript: [
         { type: "message", message: { id: "committed-user" } },
         { type: "reasoning", id: "reasoning-1", text: "durable thought" },
         {
           type: "code_block",
           id: "code-1",
           code: "print(\\"durable\\")",
           output: "durable"
         }
       ]
     });`,
    renderContext,
  );

  assert.deepEqual(
    rendered,
    [
      ["message", "committed-user"],
      ["reasoning", "reasoning-1", "durable thought", null],
      ["code_block", "code-1", 'print("durable")', "durable"],
    ],
  );
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
    assistantDraftTurnId: "cancel-turn",
    assistantDraftText: "provisional text",
    assistantDraftChunks: [
      { turnId: "cancel-turn", text: "provisional text" },
    ],
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
     applyProductEvent({
       event_id: "cancel-done",
       sequence: 1,
       type: "done",
       turn_id: "cancel-turn"
     });
     this.result = {
       assistantDraft,
       assistantDraftTurnId,
       assistantDraftText,
       assistantDraftChunks
     };`,
    reducerContext,
  );

  assert.equal(draftRemoved, true);
  assert.equal(busy, false);
  assert.equal(reducerContext.result.assistantDraft, null);
  assert.equal(reducerContext.result.assistantDraftTurnId, null);
  assert.equal(reducerContext.result.assistantDraftText, "");
  assert.deepEqual([...reducerContext.result.assistantDraftChunks], []);
});

test("turn A Done does not retract turn B provisional prose", () => {
  let draftRemoved = false;
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
    assistantDraftTurnId: "turn-b",
    assistantDraftText: "turn B provisional text",
    assistantDraftChunks: [
      {
        turnId: "turn-b",
        correlationId: "turn-b-prose",
        text: "turn B provisional text",
      },
    ],
    pendingTools: [],
    appendTool() {},
    reasoningChunks: [],
    pendingCodeBlock: null,
    reasoning: null,
    renderMessage() {},
    renderIngressReceipt() {},
    setBusy() {},
    refreshUsage() {},
  };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_TRANSIENT_SETTLEMENT", "WORKBENCH_TRANSIENT_SETTLEMENT")}
     ${markedSource("WORKBENCH_PRODUCT_EVENT_REDUCER", "WORKBENCH_PRODUCT_EVENT_REDUCER")}
     applyProductEvent({
       event_id: "turn-a-done",
       sequence: 1,
       type: "done",
       turn_id: "turn-a"
     });
     this.result = {
       assistantDraft,
       assistantDraftTurnId,
       assistantDraftText,
       assistantDraftChunks
     };`,
    reducerContext,
  );

  assert.equal(draftRemoved, false);
  assert.equal(reducerContext.result.assistantDraftTurnId, "turn-b");
  assert.equal(reducerContext.result.assistantDraftText, "turn B provisional text");
  assert.deepEqual(
    [...reducerContext.result.assistantDraftChunks].map((chunk) => ({
      turnId: chunk.turnId,
      text: chunk.text,
    })),
    [{ turnId: "turn-b", text: "turn B provisional text" }],
  );
});
