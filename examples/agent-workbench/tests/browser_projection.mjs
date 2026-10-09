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

const stopTerminal = JSON.parse(process.env.LASH_WORKBENCH_STOP_TERMINAL ?? "null");
assert.ok(stopTerminal, "Stop terminal must come from the Rust projection gate");
const stopReceipt = { accepted: true, cancellations: [{ terminal: stopTerminal }] };

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

// FIG-5036: a committed Stop is reported once, by the shared event row every
// viewer of the session receives. The stopping tab used to add a local note
// with the same request evidence, so it saw the stop twice.
test("a committed Stop adds no local duplicate of the shared stop row", async () => {
  const notes = [];
  const errors = [];
  const stopContext = {
    controller: null,
    resetInFlight: false,
    async fetch(url, options) {
      assert.equal(url, "/api/turn/cancel?mode=stop");
      assert.equal(options.method, "POST");
      return {
        ok: true,
        async json() { return stopReceipt; },
      };
    },
    armStopEscalation() {},
    renderNote(message) { notes.push(message); },
    renderError(message) { errors.push(message); },
  };

  vm.runInNewContext(
    `${markedSource("WORKBENCH_STOP_TURN", "WORKBENCH_STOP_TURN")}
     this.stopTurn = stopTurn;`,
    stopContext,
  );
  stopContext.stopTurn();
  await new Promise(resolve => setTimeout(resolve, 0));

  assert.deepEqual(notes, []);
  assert.deepEqual(errors, []);
});

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

test("one running process is one row in the work rail", () => {
  // `/api/work` names a process by incarnation and `/api/lash-vm-graphs` names
  // the same process without one, so matching the two key strings de-duplicated
  // nothing: every running process rendered twice, and the duplicate carried the
  // engine's lift digest instead of the declared label and had no cancel control
  // (FIG-3145).
  const processId = "tool-intent:v2:blake3:ae0a64ab83bc1fd36";
  const context = {
    Set,
    Map,
    String,
    Boolean,
    kindLabel: kind => String(kind),
    formatTime: () => "",
    shortId: value => String(value).slice(0, 8),
    graphIndexByKey: new Map([["process:" + processId, { node_count: 7 }]]),
  };
  const rows = vm.runInNewContext(
    `${markedSource("WORKBENCH_EXECUTION_ROWS", "WORKBENCH_EXECUTION_ROWS")}
     executionRows(
       [{
         kind: "process",
         label: "FIG425_cancellable_0915c",
         process: {
           process_id: ${JSON.stringify(processId)},
           graph_key: ${JSON.stringify("process:" + processId + ":incarnation:5")},
           lifecycle: { state: "running" },
           status_label: "running",
           terminal: false,
         },
       }],
       [{
         kind: "process",
         graph_key: ${JSON.stringify("process:" + processId)},
         title: "__process_a05f96c8",
         node_count: 7,
       }],
     );`,
    context,
  );

  assert.equal(rows.length, 1, "a running process must not render twice");
  const [row] = rows;
  assert.equal(row.title, "FIG425_cancellable_0915c", "the surviving row carries the declared label");
  assert.equal(row.process_id, processId, "the surviving row is the one the cancel control is bound to");
  assert.ok(
    !String(row.title).startsWith("__process_"),
    "the engine's lift digest must not be what the rail names the run",
  );
  // The surviving row also inherits what the graph summary would have said,
  // even though the two surfaces key it differently.
  assert.ok(row.meta.includes("7 nodes"), `expected the graph node count in ${row.meta}`);
});

test("a graph-only process still renders, and an incarnation is not needed to match it", () => {
  const rows = vm.runInNewContext(
    `${markedSource("WORKBENCH_EXECUTION_ROWS", "WORKBENCH_EXECUTION_ROWS")}
     executionRows(
       [{ kind: "process", process: { process_id: "in-the-work-api", lifecycle: { state: "running" }, status_label: "running" } }],
       [
         { kind: "process", graph_key: "process:in-the-work-api", title: "__process_dedup_me", node_count: 1 },
         { kind: "process", graph_key: "process:graph-only", title: "__process_keep_me", node_count: 2 },
       ],
     );`,
    {
      Set,
      Map,
      String,
      Boolean,
      kindLabel: kind => String(kind),
      formatTime: () => "",
      shortId: value => String(value).slice(0, 8),
      graphIndexByKey: new Map(),
    },
  );

  // A work item whose surface reported no graph key at all still de-duplicates
  // its graph row, and a process only the graph surface knows about is kept.
  assert.equal(rows.map(row => row.title).join(" | "), "__process_keep_me | in-the-w");
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

test("a new session never inherits another session's cursors", () => {
  const projection = createWorkbenchProjectionState();
  applyProjectionSnapshot(projection, snapshot("session-a", 20, ["old"]));
  projection.replayIncarnationId = "incarnation-a";

  applyProjectionSnapshot(projection, snapshot("session-b", 0));

  assert.equal(projection.sessionId, "session-b");
  assert.equal(projection.productCursor, 0);
  assert.equal(projection.observationCursor, "observation-session-b-0");
  assert.equal(projection.replayIncarnationId, null);
});

// Resident state outside the transcript (the model) changes without a
// commit: the page reads it again, and hands the read to the timeline, which
// upserts it with the epoch the read began at instead of clearing anything.
test("a resident replacement reads the state again without clearing the timeline", async () => {
  const applied = [];
  let resolveSnapshot;
  const page = {
    projectionState: null,
    shellAvailability: {},
    conversation: {
      epoch: () => 41,
      applyObservation() {},
    },
    renderNote() {},
    renderError() {},
    markShellChannel() {},
    renderShellStatus() {},
    fetchStateSnapshot: () => new Promise(resolve => { resolveSnapshot = resolve; }),
    applyStateSnapshot(state, sequence, since) { applied.push([state.settings.model, since]); },
    clearTranscript() { throw new Error("a resident replacement must not clear the timeline"); },
  };
  vm.runInNewContext(
    `${source}
     projectionState = createWorkbenchProjectionState();
     applyProjectionSnapshot(projectionState, { settings: { session_id: "resident-session" }, product_events: { cursor: 0 } });
     ${markedSource("WORKBENCH_REMOTE_STREAM_RECOVERY", "WORKBENCH_REMOTE_STREAM_RECOVERY")}
     this.handleObservationStreamLine = handleObservationStreamLine;`,
    page,
  );
  page.handleObservationStreamLine(JSON.stringify({
    type: "resident_replacement",
    cursor: "cursor-after-resident",
    event: { type: "resident_changed", session_id: "resident-session", replay_incarnation_id: "resident", cursor: "cursor-after-resident" },
  }));
  resolveSnapshot({ settings: { session_id: "resident-session", model: "resident-model" }, product_events: { cursor: 0 } });
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.deepEqual(applied, [["resident-model", 41]]);
});

function shellModule() {
  const context = { Object, Number, String, Boolean };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_SHELL_AVAILABILITY", "WORKBENCH_SHELL_AVAILABILITY")}
     this.exports = {
       createShellAvailability,
       markShellChannel,
       markShellHydrated,
       markShellTerminal,
       markShellReplacing,
       clearShellReplacing,
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
    shellStatus: element({ hidden: true }),
    shellStatusText: element(),
    shellStatusDetail: element({ hidden: true }),
    timelineEmpty: {
      ...element({ className: "empty pending" }),
      children: [],
      replaceChildren(...nodes) {
        this.children = nodes;
        this.textContent = nodes
          .map(node => (typeof node === "string" ? node : node.textContent))
          .join("");
      },
    },
  };
  const views = [];
  // The context deliberately withholds every handle to transcript content —
  // `timeline`, `clearTranscript`, `renderError`, `renderNote`. A renderer that
  // reached for one to express a degraded state would throw a ReferenceError
  // here, which is what makes "a connection change never touches content" a
  // tested property rather than an intention.
  const renderContext = {
    ...elements,
    setView(view) {
      views.push(view);
    },
    document: {
      getElementById(id) {
        return id === "timelineEmpty" ? elements.timelineEmpty : null;
      },
      createElement() {
        const listeners = {};
        return {
          type: "",
          className: "",
          textContent: "",
          addEventListener(type, listener) {
            listeners[type] = listener;
          },
          click() {
            if (listeners.click) listeners.click();
          },
        };
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
    bannerHidden: elements.shellStatus.hidden,
    banner: elements.shellStatusText.textContent,
    bannerDetail: elements.shellStatusDetail.textContent,
    placeholder: elements.timelineEmpty.textContent,
    placeholderClass: elements.timelineEmpty.className,
    placeholderLink:
      elements.timelineEmpty.children.find(node => typeof node !== "string") || null,
    views,
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
    shell.shellStatusModel(settledEmpty, { session: "workbench-a" }),
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

test("a terminal /api/state refusal renders its canonical error and stops retrying", async () => {
  const shell = shellModule();
  const canonical = "session `retired-session` was used and deleted; session ids cannot be reused in this store";
  const errors = [];
  const context = {
    conversation: { epoch: () => 0 },
    Error,
    Math,
    Number,
    String,
    Boolean,
    Set,
    cleanErrorText(message) { return String(message); },
    STATE_REQUEST_TIMEOUT_MS: 5000,
    errors,
  };
  const runtime = vm.runInNewContext(
    `${markedSource("WORKBENCH_PROJECTION_STATE", "WORKBENCH_PROJECTION_STATE")}
     ${markedSource("WORKBENCH_SHELL_AVAILABILITY", "WORKBENCH_SHELL_AVAILABILITY")}
     const projectionState = createWorkbenchProjectionState();
     const shellAvailability = createShellAvailability();
     let streamGeneration = 0;
     let fetchCalls = 0;
     let retryTimers = 0;
     let renderedModel = null;
     function clearTimeout() {}
     function setTimeout() {
       retryTimers += 1;
       return retryTimers;
     }
     const AbortSignal = { timeout() { return undefined; } };
     function fetch() {
       fetchCalls += 1;
       return Promise.resolve({
         ok: false,
         status: 409,
         async json() { return { error: ${JSON.stringify(canonical)} }; },
       });
     }
     function renderShellStatus() {
       renderedModel = shellStatusModel(shellAvailability, {});
     }
     function renderError(message, options) { errors.push([message, options]); }
     function applyStateSnapshot() {}
     function restartEventStreams() {}
     ${markedSource("WORKBENCH_SESSION_RETIREMENT", "WORKBENCH_SESSION_RETIREMENT")}
     const sessionRetirement = createSessionRetirement();
     const scopedSessionId = null;
     let adoptions = 0;
     async function adoptReplacementSession() { adoptions += 1; }
     ${markedSource("WORKBENCH_STATE_FETCH", "WORKBENCH_STATE_FETCH")}
     ${markedSource("WORKBENCH_STATE_RECOVERY", "WORKBENCH_STATE_RECOVERY")}
     ({
       runLoadState: loadState,
       fetchCalls: () => fetchCalls,
       retryTimers: () => retryTimers,
       renderedModel: () => renderedModel,
       terminalDisposition: () => stateFailureDisposition(new StateSnapshotHttpError(409, "terminal")),
       transportDisposition: () => stateFailureDisposition(new Error("connection reset")),
     });`,
    {
      ...context,
      shellStatusModel: shell.shellStatusModel,
    },
  );
  await runtime.runLoadState();

  assert.equal(runtime.fetchCalls(), 1);
  assert.equal(errors.length, 1);
  assert.equal(errors[0][0], canonical);
  assert.equal(errors[0][1].retry, false);
  assert.equal(runtime.retryTimers(), 0, "terminal refusals must not schedule retry backoff");
  assert.equal(runtime.terminalDisposition(), "terminal");
  assert.equal(runtime.transportDisposition(), "transport");
  assert.equal(runtime.renderedModel().phase, "terminal");
  assert.equal(runtime.renderedModel().banner.text, canonical);
  assert.doesNotMatch(runtime.renderedModel().banner.text, /unreachable|retrying/);
});

test("a retirement refusal hands the page over instead of rendering a dead end", async () => {
  const shell = shellModule();
  const scoped = "workbench-0298b733";
  const errors = [];
  const context = {
    conversation: { epoch: () => 0 },
    Error,
    Math,
    Number,
    String,
    Boolean,
    Set,
    Array,
    cleanErrorText(message) { return String(message); },
    STATE_REQUEST_TIMEOUT_MS: 5000,
    errors,
  };
  const runtime = vm.runInNewContext(
    `${markedSource("WORKBENCH_PROJECTION_STATE", "WORKBENCH_PROJECTION_STATE")}
     ${markedSource("WORKBENCH_SHELL_AVAILABILITY", "WORKBENCH_SHELL_AVAILABILITY")}
     ${markedSource("WORKBENCH_SESSION_RETIREMENT", "WORKBENCH_SESSION_RETIREMENT")}
     const projectionState = createWorkbenchProjectionState();
     const shellAvailability = createShellAvailability();
     /* this page was working in the session when the reset retired it: the
        workbench has answered for this id, which is what makes the refusal a
        hand-off rather than the FIG-3154 refusal of an id never held */
     markShellChannel(shellAvailability, "state", true);
     const sessionRetirement = createSessionRetirement();
     const scopedSessionRefusal = createScopedSessionRefusal();
     const scopedSessionId = ${JSON.stringify(scoped)};
     let streamGeneration = 0;
     let retirement = "retiring";
     let adoptions = 0;
     let retryTimers = 0;
     let renderedModel = null;
     function clearTimeout() {}
     function setTimeout() {
       retryTimers += 1;
       return retryTimers;
     }
     const AbortSignal = { timeout() { return undefined; } };
     function fetch() {
       return Promise.resolve({
         ok: false,
         status: 409,
         async json() {
           return {
             error: "session \`" + scopedSessionId + "\` is being deleted",
             session_id: scopedSessionId,
             session_retirement: retirement,
           };
         },
       });
     }
     async function adoptReplacementSession() { adoptions += 1; }
     function renderShellStatus() {
       renderedModel = shellStatusModel(shellAvailability, {});
     }
     function renderError(message, options) { errors.push([message, options]); }
     function applyStateSnapshot() {}
     function restartEventStreams() {}
     function stopEventStreams() {}
     function showSessionError() {}
     ${markedSource("WORKBENCH_STATE_FETCH", "WORKBENCH_STATE_FETCH")}
     ${markedSource("WORKBENCH_STATE_RECOVERY", "WORKBENCH_STATE_RECOVERY")}
     ({
       runLoadState: loadState,
       settle() { retirement = "retired"; },
       adoptions: () => adoptions,
       retryTimers: () => retryTimers,
       renderedModel: () => renderedModel,
       probeIsFutile: path => sessionScopedProbeIsFutile(sessionRetirement, path),
     });`,
    { ...context, shellStatusModel: shell.shellStatusModel },
  );

  // While the delete settles the page waits: no error row, no adoption, and
  // every other rail stops asking a session that can only refuse them.
  await runtime.runLoadState();
  assert.deepEqual(errors, [], "a session being replaced is not an error to render");
  assert.equal(runtime.adoptions(), 0, "a retiring session may still answer its own reset");
  assert.equal(runtime.renderedModel().phase, "replacing");
  assert.equal(runtime.renderedModel().banner.hidden, true);
  assert.equal(runtime.probeIsFutile("/api/observations"), true);
  assert.equal(runtime.probeIsFutile("/api/state"), false);
  assert.ok(runtime.retryTimers() > 0, "the one probe that ends the wait keeps running");

  // Once the delete has settled, this id will never answer again and a second
  // reset is refused too: the page moves itself to a live session.
  runtime.settle();
  await runtime.runLoadState();
  assert.equal(runtime.adoptions(), 1);
  assert.deepEqual(errors, []);
});

/* FIG-3154: opening `/?session_id=<retired-id>` got one 409 carrying the
   store's canonical single-use refusal and the page then retargeted every
   session-scoped call onto the rotated replacement, rendered the replacement's
   id, and painted its transcript, rails and usage — with `.session-error`
   empty and the refusal nowhere in the document. FIG-3136's adoption is a
   hand-off from a session this page was working in; an id it never held has no
   replacement to follow. */
test("a retired id named in the URL is refused on screen, never swapped for a live session", async () => {
  const shell = shellModule();
  const retired = "fig754-retired-0915a";
  const canonical =
    "session store operation failed: session `" + retired +
    "` was used and deleted; session ids cannot be reused in this store";
  const errors = [];
  const context = {
    conversation: { epoch: () => 0 },
    Error,
    Math,
    Number,
    String,
    Boolean,
    Set,
    Array,
    cleanErrorText(message) { return String(message); },
    STATE_REQUEST_TIMEOUT_MS: 5000,
    errors,
  };
  const runtime = vm.runInNewContext(
    `${markedSource("WORKBENCH_PROJECTION_STATE", "WORKBENCH_PROJECTION_STATE")}
     ${markedSource("WORKBENCH_SHELL_AVAILABILITY", "WORKBENCH_SHELL_AVAILABILITY")}
     ${markedSource("WORKBENCH_SESSION_RETIREMENT", "WORKBENCH_SESSION_RETIREMENT")}
     const projectionState = createWorkbenchProjectionState();
     const shellAvailability = createShellAvailability();
     const sessionRetirement = createSessionRetirement();
     const scopedSessionRefusal = createScopedSessionRefusal();
     /* the page has never been answered for this id: it is the one the URL
        named and the first snapshot is the refusal */
     const scopedSessionId = ${JSON.stringify(retired)};
     let streamGeneration = 0;
     let adoptions = 0;
     let retryTimers = 0;
     let streamsStopped = 0;
     let shortCircuited = 0;
     let sessionErrorText = "";
     let renderedModel = null;
     const requested = [];
     function clearTimeout() {}
     function setTimeout() {
       retryTimers += 1;
       return retryTimers;
     }
     const AbortSignal = { timeout() { return undefined; } };
     function refusal() {
       return {
         ok: false,
         status: 409,
         async json() {
           return {
             error: ${JSON.stringify(canonical)},
             session_id: scopedSessionId,
             session_retirement: "retired",
           };
         },
       };
     }
     /* the production gate: a refused id is answered in the page, so the
        workbench is never asked again */
     function fetch(input) {
       if (refusedScopedProbeIsFutile(scopedSessionRefusal, input)) {
         shortCircuited += 1;
         return Promise.resolve(refusal());
       }
       requested.push(input);
       return Promise.resolve(refusal());
     }
     async function adoptReplacementSession() { adoptions += 1; }
     function stopEventStreams() { streamsStopped += 1; }
     function showSessionError(message) { sessionErrorText = message; }
     function renderShellStatus() {
       renderedModel = shellStatusModel(shellAvailability, {});
     }
     function renderError(message, options) { errors.push([message, options]); }
     function applyStateSnapshot() {}
     function restartEventStreams() {}
     ${markedSource("WORKBENCH_STATE_FETCH", "WORKBENCH_STATE_FETCH")}
     ${markedSource("WORKBENCH_STATE_RECOVERY", "WORKBENCH_STATE_RECOVERY")}
     ({
       runLoadState: loadState,
       adoptions: () => adoptions,
       retryTimers: () => retryTimers,
       streamsStopped: () => streamsStopped,
       sessionErrorText: () => sessionErrorText,
       renderedModel: () => renderedModel,
       requested: () => requested.join(" "),
       shortCircuited: () => shortCircuited,
       probeIsFutile: path => refusedScopedProbeIsFutile(scopedSessionRefusal, path),
       retirementPhase: () => sessionRetirement.phase,
     });`,
    { ...context, shellStatusModel: shell.shellStatusModel },
  );

  await runtime.runLoadState();

  // The refusal is the answer to what was asked, rendered where the runbook
  // looks for it, and it is not an outage to retry either.
  assert.equal(runtime.sessionErrorText(), canonical);
  assert.equal(runtime.adoptions(), 0, "an id this page never held has no replacement to follow");
  assert.equal(runtime.retirementPhase(), "none", "a refused id is not this page's retirement");
  assert.equal(runtime.renderedModel().phase, "terminal");
  assert.equal(runtime.renderedModel().banner.text, canonical);
  assert.equal(runtime.retryTimers(), 0, "the retired id will never answer; retrying is the storm");
  assert.equal(runtime.streamsStopped(), 1, "the streams are scoped to the refused id too");
  assert.equal(runtime.requested(), "/api/state", "one refusal, then nothing");

  // Everything session-scoped stops; the roster is how the operator leaves.
  assert.equal(runtime.probeIsFutile("/api/state"), true);
  assert.equal(runtime.probeIsFutile("/api/events?cursor=0"), true);
  assert.equal(runtime.probeIsFutile("/api/work"), true);
  assert.equal(runtime.probeIsFutile("/api/sessions"), false);

  // A second pass changes nothing and asks nothing: the fetch above throws if
  // a refused id is requested again.
  await runtime.runLoadState();
  assert.equal(runtime.adoptions(), 0);
  assert.equal(runtime.sessionErrorText(), canonical);
  assert.equal(runtime.requested(), "/api/state", "the workbench is asked once, not again");
  assert.equal(runtime.shortCircuited(), 1, "the refusal is answered in the page");
  assert.equal(runtime.retryTimers(), 0);
});

test("adoption is a hand-off from a session the workbench answered for", () => {
  const retirement = sessionRetirementModule();

  // An unscoped page reads whatever the workbench resolves a query-less call
  // to, so the roster's rotation is its hand-off and always applies.
  assert.equal(retirement.retirementIsHandOff(null, false), true);
  // A tab pinned to an id follows the replacement only once that id has
  // actually answered it.
  assert.equal(retirement.retirementIsHandOff("workbench-0298b733", true), true);
  assert.equal(
    retirement.retirementIsHandOff("workbench-0298b733", false),
    false,
    "an id refused on first contact was never this page's session to hand off",
  );

  const refused = retirement.createScopedSessionRefusal();
  assert.equal(retirement.scopedSessionIsRefused(refused), false);
  assert.equal(retirement.refusedScopedProbeIsFutile(refused, "/api/state"), false);
  retirement.noteScopedSessionRefusal(
    refused,
    { sessionId: "fig754-retired-0915a", phase: "retired" },
    "was used and deleted",
  );
  assert.equal(retirement.scopedSessionIsRefused(refused), true);
  assert.equal(retirement.refusedScopedProbeIsFutile(refused, "/api/state"), true);
  assert.equal(retirement.refusedScopedProbeIsFutile(refused, "/api/observations?cursor=1"), true);
  assert.equal(
    retirement.refusedScopedProbeIsFutile(refused, "/api/sessions/select"),
    false,
    "the roster is not session-scoped and is how the operator leaves",
  );
  assert.equal(retirement.refusedScopedProbeIsFutile(refused, "/static/app.css"), false);
});

test("a drop after hydration reconnects over the last known content", () => {
  const shell = shellModule();
  const availability = shell.markShellChannel(
    shell.markShellHydrated(
      shell.markShellChannel(shell.createShellAvailability(), "product", true),
    ),
    "product",
    false,
    "transcript stream disconnected",
  );

  const render = shellRender(
    shell.shellStatusModel(availability, {
      session: "workbench-a",
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

test("a first connection during an active turn is running, but an established stream drop reconnects", () => {
  const shell = shellModule();
  const availability = shell.markShellChannel(
    shell.createShellAvailability(),
    "state",
    true,
  );
  shell.markShellChannel(
    availability,
    "observation",
    false,
    "the live turn stream disconnected before the page attached",
  );
  shell.markShellHydrated(availability);

  const firstConnection = shellRender(
    shell.shellStatusModel(availability, {
      session: "workbench-a",
      busy: true,
    }),
  );
  assert.equal(firstConnection.pill, "running");
  assert.equal(firstConnection.subtitle, "turn running");

  shell.markShellChannel(availability, "observation", true);
  shell.markShellChannel(
    availability,
    "observation",
    false,
    "the live turn stream disconnected after attachment",
  );
  const establishedDrop = shellRender(
    shell.shellStatusModel(availability, {
      session: "workbench-a",
      busy: true,
    }),
  );
  assert.equal(establishedDrop.pill, "reconnecting");
  assert.match(establishedDrop.subtitle, /a turn was running/);
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
    shell.shellStatusModel(availability, { session: "workbench-a" }),
  );
  assert.equal(render.pill, "idle");
  assert.equal(render.pillClass, "pill");
  assert.equal(render.session, "workbench-a");
  assert.equal(render.bannerHidden, true);
  assert.equal(render.placeholderClass, "empty");
  assert.equal(render.placeholder, shell.timelinePlaceholder("live"));

  // The hint ends in a working link to the triggers view: the same
  // setView("triggers") the sidebar entry calls.
  assert.equal(render.placeholderLink.textContent, "triggers page");
  render.placeholderLink.click();
  assert.deepEqual(render.views, ["triggers"]);

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

/* The red banner Sam saw flash on every send: `/api/state` answered, but the observation stream had already moved past it, so the shell marked the snapshot channel *unreachable* and painted "reconnecting" until the retry Staleness is ordering, not unreachability — a server that answered is reachable, and only the bounded retry ladder should run. */
test("a snapshot overtaken by live observations is not an outage", async () => {
  let resolveSnapshot;
  let failNextFetch = false;
  const scheduledRetries = [];
  const retryDelays = [];
  let nextRetryTimerId = 0;
  const context = {
    conversation: { epoch: () => 0 },
    Map,
    Set,
    Math,
    Number,
    String,
    Boolean,
    Object,
    Promise,
    Error,
    async fetchStateSnapshot() {
      if (failNextFetch) throw new Error("fetch failed");
      return new Promise(resolve => { resolveSnapshot = resolve; });
    },
    renderShellStatus() {},
    setTimeout(callback, delay) {
      const timer = { id: ++nextRetryTimerId, callback };
      scheduledRetries.push(timer);
      retryDelays.push(delay);
      return timer.id;
    },
    clearTimeout(timerId) {
      const index = scheduledRetries.findIndex(timer => timer.id === timerId);
      if (index >= 0) scheduledRetries.splice(index, 1);
    },
    handleSessionRetirementFailure() { return false; },
    handleTerminalStateFailure() { return false; },
    markShellTerminal() {},
    snapshotFailureReason() { return "the workbench stopped answering"; },
    renderError() {},
    applyStateSnapshot() {
      throw new Error("an overtaken snapshot must never be applied");
    },
    restartEventStreams() {
      throw new Error("an overtaken snapshot must never re-attach the streams");
    },
    streamGeneration: 1,
  };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_PROJECTION_STATE", "WORKBENCH_PROJECTION_STATE")}
     ${markedSource("WORKBENCH_SHELL_AVAILABILITY", "WORKBENCH_SHELL_AVAILABILITY")}
     ${markedSource("WORKBENCH_STATE_RETRY", "WORKBENCH_STATE_RETRY")}
     this.projectionState = createWorkbenchProjectionState();
     this.shellAvailability = createShellAvailability();
     this.loadState = loadState;`,
    context,
  );

  // The shell as it stands mid-session: hydrated, with the snapshot channel
  // established, which is what makes an established-channel outage claimable.
  const shell = shellModule();
  context.projectionState.sessionId = "workbench-a";
  context.projectionState.observationCursor = "observation-7";
  shell.markShellChannel(context.shellAvailability, "state", true);
  shell.markShellHydrated(context.shellAvailability);

  // A turn starts: `loadState()` fires, and a live observation lands before the
  // response does, so the response is the latest request but no longer current.
  const overtaken = context.loadState();
  context.projectionState.observationCursor = "observation-8";
  resolveSnapshot({
    settings: { session_id: "workbench-a" },
    observation: { cursor: "observation-7" },
    product_events: { cursor: 3, events: [] },
  });
  await overtaken;

  assert.equal(
    context.shellAvailability.channels.state,
    true,
    "a server that answered is reachable, whatever the response's ordering",
  );
  const model = shell.shellStatusModel(context.shellAvailability, { session: "workbench-a" });
  assert.equal(shellRender(model).bannerHidden, true, "the outage banner must stay hidden");
  assert.equal(
    scheduledRetries.length,
    1,
    "the bounded retry ladder must still run so the snapshot converges",
  );
  assert.deepEqual(retryDelays, [900]);

  // The real-outage law is untouched: a fetch that fails marks the channel down
  // and the established-channel banner comes back.
  failNextFetch = true;
  await assert.rejects(context.loadState(), /state unavailable/);
  assert.equal(context.shellAvailability.channels.state, false);
  const outage = shell.shellStatusModel(context.shellAvailability, { session: "workbench-a" });
  assert.equal(shellRender(outage).bannerHidden, false);
});

/* The second flash Sam saw: `/api/state` opens the session, and the running turn
   leaves that open contended, so the workbench answers 503 "session is
   temporarily busy; retry the request" (verdict Retryable). A server that
   answered with a retry instruction is not an outage. */
test("a 503 from a snapshot read retries quietly instead of claiming an outage", async () => {
  let nextFailure = null;
  const scheduledRetries = [];
  const retryDelays = [];
  let nextRetryTimerId = 0;
  const renderedErrors = [];
  const context = {
    conversation: { epoch: () => 0 },
    Map,
    Set,
    Math,
    Number,
    String,
    Boolean,
    Object,
    Promise,
    Error,
    async fetchStateSnapshot() {
      if (nextFailure) throw nextFailure;
      throw new Error("the test executes only failures");
    },
    renderShellStatus() {},
    setTimeout(callback, delay) {
      const timer = { id: ++nextRetryTimerId, callback };
      scheduledRetries.push(timer);
      retryDelays.push(delay);
      return timer.id;
    },
    clearTimeout(timerId) {
      const index = scheduledRetries.findIndex(timer => timer.id === timerId);
      if (index >= 0) scheduledRetries.splice(index, 1);
    },
    renderError(message) { renderedErrors.push(message); },
    handleSessionRetirementFailure() { return false; },
    handleTerminalStateFailure() { return false; },
    snapshotFailureReason() { return "the workbench stopped answering"; },
    markShellTerminal() {},
    applyStateSnapshot() {},
    restartEventStreams() {},
    streamGeneration: 1,
  };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_PROJECTION_STATE", "WORKBENCH_PROJECTION_STATE")}
     ${markedSource("WORKBENCH_SHELL_AVAILABILITY", "WORKBENCH_SHELL_AVAILABILITY")}
     ${markedSource("WORKBENCH_STATE_RETRY", "WORKBENCH_STATE_RETRY")}
     this.projectionState = createWorkbenchProjectionState();
     this.shellAvailability = createShellAvailability();
     this.StateSnapshotHttpError = StateSnapshotHttpError;
     this.stateFailureDisposition = stateFailureDisposition;
     this.loadState = loadState;`,
    context,
  );
  const shell = shellModule();
  shell.markShellChannel(context.shellAvailability, "state", true);
  shell.markShellHydrated(context.shellAvailability);

  // 503 is its own disposition: neither a dead backend nor a refusal to answer.
  const contended = new context.StateSnapshotHttpError(
    503,
    "session is temporarily busy; retry the request",
  );
  assert.equal(context.stateFailureDisposition(contended), "retryable");
  assert.equal(contended.terminal, false, "a retry instruction is not a terminal refusal");

  nextFailure = contended;
  await context.loadState();
  assert.equal(
    context.shellAvailability.channels.state,
    true,
    "a 503 must not mark the snapshot channel unreachable",
  );
  assert.equal(
    shellRender(shell.shellStatusModel(context.shellAvailability, { session: "workbench-a" }))
      .bannerHidden,
    true,
    "a mid-turn 503 must not paint the outage banner",
  );
  assert.deepEqual(retryDelays, [900], "the bounded retry ladder still runs");
  assert.deepEqual(renderedErrors, [], "a quiet retry says nothing in the transcript");

  // A 500 is still an outage, and a 4xx is still terminal: only 503 is quiet.
  nextFailure = new context.StateSnapshotHttpError(500, "internal server error");
  await assert.rejects(context.loadState(), /state unavailable/);
  assert.equal(context.shellAvailability.channels.state, false);
  assert.equal(context.stateFailureDisposition(
    new context.StateSnapshotHttpError(404, "no such session"),
  ), "terminal");
});

/* FIG-3136: a reset retires the session the page is scoped to, and every
   session-bound surface then refuses that id — while the delete settles, and
   forever after. One reset collected 364 such refusals in 75 s and the page
   learned nothing from any of them: it painted "refused", disabled the reset
   button for good, and kept probing a tombstone. These run the production
   classifier and probe gate. */
function sessionRetirementModule() {
  const context = { Object, Number, String, Boolean, Array };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_SESSION_RETIREMENT", "WORKBENCH_SESSION_RETIREMENT")}
     this.exports = {
       createSessionRetirement,
       sessionRetirementRefusal,
       noteSessionRetirement,
       clearSessionRetirement,
       sessionIsRetiring,
       sessionScopedProbeIsFutile,
       replacementSessionId,
       retirementIsHandOff,
       createScopedSessionRefusal,
       noteScopedSessionRefusal,
       scopedSessionIsRefused,
       refusedScopedProbeIsFutile
     };`,
    context,
  );
  return context.exports;
}

test("a refusal naming this session's retirement is a hand-off, not an outage", () => {
  const retirement = sessionRetirementModule();
  const scoped = "workbench-0298b733";

  // The refusal the reproduction showed as a red error row, classified.
  const refusal = retirement.sessionRetirementRefusal(
    409,
    {
      error: "session `workbench-0298b733` is being deleted; session ids cannot be reused",
      session_id: scoped,
      session_retirement: "retiring",
    },
    scoped,
  );
  assert.equal(refusal.phase, "retiring");
  assert.equal(refusal.sessionId, scoped);

  // Another session's delete is not this tab's hand-off, and a conflict that
  // names no retirement stays the conflict it was.
  assert.equal(
    retirement.sessionRetirementRefusal(
      409,
      { session_id: "workbench-other", session_retirement: "retired" },
      scoped,
    ),
    null,
    "a refusal about another session must not retire this page's session",
  );
  assert.equal(
    retirement.sessionRetirementRefusal(409, { error: "a turn is already running" }, scoped),
    null,
    "a conflict with no retirement is not a hand-off",
  );
  assert.equal(
    retirement.sessionRetirementRefusal(
      503,
      { session_id: scoped, session_retirement: "retiring" },
      scoped,
    ),
    null,
    "only a 409 retires a session",
  );
});

test("a retiring session stops the probe storm and keeps the one probe that ends it", () => {
  const retirement = sessionRetirementModule();
  const scoped = "workbench-0298b733";
  const state = retirement.createSessionRetirement();

  // Before any refusal every rail runs.
  assert.equal(retirement.sessionScopedProbeIsFutile(state, "/api/observations"), false);

  retirement.noteSessionRetirement(state, {
    phase: "retiring",
    sessionId: scoped,
  });
  for (const rail of [
    "/api/observations",
    "/api/events?cursor=4",
    "/api/queued_work",
    "/api/triggers",
    "/api/lash_vm/graphs",
    "/api/work",
  ]) {
    assert.equal(
      retirement.sessionScopedProbeIsFutile(state, rail),
      true,
      `${rail} can only collect refusals against a retiring session`,
    );
  }
  // The snapshot is what tells the page "retiring" has become "retired", and
  // the roster is not session-scoped and is where the replacement is found.
  assert.equal(retirement.sessionScopedProbeIsFutile(state, "/api/state"), false);
  assert.equal(retirement.sessionScopedProbeIsFutile(state, "/api/sessions"), false);
  assert.equal(retirement.sessionScopedProbeIsFutile(state, "/api/sessions/select"), false);

  // retiring -> retired is the only direction: a late refusal answered before
  // the delete settled must not walk the page back into waiting.
  retirement.noteSessionRetirement(state, { phase: "retired", sessionId: scoped });
  retirement.noteSessionRetirement(state, { phase: "retiring", sessionId: scoped });
  assert.equal(state.phase, "retired");

  retirement.clearSessionRetirement(state);
  assert.equal(retirement.sessionIsRetiring(state), false);
  assert.equal(retirement.sessionScopedProbeIsFutile(state, "/api/observations"), false);
});

test("the replacement for a retired session is the live one, never the tombstone", () => {
  const retirement = sessionRetirementModule();
  const retired = "workbench-0298b733";

  // The delete's settlement rotates the roster onto the replacement, so the
  // workbench's own current is the first answer.
  assert.equal(
    retirement.replacementSessionId(
      {
        current_session_id: "workbench-04cb2237",
        sessions: [{ session_id: "workbench-04cb2237", last_active_ms: 2 }],
      },
      retired,
    ),
    "workbench-04cb2237",
  );

  // A roster whose current is still the tombstone hands back the newest live
  // session instead — never the retired id.
  assert.equal(
    retirement.replacementSessionId(
      {
        current_session_id: retired,
        sessions: [
          { session_id: retired, last_active_ms: 9 },
          { session_id: "workbench-older", last_active_ms: 1 },
          { session_id: "workbench-newer", last_active_ms: 5 },
        ],
      },
      retired,
    ),
    "workbench-newer",
  );

  // Nothing live left: the page has to create one.
  assert.equal(
    retirement.replacementSessionId(
      { current_session_id: retired, sessions: [{ session_id: retired, last_active_ms: 9 }] },
      retired,
    ),
    null,
  );
});

test("a retired session is replaced on screen, not refused", () => {
  const shell = shellModule();
  const availability = shell.markShellHydrated(shell.createShellAvailability());

  // Today's answer for this refusal: terminal. It names a fault the operator
  // cannot act on and offers a retry that can never succeed.
  shell.markShellTerminal(availability, "state", "session `workbench-0298b733` is being deleted");
  assert.equal(shell.shellPhase(availability), "terminal");
  assert.equal(shellRender(shell.shellStatusModel(availability)).pill, "refused");

  shell.markShellReplacing(availability, "workbench-0298b733");
  assert.equal(shell.shellPhase(availability), "replacing");
  const render = shellRender(
    shell.shellStatusModel(availability, { session: "workbench-0298b733" }),
  );
  assert.equal(render.pill, "replacing");
  assert.equal(
    render.bannerHidden,
    true,
    "a session being replaced is not a fault to banner: the page is already repairing it",
  );

  shell.clearShellReplacing(availability);
  shell.markShellChannel(availability, "state", true);
  assert.equal(shell.shellPhase(availability), "live");
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
  assert.equal(shell.shellPhase(stuck), "connecting");
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

test("a typed model survives an intervening snapshot and is what the turn sends", () => {
  function control() {
    const node = {
      className: "",
      value: "",
      hidden: false,
      classList: {
        toggle(token, force) {
          const tokens = node.className.split(" ").filter(Boolean).filter(item => item !== token);
          if (force) tokens.push(token);
          node.className = tokens.join(" ");
        },
      },
    };
    return node;
  }
  const modelInput = control();
  const modelPending = control();
  const variantSelect = control();
  variantSelect.value = "high";
  const context = {
    modelInput,
    modelPending,
    variantSelect,
    validateModel() {},
  };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_MODEL_SELECTION", "WORKBENCH_MODEL_SELECTION")}
     this.modelSelection = createModelSelection();
     const modelSelection = this.modelSelection;
     this.applyProjectedModel = applyProjectedModel;
     this.onModelInput = onModelInput;
     this.selectedModelPayload = selectedModelPayload;`,
    context,
  );

  // The server's model at load.
  context.applyProjectedModel("dev/replay-route-a");
  assert.equal(modelInput.value, "dev/replay-route-a");
  assert.equal(modelPending.hidden, true);

  // The operator types the route they want and pauses.
  modelInput.value = "dev/replay-route-b";
  context.onModelInput();
  assert.equal(modelPending.hidden, false, "a pending edit must be visible");
  assert.match(modelInput.className, /pending/);

  // A snapshot lands in that pause. It used to overwrite the edit silently.
  context.applyProjectedModel("dev/replay-route-a");
  assert.equal(
    modelInput.value,
    "dev/replay-route-b",
    "a snapshot between typing and sending must not change what is sent",
  );
  assert.equal(modelPending.hidden, false);

  // The send reads the control, so the typed route is what POST /api/turn carries.
  const body = context.selectedModelPayload();
  assert.equal(body.model, "dev/replay-route-b");
  assert.equal(body.model_variant, "high");

  // The server adopts the sent model, and the edit stops being pending on its own.
  context.applyProjectedModel("dev/replay-route-b");
  assert.equal(modelInput.value, "dev/replay-route-b");
  assert.equal(modelPending.hidden, true, "an adopted edit is no longer pending");
  assert.doesNotMatch(modelInput.className, /pending/);

  // Typing back to the projected value is not an edit at all.
  modelInput.value = "dev/replay-route-b ";
  context.onModelInput();
  assert.equal(modelPending.hidden, true);

  // The snapshot path owns exactly one model write, and it goes through here.
  const snapshot = markedSource("WORKBENCH_STATE_SNAPSHOT", "WORKBENCH_STATE_SNAPSHOT");
  assert.match(snapshot, /applyProjectedModel\(state\.settings\.model\)/);
  assert.doesNotMatch(snapshot, /modelInput\.value\s*=/);
});

test("the session sidebar titles, orders and highlights chats", () => {
  const context = {};
  vm.runInNewContext(
    `${markedSource("WORKBENCH_SESSION_SIDEBAR", "WORKBENCH_SESSION_SIDEBAR")}
     this.sessionTitle = sessionTitle;
     this.sidebarSessions = sidebarSessions;
     this.sessionAge = sessionAge;`,
    context,
  );
  const generated = "workbench-0123456789abcdef0123456789abcdef";
  const listing = {
    current_session_id: "workbench-b",
    sessions: [
      { session_id: "workbench-a", name: "workbench-a", created_at_ms: 10, last_active_ms: 10 },
      { session_id: "workbench-b", name: "Fix the cron test", created_at_ms: 20, last_active_ms: 50 },
      { session_id: "workbench-c", name: generated, created_at_ms: 30, last_active_ms: 30 },
    ],
  };
  // JSON crosses the vm realm boundary, so the comparison is structural.
  const rows = JSON.parse(JSON.stringify(context.sidebarSessions(listing, "workbench-b")));
  assert.deepEqual(
    rows.map(row => [row.id, row.title.text, row.title.untitled, row.current]),
    [
      ["workbench-b", "Fix the cron test", false, true],
      ["workbench-c", "New chat", true, false],
      ["workbench-a", "New chat", true, false],
    ],
  );
  // A tab pinned to a session the roster does not carry still lists and
  // highlights it.
  const pinned = JSON.parse(JSON.stringify(context.sidebarSessions(listing, "workbench-external")));
  assert.equal(pinned.length, 4);
  assert.deepEqual(
    pinned.filter(row => row.current).map(row => row.id),
    ["workbench-external"],
  );
  const now = 10 * 24 * 3600 * 1000;
  assert.equal(context.sessionAge(0, now), "");
  assert.equal(context.sessionAge(now - 30 * 1000, now), "now");
  assert.equal(context.sessionAge(now - 5 * 60 * 1000, now), "5m");
  assert.equal(context.sessionAge(now - 3 * 3600 * 1000, now), "3h");
  assert.equal(context.sessionAge(now - 2 * 24 * 3600 * 1000, now), "2d");
});
