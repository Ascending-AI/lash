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
const multiAttachmentMessage = JSON.parse(
  process.env.LASH_WORKBENCH_MULTI_ATTACHMENT_MESSAGE ?? "null",
);
const executionEvidenceScenarios = JSON.parse(
  process.env.LASH_WORKBENCH_EXECUTION_EVIDENCE_SCENARIOS ?? "null",
);
assert.equal(
  executionEvidenceScenarios?.providers?.length,
  2,
  "execution evidence must come from real Rust runtime scenarios",
);
assert.ok(
  multiAttachmentMessage,
  "LASH_WORKBENCH_MULTI_ATTACHMENT_MESSAGE must come from the Rust projection gate",
);
const turnEvents = JSON.parse(
  process.env.LASH_WORKBENCH_TURN_EVENTS ?? "null",
);
assert.ok(
  turnEvents,
  "LASH_WORKBENCH_TURN_EVENTS must come from the Rust projection gate",
);
const durableToolTranscript = JSON.parse(
  process.env.LASH_WORKBENCH_DURABLE_TOOL_TRANSCRIPT ?? "null",
);
assert.ok(
  durableToolTranscript,
  "LASH_WORKBENCH_DURABLE_TOOL_TRANSCRIPT must come from a committed Rust trajectory",
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

function dispatchTurnEvent(event) {
  const calls = [];
  const dispatchContext = {
    __LASH_WORKBENCH_TURN_EVENT_HOOK__(hooked) {
      calls.push(["hook", hooked]);
    },
    renderStreamingUsage(usage, turnId) {
      calls.push(["usage", usage, turnId]);
    },
    renderError(message, options) {
      calls.push(["error", message, options]);
    },
  };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_TURN_EVENT_DISPATCH", "WORKBENCH_TURN_EVENT_DISPATCH")}
     handleTurnEvent(${JSON.stringify(event)}, "turn-dispatch-test");`,
    dispatchContext,
  );
  return JSON.parse(JSON.stringify(calls));
}

test("streaming usage dispatches the cumulative provider counters", () => {
  assert.deepEqual(dispatchTurnEvent(turnEvents.usage), [
    ["hook", turnEvents.usage],
    ["usage", turnEvents.usage.cumulative, "turn-dispatch-test"],
  ]);
});

test("ordinary provider errors do not dispatch retryable client errors", () => {
  assert.deepEqual(dispatchTurnEvent(turnEvents.error), [
    ["hook", turnEvents.error],
  ]);
});

test("provider failure settles to the same durable row for sender and observer", () => {
  function projectPage(lastRequest) {
    function element(tagName) {
      return {
        tagName,
        className: "",
        textContent: "",
        children: [],
        parentNode: null,
        append(...children) {
          for (const child of children) {
            if (typeof child !== "string") this.appendChild(child);
          }
        },
        appendChild(child) {
          this.children.push(child);
          child.parentNode = this;
          return child;
        },
        closest() { return null; },
      };
    }

    const timeline = element("timeline");
    const page = {
      Set,
      document: { createElement: element },
      timeline,
      projectionState: createWorkbenchProjectionState(),
      renderedProductEvents: new Set(),
      renderedMessages: new Set(),
      lastRequest,
      assistantDraft: null,
      assistantDraftTurnId: null,
      assistantDraftText: "",
      assistantDraftChunks: [],
      reasoning: null,
      reasoningChunks: [],
      pendingCodeBlock: null,
      pendingTools: [],
      __LASH_WORKBENCH_TURN_EVENT_HOOK__() {},
      clearEmpty() {},
      clearRetryStatus() {},
      markStreamingUsageSettled() {},
      appendTool() {},
      roleLabel(role) { return role; },
      setMessageBody(body, _role, text) { body.textContent = text; },
      renderMessageAttachments() {},
      scrollToEnd() {},
      renderIngressReceipt() {},
      setBusy() {},
      refreshUsage() {},
    };
    vm.runInNewContext(
      `${markedSource("WORKBENCH_MESSAGE_RENDER", "WORKBENCH_MESSAGE_RENDER")}
       ${markedSource("WORKBENCH_TERMINAL_TURN_TOMBSTONES", "WORKBENCH_TERMINAL_TURN_TOMBSTONES")}
       ${markedSource("WORKBENCH_TRANSIENT_SETTLEMENT", "WORKBENCH_TRANSIENT_SETTLEMENT")}
       ${markedSource("WORKBENCH_TURN_EVENT_DISPATCH", "WORKBENCH_TURN_EVENT_DISPATCH")}
       ${markedSource("WORKBENCH_PRODUCT_EVENT_REDUCER", "WORKBENCH_PRODUCT_EVENT_REDUCER")}
       handleTurnEvent(${JSON.stringify(turnEvents.error)}, "provider-failure-turn");
       this.transientCount = timeline.children.length;
       applyProductEvent({
         event_id: "provider-failure-message",
         sequence: 1,
         type: "message",
         message: {
           id: "provider-failure-message",
           role: "event",
           text: "turn could not be completed",
           attachments: []
         }
       });
       applyProductEvent({
         event_id: "provider-failure-done",
         sequence: 2,
         type: "done",
         turn_id: "provider-failure-turn",
         outcome: "completed"
       });`,
      page,
    );
    return {
      transientCount: page.transientCount,
      rows: timeline.children.map((row) => ({
        className: row.className,
        text: row.children[1].textContent,
        controls: row.children[1].children.length,
      })),
    };
  }

  const sender = projectPage({ url: "/api/turn", payload: { text: "sent here" } });
  const observer = projectPage(null);
  assert.deepEqual(sender, {
    transientCount: 0,
    rows: [{
      className: "message event",
      text: "turn could not be completed",
      controls: 0,
    }],
  });
  assert.deepEqual(observer, sender);
});

test("second-turn streaming usage stays session-monotonic and equals settlement", () => {
  const usageTotal = { textContent: "", title: "" };
  const usageBreakdown = { textContent: "", title: "" };
  const usageContext = { Intl, Map, Number, usageTotal, usageBreakdown };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_TERMINAL_TURN_TOMBSTONES", "WORKBENCH_TERMINAL_TURN_TOMBSTONES")}
     ${markedSource("WORKBENCH_USAGE_PROJECTION", "WORKBENCH_USAGE_PROJECTION")}
     const readings = [];
     const record = () => readings.push({
       total: usageTotal.textContent,
       breakdown: usageBreakdown.textContent,
       detail: usageBreakdown.title,
       ledger: usageTotal.title
     });
     renderUsage({ entry_count: 2, usage: {
       input_tokens: 40,
       cache_read_input_tokens: 10,
       cache_write_input_tokens: 5,
       output_tokens: 20,
       reasoning_output_tokens: 2,
       total_tokens: 75
     }});
     record();
     renderStreamingUsage({
       input_tokens: 4,
       cache_read_input_tokens: 3,
       cache_write_input_tokens: 2,
       output_tokens: 1,
       reasoning_output_tokens: 1
     }, "turn-b");
     record();
     renderStreamingUsage({
       input_tokens: 7,
       cache_read_input_tokens: 4,
       cache_write_input_tokens: 3,
       output_tokens: 6,
       reasoning_output_tokens: 4
     }, "turn-b");
     record();
     markStreamingUsageSettled("turn-b");
     renderUsage({ entry_count: 3, usage: {
       input_tokens: 47,
       cache_read_input_tokens: 14,
       cache_write_input_tokens: 8,
       output_tokens: 26,
       reasoning_output_tokens: 6,
       total_tokens: 95
     }});
     record();
     this.readings = readings;`,
    usageContext,
  );

  assert.deepEqual(JSON.parse(JSON.stringify(usageContext.readings)), [
    {
      total: "75 total",
      breakdown: "55 in · 20 out",
      detail: "uncached input 40 · cache read 10 · cache write 5 · reasoning output 2",
      ledger: "2 source/model ledger entries",
    },
    {
      total: "85 total",
      breakdown: "64 in · 21 out",
      detail: "uncached input 44 · cache read 13 · cache write 7 · reasoning output 3",
      ledger: "2 settled source/model ledger entries · live turn usage included",
    },
    {
      total: "95 total",
      breakdown: "69 in · 26 out",
      detail: "uncached input 47 · cache read 14 · cache write 8 · reasoning output 6",
      ledger: "2 settled source/model ledger entries · live turn usage included",
    },
    {
      total: "95 total",
      breakdown: "69 in · 26 out",
      detail: "uncached input 47 · cache read 14 · cache write 8 · reasoning output 6",
      ledger: "3 source/model ledger entries",
    },
  ]);
});

function runTerminalUsageProjection(body) {
  const usageTotal = { textContent: "", title: "" };
  const usageBreakdown = { textContent: "", title: "" };
  const usageContext = {
    Intl,
    Map,
    Number,
    Set,
    usageTotal,
    usageBreakdown,
    projectionState: createWorkbenchProjectionState(),
    renderedProductEvents: new Set(),
    pendingTools: [],
    assistantDraft: null,
    assistantDraftTurnId: null,
    assistantDraftText: "",
    assistantDraftChunks: [],
    reasoningChunks: [],
    pendingCodeBlock: null,
    reasoning: null,
    clearRetryStatus() {},
    appendTool() {},
    renderMessage() {},
    renderIngressReceipt() {},
    setBusy() {},
  };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_TERMINAL_TURN_TOMBSTONES", "WORKBENCH_TERMINAL_TURN_TOMBSTONES")}
     ${markedSource("WORKBENCH_USAGE_PROJECTION", "WORKBENCH_USAGE_PROJECTION")}
     ${markedSource("WORKBENCH_TRANSIENT_SETTLEMENT", "WORKBENCH_TRANSIENT_SETTLEMENT")}
     ${markedSource("WORKBENCH_PRODUCT_EVENT_REDUCER", "WORKBENCH_PRODUCT_EVENT_REDUCER")}
     ${body}`,
    usageContext,
  );
  return {
    total: usageTotal.textContent,
    breakdown: usageBreakdown.textContent,
    ledger: usageTotal.title,
  };
}

test("Done then authoritative usage refresh rejects delayed same-turn usage", () => {
  assert.deepEqual(runTerminalUsageProjection(`
    function refreshUsage() {
      renderUsage({ entry_count: 3, usage: {
        input_tokens: 47,
        cache_read_input_tokens: 14,
        cache_write_input_tokens: 8,
        output_tokens: 26,
        reasoning_output_tokens: 6
      }});
    }
    renderUsage({ entry_count: 2, usage: {
      input_tokens: 40,
      cache_read_input_tokens: 10,
      cache_write_input_tokens: 5,
      output_tokens: 20,
      reasoning_output_tokens: 2
    }});
    renderStreamingUsage({
      input_tokens: 7,
      cache_read_input_tokens: 4,
      cache_write_input_tokens: 3,
      output_tokens: 6,
      reasoning_output_tokens: 4
    }, "turn-a");
    applyProductEvent({
      event_id: "turn-a-done",
      sequence: 1,
      type: "done",
      turn_id: "turn-a"
    });
    renderStreamingUsage({
      input_tokens: 7,
      cache_read_input_tokens: 4,
      cache_write_input_tokens: 3,
      output_tokens: 6,
      reasoning_output_tokens: 4
    }, "turn-a");
  `), {
    total: "95 total",
    breakdown: "69 in · 26 out",
    ledger: "3 source/model ledger entries",
  });
});

test("Done before first usage rejects that turn's delayed first observation", () => {
  assert.deepEqual(runTerminalUsageProjection(`
    function refreshUsage() {
      renderUsage({ entry_count: 2, usage: {
        input_tokens: 40,
        cache_read_input_tokens: 10,
        cache_write_input_tokens: 5,
        output_tokens: 20,
        reasoning_output_tokens: 2
      }});
    }
    renderUsage({ entry_count: 2, usage: {
      input_tokens: 40,
      cache_read_input_tokens: 10,
      cache_write_input_tokens: 5,
      output_tokens: 20,
      reasoning_output_tokens: 2
    }});
    applyProductEvent({
      event_id: "turn-a-done-before-usage",
      sequence: 1,
      type: "done",
      turn_id: "turn-a"
    });
    renderStreamingUsage({
      input_tokens: 7,
      cache_read_input_tokens: 4,
      cache_write_input_tokens: 3,
      output_tokens: 6,
      reasoning_output_tokens: 4
    }, "turn-a");
  `), {
    total: "75 total",
    breakdown: "55 in · 20 out",
    ledger: "2 source/model ledger entries",
  });
});

test("settling one turn preserves another turn's live usage overlay", () => {
  assert.deepEqual(runTerminalUsageProjection(`
    function refreshUsage() {
      renderUsage({ entry_count: 3, usage: {
        input_tokens: 47,
        cache_read_input_tokens: 14,
        cache_write_input_tokens: 8,
        output_tokens: 26,
        reasoning_output_tokens: 6
      }});
    }
    renderUsage({ entry_count: 2, usage: {
      input_tokens: 40,
      cache_read_input_tokens: 10,
      cache_write_input_tokens: 5,
      output_tokens: 20,
      reasoning_output_tokens: 2
    }});
    renderStreamingUsage({
      input_tokens: 7,
      cache_read_input_tokens: 4,
      cache_write_input_tokens: 3,
      output_tokens: 6,
      reasoning_output_tokens: 4
    }, "turn-a");
    renderStreamingUsage({
      input_tokens: 4,
      cache_read_input_tokens: 3,
      cache_write_input_tokens: 2,
      output_tokens: 1,
      reasoning_output_tokens: 1
    }, "turn-b");
    applyProductEvent({
      event_id: "turn-a-done-with-b-live",
      sequence: 1,
      type: "done",
      turn_id: "turn-a"
    });
    renderStreamingUsage({
      input_tokens: 7,
      cache_read_input_tokens: 4,
      cache_write_input_tokens: 3,
      output_tokens: 6,
      reasoning_output_tokens: 4
    }, "turn-a");
  `), {
    total: "105 total",
    breakdown: "78 in · 27 out",
    ledger: "3 settled source/model ledger entries · live turn usage included",
  });
});

test("retry reset retracts only superseded partial text and renders retry status", () => {
  let assistantRemoved = false;
  const reasoningRemoved = { superseded: false, retained: false };
  const supersededReasoning = {
    isConnected: true,
    pre: { textContent: "superseded reasoning" },
    querySelector() { return this.pre; },
    remove() {
      this.isConnected = false;
      reasoningRemoved.superseded = true;
    },
  };
  const retainedReasoning = {
    isConnected: true,
    pre: { textContent: "retained reasoning" },
    querySelector() { return this.pre; },
    remove() {
      this.isConnected = false;
      reasoningRemoved.retained = true;
    },
  };
  const retryEvents = [];
  const projectionContext = {
    Set,
    __LASH_WORKBENCH_TURN_EVENT_HOOK__() {},
    assistantDraft: {
      innerHTML: "",
      closest() {
        return { remove() { assistantRemoved = true; } };
      },
    },
    assistantDraftTurnId: "retry-turn",
    assistantDraftText: "superseded prose retained prose",
    assistantDraftChunks: [
      { correlationId: "prose-superseded", text: "superseded prose " },
      { correlationId: "prose-retained", text: "retained prose" },
    ],
    reasoning: retainedReasoning,
    reasoningChunks: [
      { correlationId: "reasoning-superseded", text: "superseded reasoning", node: supersededReasoning },
      { correlationId: "reasoning-retained", text: "retained reasoning", node: retainedReasoning },
    ],
    renderMarkdownBlocks(text) { return `rendered:${text}`; },
    renderRetryStatus(event) { retryEvents.push(event); },
    scrollToEnd() {},
  };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_ASSISTANT_RETRACTION", "WORKBENCH_ASSISTANT_RETRACTION")}
     ${markedSource("WORKBENCH_REASONING_RETRACTION", "WORKBENCH_REASONING_RETRACTION")}
     ${markedSource("WORKBENCH_ATTEMPT_RESET", "WORKBENCH_ATTEMPT_RESET")}
     ${markedSource("WORKBENCH_TURN_EVENT_DISPATCH", "WORKBENCH_TURN_EVENT_DISPATCH")}
     handleTurnEvent(${JSON.stringify(turnEvents.reset)}, "retry-turn");
     handleTurnEvent(${JSON.stringify(turnEvents.retry)}, "retry-turn");
     this.result = {
       assistantDraftText,
       assistantDraftChunks,
       assistantHtml: assistantDraft.innerHTML,
       reasoningChunks,
       retainedReasoningText: reasoning.querySelector("pre").textContent
     };`,
    projectionContext,
  );

  assert.equal(assistantRemoved, false);
  assert.equal(projectionContext.result.assistantDraftText, "retained prose");
  assert.deepEqual(
    JSON.parse(JSON.stringify(projectionContext.result.assistantDraftChunks)),
    [{ correlationId: "prose-retained", text: "retained prose" }],
  );
  assert.equal(projectionContext.result.assistantHtml, "rendered:retained prose");
  assert.equal(reasoningRemoved.superseded, true);
  assert.equal(reasoningRemoved.retained, false);
  assert.equal(projectionContext.result.retainedReasoningText, "retained reasoning");
  assert.deepEqual(JSON.parse(JSON.stringify(retryEvents)), [turnEvents.retry]);
});

test("retry status ownership survives another turn's delayed Done", () => {
  function element(tagName) {
    const node = {
      tagName,
      className: "",
      children: [],
      parentNode: null,
      textContent: "",
      isConnected: false,
      append(...children) {
        for (const child of children) {
          if (typeof child !== "string") this.appendChild(child);
        }
      },
      appendChild(child) {
        this.children.push(child);
        child.parentNode = this;
        child.isConnected = true;
        return child;
      },
      remove() {
        if (this.parentNode) {
          this.parentNode.children = this.parentNode.children.filter((child) => child !== this);
        }
        this.parentNode = null;
        this.isConnected = false;
      },
      classList: {
        add(token) {
          if (!node.className.split(" ").includes(token)) node.className += ` ${token}`;
        },
      },
    };
    return node;
  }

  const timeline = element("timeline");
  const retryContext = {
    Set,
    Map,
    document: { createElement: element },
    timeline,
    retryStatuses: new Map(),
    projectionState: createWorkbenchProjectionState(),
    renderedProductEvents: new Set(),
    pendingTools: [],
    assistantDraft: null,
    assistantDraftTurnId: null,
    assistantDraftText: "",
    assistantDraftChunks: [],
    reasoningChunks: [],
    pendingCodeBlock: null,
    reasoning: null,
    __LASH_WORKBENCH_TURN_EVENT_HOOK__() {},
    clearEmpty() {},
    scrollToEnd() {},
    markStreamingUsageSettled() {},
    appendTool() {},
    renderMessage() {},
    renderIngressReceipt() {},
    setBusy() {},
    refreshUsage() {},
  };
  const retryA = { ...turnEvents.retry, reason: "turn A retry" };
  const retryB = { ...turnEvents.retry, reason: "turn B retry" };
  const retryBReplacement = { ...retryB, attempt: 2 };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_TERMINAL_TURN_TOMBSTONES", "WORKBENCH_TERMINAL_TURN_TOMBSTONES")}
     ${markedSource("WORKBENCH_RETRY_STATUS", "WORKBENCH_RETRY_STATUS")}
     ${markedSource("WORKBENCH_TRANSIENT_SETTLEMENT", "WORKBENCH_TRANSIENT_SETTLEMENT")}
     ${markedSource("WORKBENCH_TURN_EVENT_DISPATCH", "WORKBENCH_TURN_EVENT_DISPATCH")}
     ${markedSource("WORKBENCH_PRODUCT_EVENT_REDUCER", "WORKBENCH_PRODUCT_EVENT_REDUCER")}
     handleTurnEvent(${JSON.stringify(retryA)}, "turn-a");
     handleTurnEvent(${JSON.stringify(retryB)}, "turn-b");
     this.afterBoth = timeline.children.map(node => node.children[1].textContent);
     handleTurnEvent(${JSON.stringify(retryBReplacement)}, "turn-b");
     this.afterReplacement = timeline.children.map(node => node.children[1].textContent);
     applyProductEvent({
       event_id: "turn-a-delayed-done",
       sequence: 1,
       type: "done",
       turn_id: "turn-a"
     });
     this.afterDelayedDone = timeline.children.map(node => node.children[1].textContent);
     handleTurnEvent({ type: "model_request_started", protocol_iteration: 2 }, "turn-a");
     this.afterOtherRequest = timeline.children.map(node => node.children[1].textContent);
     handleTurnEvent({ type: "model_request_started", protocol_iteration: 2 }, "turn-b");
     this.afterMatchingRequest = timeline.children.length;
     handleTurnEvent(${JSON.stringify(retryB)}, "turn-b");
     finishTransientRows("turn-b");
     this.afterMatchingSettlement = timeline.children.length;`,
    retryContext,
  );

  assert.deepEqual([...retryContext.afterBoth], [
    "provider retry 1 of 3 · turn A retry · waiting 2s",
    "provider retry 1 of 3 · turn B retry · waiting 2s",
  ]);
  assert.deepEqual([...retryContext.afterReplacement], [
    "provider retry 1 of 3 · turn A retry · waiting 2s",
    "provider retry 2 of 3 · turn B retry · waiting 2s",
  ]);
  assert.deepEqual([...retryContext.afterDelayedDone], [
    "provider retry 2 of 3 · turn B retry · waiting 2s",
  ]);
  assert.deepEqual([...retryContext.afterOtherRequest], [
    "provider retry 2 of 3 · turn B retry · waiting 2s",
  ]);
  assert.equal(retryContext.afterMatchingRequest, 0);
  assert.equal(retryContext.afterMatchingSettlement, 0);
});

test("delayed same-turn retry status after Done is ignored without affecting another turn", () => {
  function element(tagName) {
    const node = {
      tagName,
      className: "",
      children: [],
      parentNode: null,
      textContent: "",
      append(...children) {
        for (const child of children) {
          if (typeof child !== "string") this.appendChild(child);
        }
      },
      appendChild(child) {
        this.children.push(child);
        child.parentNode = this;
        return child;
      },
      remove() {
        if (this.parentNode) {
          this.parentNode.children = this.parentNode.children.filter((child) => child !== this);
        }
        this.parentNode = null;
      },
      classList: {
        add(token) {
          if (!node.className.split(" ").includes(token)) node.className += ` ${token}`;
        },
      },
    };
    return node;
  }

  const timeline = element("timeline");
  const retryContext = {
    Set,
    Map,
    document: { createElement: element },
    timeline,
    retryStatuses: new Map(),
    projectionState: createWorkbenchProjectionState(),
    renderedProductEvents: new Set(),
    pendingTools: [],
    assistantDraft: null,
    assistantDraftTurnId: null,
    assistantDraftText: "",
    assistantDraftChunks: [],
    reasoningChunks: [],
    pendingCodeBlock: null,
    reasoning: null,
    __LASH_WORKBENCH_TURN_EVENT_HOOK__() {},
    clearEmpty() {},
    scrollToEnd() {},
    markStreamingUsageSettled() {},
    appendTool() {},
    renderMessage() {},
    renderIngressReceipt() {},
    setBusy() {},
    refreshUsage() {},
  };
  const retryA = { ...turnEvents.retry, reason: "late turn A retry" };
  const retryB = { ...turnEvents.retry, reason: "turn B remains active" };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_TERMINAL_TURN_TOMBSTONES", "WORKBENCH_TERMINAL_TURN_TOMBSTONES")}
     ${markedSource("WORKBENCH_RETRY_STATUS", "WORKBENCH_RETRY_STATUS")}
     ${markedSource("WORKBENCH_TRANSIENT_SETTLEMENT", "WORKBENCH_TRANSIENT_SETTLEMENT")}
     ${markedSource("WORKBENCH_TURN_EVENT_DISPATCH", "WORKBENCH_TURN_EVENT_DISPATCH")}
     ${markedSource("WORKBENCH_PRODUCT_EVENT_REDUCER", "WORKBENCH_PRODUCT_EVENT_REDUCER")}
     handleTurnEvent(${JSON.stringify(retryA)}, "turn-a");
     handleTurnEvent(${JSON.stringify(retryB)}, "turn-b");
     applyProductEvent({
       event_id: "turn-a-done-before-late-retry",
       sequence: 1,
       type: "done",
       turn_id: "turn-a"
     });
     handleTurnEvent(${JSON.stringify(retryA)}, "turn-a");
     this.rows = timeline.children.map(node => node.children[1].textContent);`,
    retryContext,
  );

  assert.deepEqual([...retryContext.rows], [
    "provider retry 1 of 3 · turn B remains active · waiting 2s",
  ]);
});

test("tool start and completion remain one nested code-block row", () => {
  function element(tagName) {
    const selectors = new Map();
    const node = {
      tagName,
      className: "",
      children: [],
      parentNode: null,
      hidden: false,
      textContent: "",
      append(...children) {
        for (const child of children) {
          if (typeof child === "string") continue;
          this.appendChild(child);
        }
      },
      appendChild(child) {
        if (child.parentNode && child.parentNode !== this) {
          child.parentNode.children = child.parentNode.children.filter(item => item !== child);
        }
        if (!this.children.includes(child)) this.children.push(child);
        child.parentNode = this;
        return child;
      },
      querySelector(selector) { return selectors.get(selector) ?? null; },
      setAttribute() {},
      addEventListener() {},
      classList: {
        add(token) {
          if (!node.className.split(" ").includes(token)) node.className += ` ${token}`;
        },
        toggle(token, force) {
          const tokens = node.className.split(" ").filter(Boolean).filter(item => item !== token);
          if (force) tokens.push(token);
          node.className = tokens.join(" ");
        },
      },
    };
    Object.defineProperty(node, "lastElementChild", {
      get() { return node.children.at(-1) ?? null; },
    });
    Object.defineProperty(node, "innerHTML", {
      set() {
        if (node.className.startsWith("tool")) {
          for (const selector of ["strong", ".badge", ".tool-head span:last-child", ".tool-summary", "pre"]) {
            selectors.set(selector, element(selector));
          }
        }
        if (node.className.startsWith("code-block")) {
          for (const selector of ["summary", ".code-source", ".code-output"]) {
            selectors.set(selector, element(selector));
          }
        }
      },
    });
    return node;
  }

  const timeline = element("timeline");
  const projectionContext = {
    Set,
    __LASH_WORKBENCH_TURN_EVENT_HOOK__() {},
    document: { createElement: element },
    timeline,
    pendingCodeBlock: null,
    pendingTools: [],
    clearEmpty() {},
    clearRetryStatus() {},
    scrollToEnd() {},
    refreshWork() {},
    renderMessage() {},
    appendReasoning() {},
  };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_TOOL_CODE_PROJECTION", "WORKBENCH_TOOL_CODE_PROJECTION")}
     ${markedSource("WORKBENCH_TURN_EVENT_DISPATCH", "WORKBENCH_TURN_EVENT_DISPATCH")}
     ${markedSource("WORKBENCH_SETTLED_TRANSCRIPT", "WORKBENCH_SETTLED_TRANSCRIPT")}
     handleTurnEvent(${JSON.stringify(turnEvents.codeStarted)}, "tool-turn");
     handleTurnEvent(${JSON.stringify(turnEvents.toolStarted)}, "tool-turn");
     this.startedCount = timeline.children[0].children.filter(child => child.className.startsWith("tool")).length;
     handleTurnEvent(${JSON.stringify(turnEvents.toolCompleted)}, "tool-turn");
     handleTurnEvent(${JSON.stringify(turnEvents.codeCompleted)}, "tool-turn");
     const codeBlock = timeline.children[0];
     const nestedTools = codeBlock.children.filter(child => child.className.startsWith("tool"));
     this.result = {
       nestedCount: nestedTools.length,
       siblingCount: timeline.children.filter(child => child.className.startsWith("tool")).length,
       badge: nestedTools[0]?.querySelector(".badge")?.textContent,
       summary: codeBlock.querySelector("summary").textContent
     };
     timeline.children = [];
     renderStateTranscript({ transcript: [
       { ...${JSON.stringify(turnEvents.codeCompleted)}, type: "code_block", tools: [${JSON.stringify(turnEvents.toolCompleted)}] }
     ] });
     const settledCodeBlock = timeline.children[0];
     const settledTools = settledCodeBlock.children.filter(child => child.className.startsWith("tool"));
     this.settledResult = {
       nestedCount: settledTools.length,
       siblingCount: timeline.children.filter(child => child.className.startsWith("tool")).length,
       badge: settledTools[0]?.querySelector(".badge")?.textContent
     };`,
    projectionContext,
  );

  assert.equal(projectionContext.startedCount, 1);
  assert.deepEqual(JSON.parse(JSON.stringify(projectionContext.result)), {
    nestedCount: 1,
    siblingCount: 0,
    badge: "completed",
    summary: "lashlang completed in 9ms · 1 tool",
  });
  assert.deepEqual(JSON.parse(JSON.stringify(projectionContext.settledResult)), {
    nestedCount: 1,
    siblingCount: 0,
    badge: "completed",
  });
});

test("tool start and completion without call id remain one nested row", () => {
  function element(tagName) {
    const selectors = new Map();
    const node = {
      tagName,
      className: "",
      children: [],
      parentNode: null,
      hidden: false,
      textContent: "",
      append(...children) {
        for (const child of children) {
          if (typeof child === "string") continue;
          this.appendChild(child);
        }
      },
      appendChild(child) {
        if (child.parentNode && child.parentNode !== this) {
          child.parentNode.children = child.parentNode.children.filter(item => item !== child);
        }
        if (!this.children.includes(child)) this.children.push(child);
        child.parentNode = this;
        return child;
      },
      querySelector(selector) { return selectors.get(selector) ?? null; },
      setAttribute() {},
      addEventListener() {},
      classList: {
        add(token) {
          if (!node.className.split(" ").includes(token)) node.className += ` ${token}`;
        },
        toggle(token, force) {
          const tokens = node.className.split(" ").filter(Boolean).filter(item => item !== token);
          if (force) tokens.push(token);
          node.className = tokens.join(" ");
        },
      },
    };
    Object.defineProperty(node, "lastElementChild", {
      get() { return node.children.at(-1) ?? null; },
    });
    Object.defineProperty(node, "innerHTML", {
      set() {
        if (node.className.startsWith("tool")) {
          for (const selector of ["strong", ".badge", ".tool-head span:last-child", ".tool-summary", "pre"]) {
            selectors.set(selector, element(selector));
          }
        }
        if (node.className.startsWith("code-block")) {
          for (const selector of ["summary", ".code-source", ".code-output"]) {
            selectors.set(selector, element(selector));
          }
        }
      },
    });
    return node;
  }

  const timeline = element("timeline");
  const projectionContext = {
    Set,
    __LASH_WORKBENCH_TURN_EVENT_HOOK__() {},
    document: { createElement: element },
    timeline,
    pendingCodeBlock: null,
    pendingTools: [],
    clearEmpty() {},
    clearRetryStatus() {},
    scrollToEnd() {},
    refreshWork() {},
    renderMessage() {},
    appendReasoning() {},
  };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_TOOL_CODE_PROJECTION", "WORKBENCH_TOOL_CODE_PROJECTION")}
     ${markedSource("WORKBENCH_TURN_EVENT_DISPATCH", "WORKBENCH_TURN_EVENT_DISPATCH")}
     handleTurnEvent(${JSON.stringify(turnEvents.codeStarted)}, "no-id-turn");
     handleTurnEvent(${JSON.stringify(turnEvents.noIdToolStarted)}, "no-id-turn");
     handleTurnEvent(${JSON.stringify(turnEvents.noIdToolCompleted)}, "no-id-turn");
     handleTurnEvent(${JSON.stringify(turnEvents.noIdCodeCompleted)}, "no-id-turn");
     const codeBlock = timeline.children[0];
     const nestedTools = codeBlock.children.filter(child => child.className.startsWith("tool"));
     this.result = {
       nestedCount: nestedTools.length,
       siblingCount: timeline.children.filter(child => child.className.startsWith("tool")).length,
       badge: nestedTools[0]?.querySelector(".badge")?.textContent,
       summary: codeBlock.querySelector("summary").textContent
     };`,
    projectionContext,
  );

  assert.deepEqual(JSON.parse(JSON.stringify(projectionContext.result)), {
    nestedCount: 1,
    siblingCount: 0,
    badge: "completed",
    summary: "lashlang completed in 10ms · 1 tool",
  });
});

test("Rust durable tool summaries render success, failure, and explicit omission honestly", () => {
  const codeRow = durableToolTranscript.find((row) => row.id === "durable-tool-trajectory");
  assert.deepEqual(codeRow.tools, [
    {
      kind: "durable_summary",
      operation: "durable.success",
      status: "success",
    },
    {
      kind: "durable_summary",
      operation: "durable.failure",
      status: "failure",
    },
    {
      kind: "omitted",
      count: 3,
    },
  ]);

  function element(tagName) {
    const selectors = new Map();
    const node = {
      tagName,
      className: "",
      children: [],
      parentNode: null,
      hidden: false,
      textContent: "",
      append(...children) {
        for (const child of children) {
          if (typeof child === "string") continue;
          this.appendChild(child);
        }
      },
      appendChild(child) {
        this.children.push(child);
        child.parentNode = this;
        return child;
      },
      querySelector(selector) { return selectors.get(selector) ?? null; },
      setAttribute() {},
      addEventListener() {},
      classList: {
        add(token) {
          if (!node.className.split(" ").includes(token)) node.className += ` ${token}`;
        },
        toggle(token, force) {
          const tokens = node.className.split(" ").filter(Boolean).filter(item => item !== token);
          if (force) tokens.push(token);
          node.className = tokens.join(" ");
        },
      },
    };
    Object.defineProperty(node, "innerHTML", {
      set() {
        if (node.className.startsWith("tool")) {
          for (const selector of ["strong", ".badge", ".tool-head span:last-child", ".tool-summary", "pre"]) {
            selectors.set(selector, element(selector));
          }
        }
        if (node.className.startsWith("code-block")) {
          for (const selector of ["summary", ".code-source", ".code-output"]) {
            selectors.set(selector, element(selector));
          }
        }
      },
    });
    return node;
  }

  const timeline = element("timeline");
  const projectionContext = {
    Set,
    document: { createElement: element },
    timeline,
    clearEmpty() {},
    scrollToEnd() {},
    refreshWork() {},
    renderMessage() {},
    appendReasoning() {},
  };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_TOOL_CODE_PROJECTION", "WORKBENCH_TOOL_CODE_PROJECTION")}
     ${markedSource("WORKBENCH_SETTLED_TRANSCRIPT", "WORKBENCH_SETTLED_TRANSCRIPT")}
     renderStateTranscript({ transcript: ${JSON.stringify(durableToolTranscript)} });`,
    projectionContext,
  );

  const codeBlock = timeline.children[0];
  const renderedTools = codeBlock.children.filter((child) => child.className.startsWith("tool"));
  assert.equal(codeBlock.querySelector("summary").textContent, "lashlang completed · 5 tools · 3 omitted");
  assert.equal(renderedTools.length, 3);
  assert.deepEqual(
    renderedTools.slice(0, 2).map((tool) => ({
      operation: tool.querySelector("strong").textContent,
      badge: tool.querySelector(".badge").textContent,
      availability: tool.querySelector(".tool-head span:last-child").textContent,
      payload: JSON.parse(tool.querySelector("pre").textContent),
    })),
    [
      {
        operation: "durable.success",
        badge: "completed",
        availability: "durable outcome only",
        payload: { status: "success" },
      },
      {
        operation: "durable.failure",
        badge: "failed",
        availability: "durable outcome only",
        payload: { status: "failure" },
      },
    ],
  );
  assert.equal(renderedTools[2].className, "tool omitted");
  assert.equal(renderedTools[2].textContent, "3 earlier tool calls omitted from durable history");
});

test("durable failure and postCommand client errors remain distinct rows", async () => {
  function element(tagName) {
    const node = {
      tagName,
      className: "",
      textContent: "",
      children: [],
      parentNode: null,
      append(...children) {
        for (const child of children) {
          if (typeof child !== "string") this.appendChild(child);
        }
      },
      appendChild(child) {
        this.children.push(child);
        child.parentNode = this;
        return child;
      },
      addEventListener(type, callback) {
        this.listeners ??= {};
        this.listeners[type] = callback;
      },
      closest() { return null; },
    };
    return node;
  }

  const timeline = element("timeline");
  let fetchCalls = 0;
  const projectionContext = {
    Set,
    document: { createElement: element },
    timeline,
    renderedMessages: new Set(),
    busy: false,
    resetInFlight: false,
    lastRequest: null,
    controller: null,
    assistantDraft: null,
    assistantDraftTurnId: null,
    assistantDraftText: "",
    assistantDraftChunks: [],
    reasoning: null,
    reasoningChunks: [],
    pendingCodeBlock: null,
    pendingTools: [],
    AbortController,
    clearEmpty() {},
    clearRetryStatus() {},
    cleanErrorText(message) { return message; },
    roleLabel(role) { return role; },
    setMessageBody(body, _role, text) { body.textContent = text; },
    renderMessageAttachments() {},
    scrollToEnd() {},
    renderNote() {},
    setBusy() {},
    refreshWork() {},
    async fetch() {
      fetchCalls += 1;
      if (fetchCalls === 1) {
        return { ok: false, async text() { return "refused"; } };
      }
      throw new TypeError("fetch failed");
    },
  };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_MESSAGE_RENDER", "WORKBENCH_MESSAGE_RENDER")}
     ${markedSource("WORKBENCH_CLIENT_ERROR_RENDER", "WORKBENCH_CLIENT_ERROR_RENDER")}
     ${markedSource("WORKBENCH_POST_COMMAND", "WORKBENCH_POST_COMMAND")}
     renderMessage({ id: "turn:failed", role: "event", text: "turn could not be completed", attachments: [] });
     this.httpFailure = postCommand("/api/turn", { text: "retry HTTP" });`,
    projectionContext,
  );
  await projectionContext.httpFailure;
  vm.runInNewContext(
    `this.fetchFailure = postCommand("/api/turn", { text: "retry fetch" });`,
    projectionContext,
  );
  await projectionContext.fetchFailure;

  assert.equal(timeline.children.length, 3);
  assert.equal(timeline.children[0].className, "message event");
  assert.equal(timeline.children[0].children[1].textContent, "turn could not be completed");
  assert.equal(timeline.children[0].children[1].children.length, 0);
  assert.equal(timeline.children[1].className, "message error");
  assert.equal(timeline.children[1].children[1].textContent, "request could not be completed");
  assert.equal(timeline.children[1].children[1].children[1].textContent, "retry turn");
  assert.equal(timeline.children[2].className, "message error");
  assert.equal(timeline.children[2].children[1].textContent, "request could not be completed");
  assert.equal(timeline.children[2].children[1].children[1].textContent, "retry turn");
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

test("real provider turns survive cursor replay, recovery races, terminal replacement, and production snapshot rendering", async () => {
  for (const scenario of executionEvidenceScenarios.providers) {
    const target = { textContent: "" };
    const snapshots = [];
    const scheduledStateRetries = [];
    const stateRetryDelays = [];
    let nextStateRetryTimerId = 0;
    let resolveDelayedSnapshot;
    let handledModelCalls = 0;
    const element = () => ({
      value: "",
      innerHTML: "",
      textContent: "",
      appendChild() {},
      addEventListener() {},
    });
    const scorecardContext = {
      Map,
      Set,
      Math,
      Number,
      executionScorecard: target,
      finishTransientRows() {},
      async fetchStateSnapshot() {
        const next = snapshots.shift();
        return next === "delayed"
          ? new Promise(resolve => { resolveDelayedSnapshot = resolve; })
          : next;
      },
      renderShellStatus() {},
      setTimeout(callback, delay) {
        const timer = { id: ++nextStateRetryTimerId, callback };
        scheduledStateRetries.push(timer);
        stateRetryDelays.push(delay);
        return timer.id;
      },
      clearTimeout(timerId) {
        const index = scheduledStateRetries.findIndex(timer => timer.id === timerId);
        if (index >= 0) scheduledStateRetries.splice(index, 1);
      },
      renderError() {},
      snapshotFailureReason(error) { return String(error); },
      __LASH_WORKBENCH_TURN_EVENT_HOOK__(event) {
        if (event.type === "model_call_recorded") handledModelCalls += 1;
      },
      modelInput: element(),
      variantSelect: element(),
      knownModels: new Set(),
      modelListenersBound: false,
      document: { getElementById: element, createElement: element },
      clearTerminalTurnTombstones() {},
      clearTranscript() {},
      validateModel() {},
      knownWebState: null,
      knownSessionLabel: null,
      renderUsage() {},
      renderQueuedWork() {},
      renderStateTranscript() {},
      renderIngressReceipt() {},
      recordTurnInputApplications() {},
      busy: false,
      streamGeneration: 1,
      restartEventStreams() {},
      setBusy(value) { this.busy = value; },
    };
    vm.runInNewContext(
      `${markedSource("WORKBENCH_PROJECTION_STATE", "WORKBENCH_PROJECTION_STATE")}
       ${markedSource("WORKBENCH_EXECUTION_SCORECARD", "WORKBENCH_EXECUTION_SCORECARD")}
       ${markedSource("WORKBENCH_SHELL_AVAILABILITY", "WORKBENCH_SHELL_AVAILABILITY")}
       this.executionScorecardState = createExecutionScorecardState();
       this.projectionState = createWorkbenchProjectionState();
       this.shellAvailability = createShellAvailability();
       this.renderedProductEvents = projectionState.renderedProductEvents;
       this.appliedObservationEvents = projectionState.appliedObservationEvents;
       ${markedSource("WORKBENCH_TURN_EVENT_REDUCER", "WORKBENCH_TURN_EVENT_REDUCER")}
       ${markedSource("WORKBENCH_STATE_SNAPSHOT", "WORKBENCH_STATE_SNAPSHOT")}
       ${markedSource("WORKBENCH_STATE_RETRY", "WORKBENCH_STATE_RETRY")}
       ${markedSource("WORKBENCH_REMOTE_STREAM_RECOVERY", "WORKBENCH_REMOTE_STREAM_RECOVERY")}`,
      scorecardContext,
    );
    scorecardContext.applyProjectionSnapshot(
      scorecardContext.projectionState,
      scenario.first_snapshot,
      true,
    );
    scorecardContext.markShellHydrated(scorecardContext.shellAvailability);

    const firstLine = JSON.stringify(scenario.first_observation_line);
    scorecardContext.handleObservationStreamLine(firstLine);
    scorecardContext.handleObservationStreamLine(firstLine);
    assert.equal(handledModelCalls, 1, "cursor dedupe must stop a second reducer dispatch");
    const firstRows = target.textContent
      .split("\n")
      .filter(row => row.includes(scenario.expected.first_call_id));
    assert.equal(firstRows.length, 2, "the real retry call must render both attempt rows");
    assert.match(firstRows[0], /#1 failed/);
    assert.match(firstRows[0], /position no_response/);
    assert.match(
      firstRows[0],
      new RegExp(`error ${scenario.expected.failed_attempt_error_class}`),
    );
    assert.match(firstRows[0], /retry scheduled/);
    assert.doesNotMatch(firstRows[0], / · (?:model|response|finish|reasoning) /);
    assert.match(firstRows[1], /#2 completed/);
    assert.match(firstRows[1], /position terminal_observed/);
    assert.match(firstRows[1], new RegExp(`model ${scenario.expected.served_model}`));
    assert.match(firstRows[1], new RegExp(`response ${scenario.expected.response_id}`));
    assert.match(firstRows[1], new RegExp(`finish ${scenario.expected.finish}`));
    assert.match(firstRows[1], /reasoning 0/);

    snapshots.push(scenario.first_snapshot);
    scorecardContext.handleObservationStreamLine(
      JSON.stringify(scenario.first_terminal_replacement_line),
    );
    await new Promise(resolve => setImmediate(resolve));
    assert.match(
      target.textContent,
      new RegExp(scenario.expected.first_call_id),
      "the production snapshot renderer must retain the first runtime ledger",
    );
    assert.equal(
      target.textContent
        .split("\n")
        .filter(row => row.includes(scenario.expected.first_call_id)).length,
      2,
      "terminal replacement must retain both retry attempts",
    );

    const retriesBeforeGap = scheduledStateRetries.length;
    snapshots.push("delayed");
    scorecardContext.executionScorecardState.delete(scenario.expected.first_call_id);
    scorecardContext.renderExecutionScorecard(
      scorecardContext.executionScorecardState,
      target,
    );
    assert.doesNotMatch(
      target.textContent,
      new RegExp(scenario.expected.first_call_id),
      "the replay gap must contain a scorecard row that only a snapshot can backfill",
    );
    scorecardContext.handleObservationStreamLine(JSON.stringify({
      type: "replay_gap",
      gap: { latest_cursor: scenario.first_snapshot.observation.cursor },
    }));
    await new Promise(resolve => setImmediate(resolve));
    scorecardContext.handleObservationStreamLine(
      JSON.stringify(scenario.second_observation_line),
    );
    assert.equal(handledModelCalls, 2);
    resolveDelayedSnapshot(scenario.first_snapshot);
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(
      scheduledStateRetries.length,
      retriesBeforeGap + 1,
      "an overtaken replay-gap snapshot must schedule a fresh recovery",
    );
    assert.match(
      target.textContent,
      new RegExp(scenario.expected.second_call_id),
      "an observation arriving during recovery must survive the older snapshot",
    );
    assert.doesNotMatch(
      target.textContent,
      new RegExp(scenario.expected.first_call_id),
      "the overtaken snapshot itself must not erase the newer observation",
    );

    snapshots.push("delayed");
    const firstRetry = scheduledStateRetries.shift();
    assert.ok(firstRetry, "the stale replay-gap response must arm the production retry");
    firstRetry.callback();
    await new Promise(resolve => setImmediate(resolve));
    const racingObservation = JSON.parse(JSON.stringify(scenario.second_observation_line));
    racingObservation.event.cursor += "-retry-race";
    scorecardContext.handleObservationStreamLine(JSON.stringify(racingObservation));
    resolveDelayedSnapshot(scenario.final_snapshot);
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(
      scheduledStateRetries.length,
      1,
      "a second stale response must keep the bounded production retry chain alive",
    );
    assert.equal(
      scorecardContext.shellAvailability.channels.state,
      false,
      "a stale retry must keep the state channel down while the gap row is missing",
    );
    assert.equal(
      scorecardContext.shellStatusModel(scorecardContext.shellAvailability).banner.hidden,
      false,
      "a stale retry must keep the reconnecting banner visible",
    );
    assert.doesNotMatch(
      target.textContent,
      new RegExp(scenario.expected.first_call_id),
      "the second stale response must not pretend the missing row was repaired",
    );

    snapshots.push(scenario.final_snapshot);
    const secondRetry = scheduledStateRetries.shift();
    assert.ok(secondRetry, "the second stale response must arm the next bounded retry");
    secondRetry.callback();
    await new Promise(resolve => setImmediate(resolve));
    assert.match(
      target.textContent,
      new RegExp(scenario.expected.first_call_id),
      "the armed production retry must backfill the scorecard row after a quiet window",
    );
    assert.match(
      target.textContent,
      new RegExp(scenario.expected.second_call_id),
      "fresh gap recovery must retain the observation that overtook the stale snapshot",
    );
    assert.deepEqual(
      target.textContent
        .split("\n")
        .filter(row => row.includes(scenario.expected.first_call_id))
        .map(row => row.match(/#\d+/)?.[0]),
      ["#1", "#2"],
      "gap recovery must restore retry attempt identity and order",
    );
    assert.deepEqual(
      stateRetryDelays,
      [900, 1800],
      "stale recovery must retain the production retry backoff",
    );
    assert.equal(scorecardContext.shellAvailability.channels.state, true);
    assert.equal(
      scorecardContext.shellStatusModel(scorecardContext.shellAvailability).banner.hidden,
      true,
      "the successful retry must clear the reconnecting banner",
    );
    assert.equal(scheduledStateRetries.length, 0);

    snapshots.push(scenario.final_snapshot);
    scorecardContext.handleObservationStreamLine(
      JSON.stringify(scenario.second_terminal_replacement_line),
    );
    await new Promise(resolve => setImmediate(resolve));
    for (const callId of [scenario.expected.first_call_id, scenario.expected.second_call_id]) {
      assert.match(target.textContent, new RegExp(callId));
    }
    assert.equal(
      target.textContent.split("\n").length,
      3,
      "the real terminal replacement must converge to two retry rows plus the second call",
    );
  }
});

test("execution scorecard ordering follows attempt start time with call id as tie-breaker", () => {
  const target = { textContent: "" };
  const scorecardContext = { Map };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_EXECUTION_SCORECARD", "WORKBENCH_EXECUTION_SCORECARD")}
     this.scorecard = createExecutionScorecardState();`,
    scorecardContext,
  );
  for (const [call_id, started_at_ms] of [["a-call", 30], ["!call", 20], ["A-call", 10]]) {
    scorecardContext.applyExecutionScorecardRecord(scorecardContext.scorecard, {
      call_id,
      attempts: [{ ordinal: 1, started_at_ms, outcome: "completed", evidence: { reasoning_output_tokens: 0 } }],
    });
  }
  scorecardContext.renderExecutionScorecard(scorecardContext.scorecard, target);
  assert.deepEqual(
    target.textContent.split("\n").map(line => line.split(" #", 1)[0]),
    ["A-call", "!call", "a-call"],
  );
});

test("execution scorecard explains collection interruption only when reported", () => {
  const target = { textContent: "" };
  const scorecardContext = { Map, Math, Number };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_EXECUTION_SCORECARD", "WORKBENCH_EXECUTION_SCORECARD")}
     this.scorecard = createExecutionScorecardState();`,
    scorecardContext,
  );
  scorecardContext.applyExecutionScorecardRecord(scorecardContext.scorecard, {
    call_id: "partial",
    attempts: [{
      ordinal: 1,
      started_at_ms: 1,
      outcome: "aborted",
      evidence: { collection_interruption: "protocol_abort" },
    }],
  });
  scorecardContext.applyExecutionScorecardRecord(scorecardContext.scorecard, {
    call_id: "complete",
    attempts: [{ ordinal: 1, started_at_ms: 2, outcome: "completed", evidence: {} }],
  });
  scorecardContext.renderExecutionScorecard(scorecardContext.scorecard, target);
  assert.equal(target.textContent.match(/collection interrupted:/g)?.length, 1);
  assert.match(target.textContent, /collection interrupted: protocol_abort/);
});

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

test("message attachments render as linked images and degrade visibly on load failure", () => {
  function element(tagName) {
    return {
      tagName,
      children: [],
      dataset: {},
      hidden: false,
      listeners: {},
      append(...children) {
        this.children.push(...children);
      },
      appendChild(child) {
        this.children.push(child);
      },
      addEventListener(name, callback) {
        this.listeners[name] = callback;
      },
    };
  }

  const body = element("div");
  const renderContext = { document: { createElement: element }, body };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_MESSAGE_ATTACHMENTS", "WORKBENCH_MESSAGE_ATTACHMENTS")}
     renderMessageAttachments(body, [{
       attachment_id: "sha256:fig994-browser",
       retrieve_url: "/api/attachments/sha256:fig994-browser"
     }]);`,
    renderContext,
  );

  const gallery = body.children[0];
  const link = gallery.children[0];
  const image = link.children[0];
  const broken = link.children[1];
  assert.equal(gallery.className, "message-attachments");
  assert.equal(link.href, "/api/attachments/sha256:fig994-browser");
  assert.equal(link.target, "_blank");
  assert.equal(link.rel, "noopener");
  assert.equal(link.dataset.attachmentId, "sha256:fig994-browser");
  assert.equal(image.src, link.href);
  assert.equal(image.alt, "Uploaded image attachment");
  assert.equal(broken.hidden, true);

  image.listeners.error();
  assert.equal(image.hidden, true);
  assert.equal(broken.hidden, false);
  assert.equal(broken.textContent, "Image unavailable · open original");
});

test("a committed RLM printed-image message numbers multiple image alt labels", () => {
  function element(tagName) {
    return {
      tagName,
      children: [],
      dataset: {},
      hidden: false,
      append(...children) {
        this.children.push(...children);
      },
      appendChild(child) {
        this.children.push(child);
      },
      addEventListener() {},
    };
  }

  const body = element("div");
  const renderContext = {
    document: { createElement: element },
    body,
    attachments: multiAttachmentMessage.attachments,
  };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_MESSAGE_ATTACHMENTS", "WORKBENCH_MESSAGE_ATTACHMENTS")}
     renderMessageAttachments(body, attachments);`,
    renderContext,
  );

  const [first, second] = body.children[0].children;
  assert.equal(first.children[0].alt, "Uploaded image attachment 1");
  assert.equal(second.children[0].alt, "Uploaded image attachment 2");
  assert.equal(first.dataset.attachmentId, "sha256:rlm-printed-image-a");
  assert.equal(second.dataset.attachmentId, "sha256:rlm-printed-image-b");
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
    markStreamingUsageSettled() {},
    clearRetryStatus() {},
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
    `${markedSource("WORKBENCH_TERMINAL_TURN_TOMBSTONES", "WORKBENCH_TERMINAL_TURN_TOMBSTONES")}
     ${markedSource("WORKBENCH_TRANSIENT_SETTLEMENT", "WORKBENCH_TRANSIENT_SETTLEMENT")}
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
    markStreamingUsageSettled() {},
    clearRetryStatus() {},
    reasoningChunks: [],
    pendingCodeBlock: null,
    reasoning: null,
    renderMessage() {},
    renderIngressReceipt() {},
    setBusy() {},
    refreshUsage() {},
  };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_TERMINAL_TURN_TOMBSTONES", "WORKBENCH_TERMINAL_TURN_TOMBSTONES")}
     ${markedSource("WORKBENCH_TRANSIENT_SETTLEMENT", "WORKBENCH_TRANSIENT_SETTLEMENT")}
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

/* FIG-1000: a turn that failed committed nothing its optimistic rows can stand
   for, and the server has already retired them from the product lane. A tab that
   still renders them has exactly one way to drop a rendered row — re-deriving
   from the authoritative snapshot — so the `failed` outcome must reach
   `recoverFromState`, and a completed turn must not pay for that re-derivation. */
function doneReducerContext(outcome) {
  const recoveries = [];
  const reducerContext = {
    Set,
    projectionState: createWorkbenchProjectionState(),
    renderedProductEvents: new Set(),
    assistantDraft: null,
    assistantDraftTurnId: null,
    assistantDraftText: "",
    assistantDraftChunks: [],
    pendingTools: [],
    appendTool() {},
    markStreamingUsageSettled() {},
    clearRetryStatus() {},
    reasoningChunks: [],
    pendingCodeBlock: null,
    reasoning: null,
    renderMessage() {},
    renderIngressReceipt() {},
    setBusy() {},
    refreshUsage() {},
    recoverFromState(message) {
      recoveries.push(message);
    },
  };
  vm.runInNewContext(
    `${markedSource("WORKBENCH_TERMINAL_TURN_TOMBSTONES", "WORKBENCH_TERMINAL_TURN_TOMBSTONES")}
     ${markedSource("WORKBENCH_TRANSIENT_SETTLEMENT", "WORKBENCH_TRANSIENT_SETTLEMENT")}
     ${markedSource("WORKBENCH_PRODUCT_EVENT_REDUCER", "WORKBENCH_PRODUCT_EVENT_REDUCER")}
     applyProductEvent({
       event_id: "refused-turn-done",
       sequence: 1,
       type: "done",
       turn_id: "refused-turn",
       outcome: ${JSON.stringify(outcome)}
     });`,
    reducerContext,
  );
  return recoveries;
}

test("a failed turn's Done re-derives the transcript from durable truth", () => {
  const recoveries = doneReducerContext("failed");
  assert.equal(recoveries.length, 1);
  assert.match(recoveries[0], /turn failed/);
});

test("a completed turn's Done re-derives nothing", () => {
  assert.deepEqual(doneReducerContext("completed"), []);
  assert.deepEqual(doneReducerContext(undefined), []);
});
