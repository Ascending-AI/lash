// The production timeline module (assets/timeline.js) over a tree DOM: the
// FIG-5086 laws (timeline_laws.mjs), and the rules the conversation keeps
// for each source it projects, fed with the wire shapes the Rust gate
// serializes.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import vm from "node:vm";
import { createFakeDocument } from "./fake_dom.mjs";
import { laws } from "./timeline_laws.mjs";

const source = readFileSync(new URL("../assets/timeline.js", import.meta.url), "utf8");
const turnEvents = JSON.parse(process.env.LASH_WORKBENCH_TURN_EVENTS ?? "null");
assert.ok(turnEvents, "LASH_WORKBENCH_TURN_EVENTS must come from the Rust projection gate");
const durableToolTranscript = JSON.parse(process.env.LASH_WORKBENCH_DURABLE_TOOL_TRANSCRIPT ?? "null");
assert.ok(durableToolTranscript, "LASH_WORKBENCH_DURABLE_TOOL_TRANSCRIPT must come from a committed Rust trajectory");
const multiAttachmentMessage = JSON.parse(process.env.LASH_WORKBENCH_MULTI_ATTACHMENT_MESSAGE ?? "null");
assert.ok(multiAttachmentMessage, "LASH_WORKBENCH_MULTI_ATTACHMENT_MESSAGE must come from the Rust projection gate");

const document = createFakeDocument();
const context = { document, setTimeout, clearTimeout, console };
vm.runInNewContext(source, context);

function mount(hooks = {}) {
  const list = document.createElement("div");
  const footer = document.createElement("div");
  const empty = document.createElement("div");
  document.body.append(list, footer, empty);
  const timeline = context.createWorkbenchTimeline({ list, footer, empty, hooks });
  return { timeline, list, footer, empty };
}

const env = {
  assert,
  timeline: time => mount({ now: time.now }),
  removals(list) {
    const start = document.removals.length;
    return () => document.removals.slice(start).filter(removal => removal.parent === list).map(removal => removal.node.dataset.key);
  }
};

for (const law of laws) test(law.name, () => law.run(env));

let cursor = 0;
const observe = (turnId, activity) => ({
  session_id: "rules", replay_incarnation_id: "rules", turn_id: turnId, cursor: `rules-${++cursor}`,
  type: "turn_activity", activity
});
const commit = (turnId, rows) => ({
  session_id: "rules", replay_incarnation_id: "rules", turn_id: turnId, cursor: `rules-${++cursor}`, type: "committed", rows
});
let productSequence = 0;
const message = msg => ({ event_id: `rules-p-${++productSequence}`, sequence: productSequence, type: "message", message: msg });
const done = (turnId, outcome) => ({ event_id: `rules-done-${++productSequence}`, sequence: productSequence, type: "done", turn_id: turnId, outcome });
const row = (row_id, kind, turnId, content = {}, provenance = {}) => ({
  row_id, kind, timestamp: "2026-10-05T12:00:00Z", suppressed: null,
  provenance: { turn_id: turnId, input_id: null, plugin_id: null, is_turn_reply: kind === "assistant_reply", ...provenance },
  content: {
    text: "", reasoning: [], attachments: [], language: null, code: null, output: null, success: null,
    error: null, tools: [], tools_omitted: 0, ...content
  }
});
const visible = (list, selector) => list.querySelectorAll(selector).filter(node => !node.hidden);
const pendingInput = (inputId, text, status, scope = "next_turn") => ({
  input: { input_id: inputId, ingress: { scope }, state: "deferred_next_turn", input: { items: [{ type: "text", text }] } },
  status
});
const snapshotOf = fields => ({
  transcript: [], product_events: { cursor: productSequence, events: [] }, active_turns: [], pending_turn_inputs: [],
  turn_input_applications: [], unknown_turn_terminals: [], ...fields
});

// A provider failure is reported by the shared event row every viewer
// receives; the activity stream adds no client error row of its own, so the
// sender and an observer see the same timeline.
test("provider failure settles to the same durable row for sender and observer, and usage reaches the page", () => {
  function project() {
    const forwarded = [];
    const view = mount({ turnActivity: (event, turnId) => forwarded.push([event.type, turnId]) });
    view.timeline.applyObservation(observe("failed-turn", turnEvents.usage));
    view.timeline.applyObservation(observe("failed-turn", turnEvents.error));
    view.timeline.applyProductEvent(message({ id: "turn:failed", role: "event", text: "turn could not be completed", attachments: [] }));
    view.timeline.applyProductEvent(done("failed-turn", "failed"));
    return { forwarded, rows: view.list.children.map(node => [node.className, node.textContent]) };
  }
  const sender = project();
  const observer = project();
  assert.deepEqual(sender.rows, [["message event", "eventturn could not be completed"]]);
  assert.deepEqual(observer, sender);
  assert.deepEqual(sender.forwarded, [["usage", "failed-turn"], ["error", "failed-turn"]]);
});

// A request the page itself could not complete is its own row, each time,
// beside the durable failure row; neither replaces the other.
test("client request errors and a durable failure remain distinct rows", () => {
  const view = mount();
  view.timeline.applyProductEvent(message({ id: "turn:failed", role: "event", text: "turn could not be completed", attachments: [] }));
  view.timeline.error("request could not be completed", { retry: () => {} });
  view.timeline.error("request could not be completed", { retry: () => {} });
  assert.deepEqual(view.list.children.map(node => node.className), ["message event", "message error", "message error"]);
  assert.deepEqual(view.list.querySelectorAll("button.retry").map(button => button.textContent), ["retry turn", "retry turn"]);
});

test("a model attempt reset retracts only the superseded partial text, and the retry shows its status", () => {
  const view = mount();
  const turn = "retry-turn";
  const prose = (text, correlation_id) => observe(turn, { type: "assistant_prose_delta", text, correlation_id });
  const reasoning = (text, correlation_id) => observe(turn, { type: "reasoning_delta", text, correlation_id });
  view.timeline.applyObservation(reasoning("superseded reasoning", "reasoning-superseded"));
  view.timeline.applyObservation(reasoning("retained reasoning", "reasoning-retained"));
  view.timeline.applyObservation(prose("superseded prose ", "prose-superseded"));
  view.timeline.applyObservation(prose("retained prose", "prose-retained"));
  view.timeline.applyObservation(observe(turn, turnEvents.reset));
  view.timeline.applyObservation(observe(turn, turnEvents.retry));
  assert.deepEqual(visible(view.list, "details.reasoning").map(node => node.textContent), ["thinkingretained reasoning"]);
  assert.equal(view.timeline.nodeOf(`reply:${turn}`).querySelector(".msg-text").textContent.trim(), "retained prose");
  const status = visible(view.list, ".retry-status");
  assert.equal(status.length, 1);
  assert.match(status[0].textContent, /provider retry 1 of 3.*deterministic retry law · waiting 2s/);
});

test("a retry status belongs to its own turn", () => {
  const view = mount();
  const retry = (turnId, attempt, reason) => observe(turnId, { ...turnEvents.retry, attempt, reason });
  const shown = () => visible(view.list, ".retry-status").map(node => node.textContent.replace(/^provider /, "").replace(/\d\d:\d\d.*$/, ""));
  view.timeline.applyObservation(retry("turn-a", 1, "turn A retry"));
  view.timeline.applyObservation(retry("turn-b", 1, "turn B retry"));
  view.timeline.applyObservation(retry("turn-b", 2, "turn B retry"));
  assert.equal(shown().length, 2);
  assert.match(shown()[1], /retry 2 of 3/);
  // Turn A settles: its status retires, turn B's stays.
  view.timeline.applyProductEvent(done("turn-a"));
  view.timeline.applyObservation(commit("turn-a", []));
  assert.equal(shown().length, 1);
  assert.match(shown()[0], /turn B retry/);
  // Another turn's next request does not clear it; its own does.
  view.timeline.applyObservation(observe("turn-a", { type: "model_request_started", protocol_iteration: 2 }));
  assert.equal(shown().length, 1);
  view.timeline.applyObservation(observe("turn-b", { type: "model_request_started", protocol_iteration: 2 }));
  assert.equal(shown().length, 0);
  // A status for a turn that already reported `done` is stale.
  view.timeline.applyObservation(retry("turn-a", 3, "late turn A retry"));
  assert.equal(shown().length, 0);
});

test("a tool call is one row nested in its code block, live and after the commit", () => {
  const view = mount();
  const turn = "tool-turn";
  view.timeline.applyObservation(observe(turn, turnEvents.codeStarted));
  view.timeline.applyObservation(observe(turn, turnEvents.toolStarted));
  const block = view.timeline.nodeOf(`code:${turn}:0`);
  assert.equal(block.querySelectorAll("div.tool").length, 1);
  view.timeline.applyObservation(observe(turn, turnEvents.toolCompleted));
  view.timeline.applyObservation(observe(turn, turnEvents.codeCompleted));
  const tool = block.querySelector(".tool");
  const live = { tools: block.querySelectorAll(".tool").length, badge: tool.querySelector(".badge").textContent, summary: block.querySelector("span").textContent };
  assert.deepEqual(live, { tools: 1, badge: "completed", summary: "typescript completed in 9ms · 1 tool" });
  view.timeline.applyProductEvent(done(turn));
  view.timeline.applyObservation(commit(turn, [row("settled-code", "code_block", turn, {
    language: "typescript", code: turnEvents.codeStarted.code, output: "completed", success: true,
    tools: [{ operation: turnEvents.toolCompleted.name, status: "success" }]
  })]));
  assert.equal(view.list.children.length, 1);
  assert.equal(view.timeline.nodeOf(`code:${turn}:0`), block);
  assert.deepEqual(block.querySelectorAll(".tool"), [tool]);
  assert.equal(block.querySelector("span").textContent, "typescript completed in 9ms · 1 tool");
  assert.equal(block.dataset.transcriptRowId, "settled-code");
});

test("durable tool summaries render success, failure, and explicit omission honestly", () => {
  const codeRow = durableToolTranscript.find(entry => entry.kind === "code_block" && entry.content.code === "durable.tool_projection()");
  assert.deepEqual(codeRow.content.tools, [
    { operation: "durable.success", status: "success" },
    { operation: "durable.failure", status: "failure" }
  ]);
  assert.equal(codeRow.content.tools_omitted, 3);
  const view = mount();
  view.timeline.applySnapshot(snapshotOf({ transcript: durableToolTranscript }));
  const block = view.list.querySelectorAll("details.code-block").find(node => node.dataset.transcriptRowId === codeRow.row_id);
  assert.equal(block.querySelector("span").textContent, "typescript completed · 5 tools · 3 omitted");
  const tools = block.querySelectorAll(".tool");
  assert.equal(tools.length, 3);
  assert.deepEqual(tools.slice(0, 2).map(tool => ({
    operation: tool.querySelector("strong").textContent,
    badge: tool.querySelector(".badge").textContent,
    availability: tool.querySelector(".tool-head").children[2].textContent,
    payload: JSON.parse(tool.querySelector("pre").textContent)
  })), [
    { operation: "durable.success", badge: "completed", availability: "durable outcome only", payload: { status: "success" } },
    { operation: "durable.failure", badge: "failed", availability: "durable outcome only", payload: { status: "failure" } }
  ]);
  assert.equal(tools[2].className, "tool omitted");
  assert.equal(tools[2].textContent, "3 earlier tool calls omitted from durable history");
  assert.deepEqual(
    block.querySelector(".message-attachments").children.map(link => link.dataset.attachmentId),
    codeRow.content.attachments.map(attachment => attachment.id)
  );
  const failed = mount();
  failed.timeline.applySnapshot(snapshotOf({ transcript: [{ ...codeRow, row_id: "failed-cell", content: { ...codeRow.content, error: "canonical cell failure", success: false } }] }));
  const failedBlock = failed.list.querySelector("details.code-block");
  assert.ok(failedBlock.querySelector(".code-output").textContent.includes("canonical cell failure"));
  assert.ok(failedBlock.classList.contains("fail"));
});

test("committed rows render canonical reasoning and code disclosure, each carrying its row id", () => {
  const view = mount();
  view.timeline.applySnapshot(snapshotOf({
    transcript: [
      row("committed-user", "user", null, { text: "question" }),
      row("reasoning-1", "reasoning", null, { reasoning: ["durable thought"] }),
      row("code-1", "code_block", null, { code: 'print("durable")', output: "durable" })
    ]
  }));
  assert.deepEqual(view.list.children.map(node => [node.className, node.dataset.transcriptRowId]), [
    ["message user", "committed-user"], ["reasoning", "reasoning-1"], ["code-block", "code-1"]
  ]);
  assert.equal(view.list.children[1].querySelector("pre").textContent, "durable thought");
  assert.equal(view.list.children[2].querySelector(".code-source").textContent, 'print("durable")');
  assert.equal(view.list.children[2].querySelector(".code-output").textContent, "durable");
});

// The read a replay gap takes began before the live event it did not see:
// the row that event made stays.
test("a snapshot overtaken by a live event cannot erase its row", () => {
  const view = mount();
  const since = view.timeline.epoch();
  view.timeline.applyObservation(observe("overtaking-turn", { type: "assistant_prose_delta", text: "fresh", correlation_id: "p" }));
  view.timeline.applySnapshot(snapshotOf({}), since);
  assert.equal(visible(view.list, ".message.assistant").length, 1);
  view.timeline.applySnapshot(snapshotOf({ active_turns: [{ turn_id: "overtaking-turn" }] }), view.timeline.epoch());
  assert.equal(visible(view.list, ".message.assistant").length, 1, "a read that lists the turn running keeps its live row");
  view.timeline.applySnapshot(snapshotOf({}), view.timeline.epoch());
  assert.equal(visible(view.list, ".message.assistant").length, 0, "a read that saw the turn end retires its live row");
});

test("busy follows the turn, and a read begun before its done cannot re-arm it", () => {
  const changes = [];
  const view = mount({ busyChanged: next => changes.push(next) });
  view.timeline.beginSend({ nonce: "busy-1", text: "hello", attachments: [] });
  assert.equal(view.timeline.busy(), true, "busy before the request leaves");
  view.timeline.sendAccepted("busy-1", { accepted: true, turn_id: "busy-turn" });
  assert.equal(view.timeline.busy(), true);
  const since = view.timeline.epoch();
  view.timeline.applyProductEvent(done("busy-turn"));
  assert.equal(view.timeline.busy(), false);
  view.timeline.applySnapshot(snapshotOf({ active_turns: [{ turn_id: "busy-turn" }] }), since);
  assert.equal(view.timeline.busy(), false, "a read older than the done re-armed the turn");
  assert.deepEqual(changes, [true, false]);
  // A turn known only from the read that lists it running, which a later
  // read no longer lists, is idle.
  view.timeline.applySnapshot(snapshotOf({ active_turns: [{ turn_id: "other-turn" }] }));
  assert.equal(view.timeline.busy(), true);
  view.timeline.applySnapshot(snapshotOf({}), view.timeline.epoch());
  assert.equal(view.timeline.busy(), false);
});

test("turn A's done does not retire turn B's live prose", () => {
  const view = mount();
  view.timeline.applyObservation(observe("turn-a", { type: "assistant_prose_delta", text: "A text", correlation_id: "a" }));
  view.timeline.applyObservation(observe("turn-b", { type: "assistant_prose_delta", text: "B text", correlation_id: "b" }));
  view.timeline.applyProductEvent(done("turn-a", "failed"));
  assert.deepEqual(visible(view.list, ".message.assistant").map(node => node.textContent.replace(/^agent/, "").trim()), ["B text"]);
});

// FIG-1000: a failed turn committed nothing its provisional rows stand for,
// and the server has already retired its product rows.
test("a failed turn's provisional rows retire on its done", () => {
  const view = mount();
  view.timeline.beginSend({ nonce: "fail-1", text: "doomed", attachments: [] });
  view.timeline.applyProductEvent(message({ id: "workbench-user:fail-turn", role: "user", text: "doomed", client_nonce: "fail-1", provenance: { kind: "turn_input", turn_id: "fail-turn" } }));
  view.timeline.sendAccepted("fail-1", { accepted: true, turn_id: "fail-turn" });
  view.timeline.applyObservation(observe("fail-turn", { type: "assistant_prose_delta", text: "partial", correlation_id: "p" }));
  view.timeline.applyProductEvent(message({ id: "turn:failed", role: "event", text: "turn could not be completed", attachments: [] }));
  view.timeline.applyProductEvent(done("fail-turn", "failed"));
  assert.deepEqual([...view.timeline.rowKeys()], ["msg:turn:failed"]);
});

test("pending input receipts wait in the footer until their turn takes them", () => {
  const view = mount();
  const receipts = () => view.footer.children.map(node => [node.dataset.inputId, node.dataset.status, node.children[0].textContent, node.children[1].textContent]);
  view.timeline.applySnapshot(snapshotOf({
    pending_turn_inputs: [
      pendingInput("input-now", "injected now", { kind: "held", shift_epoch: 7 }, "active_turn"),
      pendingInput("input-next", "queued next", { kind: "pending" })
    ]
  }));
  assert.deepEqual(receipts(), [
    ["input-now", "held", "injected now · held under epoch 7", "injected now"],
    ["input-next", "pending", "queued next", "queued next"]
  ]);
  const nodes = view.footer.children;
  view.timeline.applySnapshot(snapshotOf({
    pending_turn_inputs: [
      pendingInput("input-now", "injected now", { kind: "held", shift_epoch: 7 }, "active_turn"),
      pendingInput("input-next", "queued next", { kind: "pending" })
    ],
    turn_input_applications: [{ input_id: "input-now", committed_message_id: "turn-1-user" }]
  }), view.timeline.epoch());
  assert.equal(receipts()[0][2], "applied to turn");
  assert.deepEqual(view.footer.children, nodes, "a re-read replaced a receipt");
  view.timeline.applyObservation(commit("turn-1", [row("turn-1-user", "user", "turn-1", { text: "injected now" }, { input_id: "input-now" })]));
  assert.deepEqual(receipts().map(receipt => receipt[0]), ["input-next"]);
  view.timeline.applySnapshot(snapshotOf({}), view.timeline.epoch());
  assert.deepEqual(receipts(), []);
});

// FIG-5036: a send to an idle session is admitted to the turn it starts at
// once; the pending-input read lists it as admitted. It is that turn's user
// row, never also a "queued next" card.
test("input a run already admitted is its run's user row, never a queued receipt", () => {
  const view = mount();
  const text = "let me know when a button is pressed";
  view.timeline.applySnapshot(snapshotOf({
    product_events: { cursor: 1, events: [message({ id: "user-2", role: "user", text, provenance: { kind: "turn_input", turn_id: "turn-2" } })] },
    pending_turn_inputs: [pendingInput("ti:2", text, { kind: "admitted", run: "turn-2" })]
  }));
  assert.equal(view.footer.children.length, 0);
  assert.equal(view.list.querySelectorAll(".message.user").length, 1);

  // Input waiting behind a running turn is a receipt until its run starts;
  // then it is that run's user row, and the run's commit is the same row.
  view.timeline.showReceipt({ accepted: true, input_id: "ti:3", ingress: { scope: "next_turn" }, text: "now say ok" });
  assert.deepEqual(view.footer.children.map(node => node.textContent), ["queued nextnow say ok"]);
  view.timeline.applySnapshot(snapshotOf({
    product_events: { cursor: 1, events: [message({ id: "user-2", role: "user", text, provenance: { kind: "turn_input", turn_id: "turn-2" } })] },
    pending_turn_inputs: [pendingInput("ti:3", "now say ok", { kind: "admitted", run: "turn-3" })]
  }), view.timeline.epoch());
  assert.equal(view.footer.children.length, 0);
  const promoted = view.timeline.nodeOf("input:turn-3");
  assert.equal(promoted.dataset.turnId, "turn-3");
  view.timeline.applyProductEvent(message({ id: "user-3", role: "user", text: "now say ok", provenance: { kind: "turn_input", turn_id: "turn-3" } }));
  assert.equal(view.list.querySelectorAll(".message.user").length, 2);
  assert.equal(view.timeline.nodeOf("input:turn-3"), promoted);
  assert.ok(!promoted.classList.contains("pending"));
});

const OCCURRENCE = {
  id: "trigger:workbench-button-trigger:press-1",
  role: "event",
  text: "red pressed",
  at: null,
  provenance: { kind: "trigger_occurrence", occurrence_id: "trigger:workbench-button-trigger:press-1", process_ids: ["p_watch_1"] }
};
const WAKE_STARTED = {
  type: "queued_work_started",
  boundary: "idle",
  batch_ids: ["qwb:1"],
  causes: [{
    id: "wake:1",
    event_type: "process.yield",
    origin: {
      kind: "process", process_id: "p_watch_1", event_type: "process.yield", sequence: 4, wake_id: "wake:1",
      caused_by: {
        type: "trigger_occurrence", occurrence_id: OCCURRENCE.id, subscription_id: "trigger-subscription:watch",
        subscription_incarnation: "incarnation:watch", subscription_revision: 1
      }
    },
    text: "Background process wake\nProcess: p_watch_1\nEvent: process.yield #4\nWake input:\n{\"button\":\"Red\"}"
  }]
};

// FIG-5036: one press rendered three rows. The occurrence is one row, and
// the turn its processes start folds into it by the cause's typed
// occurrence id, whichever arrives first.
test("one trigger occurrence is one row, and the turn it started folds into it", () => {
  for (const order of [["occurrence", "wake"], ["wake", "occurrence"]]) {
    const view = mount();
    for (const step of order) {
      if (step === "occurrence") view.timeline.applyProductEvent(message(OCCURRENCE));
      else view.timeline.applyObservation(observe("shift-run-1", WAKE_STARTED));
    }
    const rows = view.list.querySelectorAll(".message.event");
    assert.equal(rows.length, 1, order.join(" then "));
    assert.equal(rows[0].dataset.turnStarted, "true");
    assert.match(rows[0].textContent, /^red pressed→ turn started/);
    assert.match(rows[0].textContent, /wake input: \{"button":"Red"\}/);
  }
});

test("message attachments render as linked images and degrade visibly on load failure", () => {
  const body = document.createElement("div");
  context.renderMessageAttachments(body, [{
    attachment_id: "sha256:fig994-browser",
    retrieve_url: "/api/attachments/sha256:fig994-browser"
  }]);
  const gallery = body.children[0];
  const link = gallery.children[0];
  const [image, broken] = link.children;
  assert.equal(gallery.className, "message-attachments");
  assert.equal(link.href, "/api/attachments/sha256:fig994-browser");
  assert.equal(link.target, "_blank");
  assert.equal(link.rel, "noopener");
  assert.equal(link.dataset.attachmentId, "sha256:fig994-browser");
  assert.equal(image.src, link.href);
  assert.equal(image.alt, "Uploaded image attachment");
  assert.equal(broken.hidden, true);
  image.dispatch("error");
  assert.equal(image.hidden, true);
  assert.equal(broken.hidden, false);
  assert.equal(broken.textContent, "Image unavailable · open original");
});

test("a committed RLM printed-image message numbers multiple image alt labels", () => {
  const body = document.createElement("div");
  context.renderMessageAttachments(body, multiAttachmentMessage.attachments);
  const [first, second] = body.children[0].children;
  assert.equal(first.children[0].alt, "Uploaded image attachment 1");
  assert.equal(second.children[0].alt, "Uploaded image attachment 2");
  assert.equal(first.dataset.attachmentId, "sha256:rlm-printed-image-a");
  assert.equal(second.dataset.attachmentId, "sha256:rlm-printed-image-b");
});
