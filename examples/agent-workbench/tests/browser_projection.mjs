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

const triggerIdentities = JSON.parse(
  process.env.LASH_WORKBENCH_TRIGGER_IDENTITIES ?? "null",
);
assert.ok(
  triggerIdentities,
  "LASH_WORKBENCH_TRIGGER_IDENTITIES must come from the Rust projection gate",
);

function expectedSubscriptionIdDetail(value) {
  const prefix = "trigger-subscription:v2:sha256:";
  assert.match(value, /^trigger-subscription:v2:sha256:[0-9a-f]{64}$/);
  return `sha256:${value.slice(prefix.length, prefix.length + 10)}…`;
}

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

/* FIG-791: the shell may only claim "idle" / "no turns yet" from a snapshot it
   actually received. These run the production availability state machine and
   its renderer against stub elements, so the assertions are about what the DOM
   says, not about the internal phase name. */
function shellModule() {
  const context = { Object, Number, String, Boolean };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_SHELL_AVAILABILITY", "WORKBENCH_SHELL_AVAILABILITY")}
     this.exports = {
       createShellAvailability,
       markShellChannel,
       markShellHydrated,
       shellPhase,
       shellStatusModel,
       snapshotApplication,
       timelinePlaceholder
     };`,
    context,
  );
  return context.exports;
}

function shellRender(model) {
  function element(initial = {}) {
    return { textContent: "", className: "", hidden: false, ...initial };
  }
  const elements = {
    busyText: element(),
    busyPill: element({ className: "pill pending" }),
    streamState: element(),
    sessionId: element(),
    webState: element(),
    shellStatus: element({ hidden: true }),
    shellStatusText: element(),
    shellStatusDetail: element({ hidden: true }),
    timelineEmpty: element({ className: "empty pending" }),
  };
  // The context deliberately withholds every handle to transcript content —
  // `timeline`, `clearTranscript`, `renderError`, `renderNote`. A renderer that
  // reached for one to express a degraded state would throw a ReferenceError
  // here, which is what makes "a connection change never touches content" a
  // tested property rather than an intention.
  const renderContext = {
    ...elements,
    document: {
      getElementById(id) {
        return id === "timelineEmpty" ? elements.timelineEmpty : null;
      },
    },
  };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_SHELL_STATUS_RENDER", "WORKBENCH_SHELL_STATUS_RENDER")}
     applyShellStatus(${JSON.stringify(model)});`,
    renderContext,
  );
  return {
    pill: elements.busyText.textContent,
    pillClass: elements.busyPill.className,
    subtitle: elements.streamState.textContent,
    session: elements.sessionId.textContent,
    web: elements.webState.textContent,
    bannerHidden: elements.shellStatus.hidden,
    banner: elements.shellStatusText.textContent,
    bannerDetail: elements.shellStatusDetail.textContent,
    placeholder: elements.timelineEmpty.textContent,
    placeholderClass: elements.timelineEmpty.className,
  };
}

test("the shipped markup ships no session claim of its own", () => {
  // The byte-identical outage screenshot in FIG-791 was the pristine, never
  // hydrated shell: every claim on it came from static HTML, not from a
  // response. The static shell must therefore claim nothing.
  const shell = html.slice(html.indexOf("<body"));
  assert.doesNotMatch(shell, /id="timelineEmpty"[^>]*>\s*no turns yet/);
  assert.match(shell, /id="timelineEmpty"[^>]*>\s*connecting to the workbench…/);
  assert.match(shell, /id="busyText"[^>]*>connecting</);
  assert.match(shell, /id="sessionId"[^>]*>connecting…</);
  assert.doesNotMatch(shell, /id="busyText"[^>]*>idle</);
});

test("the pre-hydration shell reports its connection, not an empty session", () => {
  const shell = shellModule();
  const render = shellRender(
    shell.shellStatusModel(shell.createShellAvailability(), {}),
  );

  assert.equal(render.pill, "connecting");
  assert.doesNotMatch(render.placeholder, /no turns yet/);
  assert.match(render.placeholder, /connecting/);
  assert.equal(render.session, "connecting…");
  assert.notEqual(render.pill, "idle");
});

test("a failed /api/state is a visibly different render from an empty session", () => {
  const shell = shellModule();
  const outage = shell.markShellChannel(
    shell.createShellAvailability(),
    "state",
    false,
    "the workbench did not answer within 5s",
  );
  const settledEmpty = shell.markShellHydrated(shell.createShellAvailability());

  const outageRender = shellRender(shell.shellStatusModel(outage, {}));
  const emptyRender = shellRender(
    shell.shellStatusModel(settledEmpty, { session: "workbench-a", web: "ready" }),
  );

  // The defect this replaces: two different situations rendering the same shell.
  assert.notDeepEqual(outageRender, emptyRender);
  assert.notEqual(outageRender.pill, emptyRender.pill);
  assert.notEqual(outageRender.placeholder, emptyRender.placeholder);
  assert.notEqual(outageRender.bannerHidden, emptyRender.bannerHidden);
  assert.notEqual(outageRender.placeholderClass, emptyRender.placeholderClass);

  assert.equal(emptyRender.pill, "idle");
  assert.match(emptyRender.placeholder, /^no turns yet/);
  assert.equal(emptyRender.bannerHidden, true);

  assert.notEqual(outageRender.pill, "idle");
  assert.doesNotMatch(outageRender.placeholder, /no turns yet/);
  assert.equal(outageRender.bannerHidden, false);
  assert.match(outageRender.banner, /unreachable/);
  assert.equal(outageRender.bannerDetail, "the workbench did not answer within 5s");
  assert.equal(outageRender.session, "unknown");
});

test("a drop after hydration reconnects over the last known content", () => {
  const shell = shellModule();
  const availability = shell.markShellChannel(
    shell.markShellHydrated(shell.createShellAvailability()),
    "product",
    false,
    "transcript stream disconnected",
  );

  const render = shellRender(
    shell.shellStatusModel(availability, {
      session: "workbench-a",
      web: "ready",
      busy: true,
    }),
  );

  // Last-known-good content survives, identity included: a reconnect states
  // that the view may be stale, it does not retract the session. That the
  // renderer cannot reach transcript content at all is enforced by the stub
  // context above; here we assert it does not retract the identity either.
  const renderSource = markedSource(
    "WORKBENCH_SHELL_STATUS_RENDER",
    "WORKBENCH_SHELL_STATUS_RENDER",
  );
  assert.doesNotMatch(renderSource, /innerHTML|clearTranscript|timeline\.|renderError/);
  assert.equal(render.session, "workbench-a");
  assert.equal(render.web, "ready");
  assert.equal(render.pill, "reconnecting");
  assert.equal(render.bannerHidden, false);
  assert.match(render.banner, /live updates paused/);
  assert.match(render.subtitle, /a turn was running/);
  assert.doesNotMatch(render.placeholder, /no turns yet/);

  // A snapshot channel that is also down changes the claim about the content.
  const stateDown = shell.markShellChannel(
    availability,
    "state",
    false,
    "state request failed (503)",
  );
  const stateDownRender = shellRender(shell.shellStatusModel(stateDown, { session: "workbench-a" }));
  assert.match(stateDownRender.banner, /last known state/);
  assert.equal(stateDownRender.bannerDetail, "state request failed (503)");
});

test("a successful response is what promotes the shell to session claims", () => {
  const shell = shellModule();
  const availability = shell.createShellAvailability();
  assert.equal(shell.shellPhase(availability), "connecting");

  shell.markShellChannel(availability, "state", false, "boot failure");
  assert.equal(shell.shellPhase(availability), "unavailable");

  shell.markShellChannel(availability, "state", true);
  shell.markShellHydrated(availability);
  assert.equal(shell.shellPhase(availability), "live");

  const render = shellRender(
    shell.shellStatusModel(availability, { session: "workbench-a", web: "ready" }),
  );
  assert.equal(render.pill, "idle");
  assert.equal(render.pillClass, "pill");
  assert.equal(render.session, "workbench-a");
  assert.equal(render.bannerHidden, true);
  assert.equal(render.placeholderClass, "empty");
  assert.equal(render.placeholder, shell.timelinePlaceholder("live"));

  // Only "live" may say it.
  for (const phase of ["connecting", "unavailable", "reconnecting"]) {
    assert.doesNotMatch(shell.timelinePlaceholder(phase), /no turns yet/);
  }
});

test("a late snapshot replaces the streams' rows without erasing newer ones", () => {
  const shell = shellModule();
  const fresh = shell.createShellAvailability();
  const hydrated = shell.markShellHydrated(shell.createShellAvailability());
  const latest = { isLatestRequest: true };

  // Nothing has rendered before the first stream starts.
  assert.equal(
    shell.snapshotApplication(fresh, { ...latest, streamsStarted: false, responseIsCurrent: true }),
    "initial",
  );
  assert.equal(
    shell.snapshotApplication(fresh, { ...latest, streamsStarted: false, responseIsCurrent: false }),
    "initial",
  );

  // A hydration that lands after the streams started replaces their rows:
  // reasoning and code rows carry no id dedup, so appending would double them.
  assert.equal(
    shell.snapshotApplication(fresh, { ...latest, streamsStarted: true, responseIsCurrent: false }),
    "authoritative",
  );
  assert.equal(
    shell.snapshotApplication(hydrated, { ...latest, streamsStarted: true, responseIsCurrent: true }),
    "authoritative",
  );

  // But once a snapshot has been applied, a response behind the live projection
  // may not erase rows it never saw — the existing recovery guard still rules.
  assert.equal(
    shell.snapshotApplication(hydrated, { ...latest, streamsStarted: true, responseIsCurrent: false }),
    "ignore",
  );

  // The retry button, the backoff timer and a reset can all have a request in
  // flight at once. A response that is no longer the newest request is dropped
  // whatever else is true of it — including before hydration, where the
  // recovery guard has no session to compare and cannot speak.
  for (const availability of [fresh, hydrated]) {
    for (const streamsStarted of [false, true]) {
      for (const responseIsCurrent of [false, true]) {
        assert.equal(
          shell.snapshotApplication(availability, {
            isLatestRequest: false,
            streamsStarted,
            responseIsCurrent,
          }),
          "ignore",
        );
      }
    }
  }
});

test("an unattached stream is neither a live channel nor an outage", () => {
  const shell = shellModule();

  // Born unknown: a stream that has not attached yet is not evidence of an
  // outage, so a fresh shell is "connecting", not "unavailable".
  const fresh = shell.createShellAvailability();
  assert.equal(fresh.channels.product, null);
  assert.equal(shell.shellPhase(fresh), "connecting");

  // …but it is not evidence of liveness either. The connect watchdog turns a
  // stream that never lands into a known-down channel, and the shell stops
  // claiming the session is idle.
  const stuck = shell.markShellChannel(
    shell.markShellHydrated(shell.createShellAvailability()),
    "product",
    false,
    "the transcript stream is not connecting",
  );
  assert.equal(shell.shellPhase(stuck), "reconnecting");
  const render = shellRender(shell.shellStatusModel(stuck, { session: "workbench-a" }));
  assert.notEqual(render.pill, "idle");
  assert.equal(render.bannerDetail, "the transcript stream is not connecting");

  // A stream that ends on its own is down until the next attempt re-establishes
  // it: the shell must not keep claiming liveness through a silent close.
  assert.match(html, /markShellChannel\(shellAvailability, "product", false, "the transcript stream closed"\)/);
  assert.match(html, /const STREAM_CONNECT_TIMEOUT_MS = \d+;/);
});

test("the boot path bounds its snapshot request and retries it", () => {
  // The non-determinism in FIG-791: an unbounded, un-retried one-shot left a
  // reload during the outage on whatever the static markup said, forever, when
  // the backend accepted the connection and blocked instead of refusing it.
  assert.match(html, /AbortSignal\.timeout\(timeoutMs\)/);
  assert.match(html, /function scheduleStateRetry\(\)/);
  assert.doesNotMatch(html, /renderNote\("transcript updates reconnecting"\)/);
});

test("trigger registration controls use the payload subscription key", () => {
  assert.doesNotMatch(html, /registration\.handle/);
  assert.match(html, /registration\.subscription_key/);
  assert.match(html, /dataset\.triggerSubscriptionKey/);
});

test("trigger registration rows separate display name, identity, and trigger key", () => {
  const subscriptionIdA = triggerIdentities.session_a;
  const subscriptionIdB = triggerIdentities.session_b;
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
       subscription_key: "derived/v2/content-address",
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

  assert.match(subscriptionIdA, /^trigger-subscription:v2:sha256:[0-9a-f]{64}$/);
  assert.match(subscriptionIdB, /^trigger-subscription:v2:sha256:[0-9a-f]{64}$/);
  assert.equal(rowContext.rows[0].name, "mirror_job ← cron.Schedule (every 2s)");
  assert.equal(rowContext.rows[1].name, rowContext.rows[0].name);
  assert.equal(
    rowContext.rows[0].detail,
    `id ${expectedSubscriptionIdDetail(subscriptionIdA)} · trigger key v2/content-ad… · scope session:session-a · incarnation incarnation-…`,
  );
  assert.equal(
    rowContext.rows[0].title,
    `id ${subscriptionIdA} · trigger key derived/v2/content-address · scope session:session-a · alias shared-blue-watch · incarnation incarnation-a`,
  );
  assert.doesNotMatch(rowContext.rows[0].name, /shared-blue-watch/);
  assert.notEqual(
    rowContext.rows[0].detail.match(/^id ([^·]+)/)?.[1],
    rowContext.rows[1].detail.match(/^id ([^·]+)/)?.[1],
    "same-name, same-key registrations must render visibly distinct ids",
  );
});

test("trigger detail truncation keeps the distinguishing suffix of namespaced values", () => {
  const context = {};
  vm.runInNewContext(
    `${markedSource(
      "WORKBENCH_TRIGGER_REGISTRATION_PROJECTION",
      "WORKBENCH_TRIGGER_REGISTRATION_PROJECTION",
    )}
     this.scopeA = truncateTriggerScope("session:workbench-0f3a9d2c-aaaa-bbbb-cccc-111111111111");
     this.scopeB = truncateTriggerScope("session:workbench-0f3a9d2c-aaaa-bbbb-cccc-222222222222");
     this.hostScope = truncateTriggerScope("host");
     this.futureId = truncateSubscriptionId(
       "trigger-subscription:v3:sha256:9c4d0a71ee000000000000000000000000000000000000000000000000feedbeef"
     );
     this.rawCronElsewhere = triggerRegistrationSourceSummary({
       source_type: "timer.Schedule",
       source: {
         $lash_host_descriptor_type: "timer.Schedule",
         $lash_host_descriptor_value: { expr: "*/2 * * * * *" }
       }
     });`,
    context,
  );

  // Same-prefix session scopes must stay visibly distinct after truncation.
  assert.match(context.scopeA, /^session:…/);
  assert.notEqual(context.scopeA, context.scopeB, "same-prefix scopes must render distinct tails");
  assert.equal(context.hostScope, "host");
  // An unrecognized (future-versioned) id keeps its distinguishing digest tail,
  // never a constant head — the FIG-774 zero-bit truncation must not return.
  assert.match(context.futureId, /feedbeef…?$|…feedbeef$/);
  // The seconds-cron compaction is gated on cron.Schedule: other sources render raw.
  assert.equal(context.rawCronElsewhere, "*/2 * * * * *");
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
       subscription_id: ${JSON.stringify(triggerIdentities.wired)},
       subscription_key: "wired-key",
       registrant_scope: "session:wired-session",
       incarnation: "wired-incarnation"
     }]);`,
    renderContext,
  );

  const row = triggerRegistrations.children[0];
  assert.equal(triggerCount.textContent, "1");
  assert.equal(row.children[0].textContent, "wired_job ← cron.Schedule (every 2s)");
  assert.match(
    row.children[1].textContent,
    new RegExp(`^id ${expectedSubscriptionIdDetail(triggerIdentities.wired)}`),
  );
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
