// FIG-5086: the workbench timeline is one keyed projection, patched in place,
// in causal order. Each law drives the production timeline module through the
// sources a page feeds it (a send, the product lane, live observations, a
// turn's committed rows, an authoritative snapshot) and reads the DOM it
// produces. The same laws run in Node over a small tree DOM
// (timeline_projection.mjs) and in Chromium over the real DOM
// (timeline_browser.py).
//
// `env` supplies: `assert`, `timeline(clock)` → { timeline, list, footer },
// and `removals(list)` → () => [keys of rows removed from the list since].
//
// A turn settles as the streams usually deliver it: its reply and `done` on
// the product lane, then its commit, which retires its live-only rows at
// once (the grace after `done` covers a commit that has not arrived yet).

const T0 = Date.parse("2026-10-05T12:00:00.000Z");
const iso = ms => new Date(ms).toISOString();

function clock() {
  const state = { now: T0 };
  return { now: () => state.now, advance: ms => { state.now += ms; return state.now; } };
}

let sequence = 0;
const product = (message, extra = {}) => ({ event_id: `p-${++sequence}`, sequence, type: "message", message, ...extra });
const done = (turnId, outcome) => ({ event_id: `done-${turnId}-${++sequence}`, sequence, type: "done", turn_id: turnId, ...(outcome ? { outcome } : {}) });
const userInput = (turnId, text, at, nonce) => product({
  id: `workbench-user:${turnId}`, role: "user", text, at: iso(at), provenance: { kind: "turn_input", turn_id: turnId },
  ...(nonce ? { client_nonce: nonce } : {})
});
const productReply = (id, turnId, text, at) => product({ id, role: "assistant", text, at: iso(at), provenance: { kind: "turn_output", turn_id: turnId } });
const occurrence = (id, text, at) => product({
  id, role: "event", text, at: iso(at), provenance: { kind: "trigger_occurrence", occurrence_id: id, process_ids: [] }
});

let cursor = 0;
const activity = (turnId, event, correlationId = "") => {
  cursor += 1;
  return {
    session_id: "laws", replay_incarnation_id: "laws", turn_id: turnId, revision: cursor,
    cursor: `c-${String(cursor).padStart(5, "0")}`, type: "turn_activity",
    activity: { sequence: cursor, id: `a-${cursor}`, correlation_id: correlationId, ...event }
  };
};
const committed = (turnId, rows) => {
  cursor += 1;
  return {
    session_id: "laws", replay_incarnation_id: "laws", turn_id: turnId, revision: cursor,
    cursor: `c-${String(cursor).padStart(5, "0")}`, type: "committed", rows
  };
};
const row = (rowId, kind, turnId, at, content = {}, provenance = {}) => ({
  row_id: rowId, kind, timestamp: iso(at), suppressed: null,
  provenance: { turn_id: turnId, input_id: null, plugin_id: null, is_turn_reply: kind === "assistant_reply", ...provenance },
  content: {
    text: "", reasoning: [], attachments: [], language: null, code: null, output: null, success: null,
    error: null, tools: [], tools_omitted: 0, ...content
  }
});

const rowKeys = list => [...list.children].map(node => node.dataset.key);

/* A turn as the streams deliver it: a send, its reasoning, a code block
   with one tool call, a reply in two deltas, its commit and its `done`. */
function streamTurn(view, time, turnId, { nonce = "nonce-1", text = "what is the weather?", beforeReply } = {}) {
  const { timeline } = view;
  timeline.beginSend({ nonce, text, attachments: [] });
  time.advance(40);
  timeline.applyProductEvent(userInput(turnId, text, time.now(), nonce));
  timeline.sendAccepted(nonce, { accepted: true, queued: false, turn_id: turnId });
  time.advance(200);
  timeline.applyObservation(activity(turnId, { type: "turn_started" }));
  timeline.applyObservation(activity(turnId, { type: "reasoning_delta", text: "Checking the forecast." }, "r1"));
  time.advance(300);
  timeline.applyObservation(activity(turnId, { type: "code_block_started", language: "typescript", code: "await weather.forecast()" }));
  timeline.applyObservation(activity(turnId, {
    type: "tool_call_started", call_id: `tc-${turnId}`, name: "mcp__parallel__web_search_x", args: { q: "weather" }
  }));
  time.advance(100);
  timeline.applyObservation(activity(turnId, {
    type: "tool_call_completed", call_id: `tc-${turnId}`, name: "mcp__parallel__web_search_x", args: { q: "weather" },
    output: { outcome: { status: "success", payload: { results: [1] } } }, duration_ms: 12
  }));
  timeline.applyObservation(activity(turnId, {
    type: "code_block_completed", language: "typescript", output: "sunny", duration_ms: 30, tool_call_ids: [`tc-${turnId}`]
  }));
  time.advance(500);
  beforeReply?.();
  time.advance(500);
  timeline.applyObservation(activity(turnId, { type: "assistant_prose_delta", text: "It will be " }, "p1"));
  time.advance(100);
  timeline.applyObservation(activity(turnId, { type: "assistant_prose_delta", text: "sunny." }, "p1"));
  time.advance(100);
  return {
    rows: [
      row(`n-${turnId}-user`, "user", turnId, T0, { text }),
      row(`n-${turnId}-reason`, "reasoning", turnId, T0 + 300, { reasoning: ["Checking the forecast."] }),
      row(`n-${turnId}-code`, "code_block", turnId, T0 + 900, {
        language: "typescript", code: "await weather.forecast()", output: "sunny", success: true,
        tools: [{ operation: "web.search", status: "success" }]
      }),
      row(`n-${turnId}-reply`, "assistant_reply", turnId, time.now(), { text: "It will be sunny." })
    ]
  };
}

function settle(view, time, turnId, rows) {
  view.timeline.applyProductEvent(productReply(`reply:${turnId}`, turnId, "It will be sunny.", time.now()));
  view.timeline.applyProductEvent(done(turnId));
  view.timeline.applyObservation(committed(turnId, rows));
}

/* A structural picture of the rendered rows, for comparing two renders. */
function serialize(node) {
  if (node.nodeType === 3) return node.nodeValue;
  return [
    node.tagName, node.className, node.hidden ? "hidden" : "", JSON.stringify({ ...node.dataset }),
    [...node.childNodes].map(serialize)
  ];
}

export const laws = [
  {
    name: "a turn's thinking, code and tools precede its reply without moving on settlement",
    async run(env) {
      const time = clock();
      const view = env.timeline(time);
      const turn = "turn-code-before-reply";
      view.timeline.applyProductEvent(userInput(turn, "say hello in five words", time.now()));
      time.advance(100);
      view.timeline.applyObservation(activity(turn, { type: "reasoning_delta", text: "Preparing a greeting." }, "r1"));
      // Model prose can arrive before the cell it describes executes. Its
      // reply row still belongs after that turn's code and tool rows.
      time.advance(100);
      view.timeline.applyObservation(activity(turn, { type: "assistant_prose_delta", text: "Hello there, wonderful curious friend." }, "p1"));
      for (let index = 0; index < 2; index++) {
        time.advance(100);
        view.timeline.applyObservation(activity(turn, { type: "code_block_started", language: "typescript", code: `await greeting(${index})` }));
        view.timeline.applyObservation(activity(turn, { type: "tool_call_started", call_id: `tc-${index}`, name: "greeting", args: { index } }));
        view.timeline.applyObservation(activity(turn, { type: "tool_call_completed", call_id: `tc-${index}`, name: "greeting", output: {}, duration_ms: 1 }));
        view.timeline.applyObservation(activity(turn, { type: "code_block_completed", language: "typescript", output: "hello", tool_call_ids: [`tc-${index}`] }));
      }
      const expected = [`input:${turn}`, `thinking:${turn}:0`, `code:${turn}:0`, `code:${turn}:1`, `reply:${turn}`];
      env.assert.deepEqual(rowKeys(view.list), expected, "live execution renders below the reply");
      const nodes = [...view.list.children];
      const tools = nodes.flatMap(node => [...node.querySelectorAll(".tool")]);
      env.assert.equal(tools.length, 2);
      const removed = env.removals(view.list);
      // The reply's node was recorded before execution; neither its timestamp
      // nor commit position is the presentation lane of the final answer.
      const rows = [
        row("n-input", "user", turn, T0, { text: "say hello in five words" }),
        row("n-reply", "assistant_reply", turn, T0 + 200, {
          reasoning: ["Preparing a greeting."], text: "Hello there, wonderful curious friend."
        }),
        ...[0, 1].map(index => row(`n-code-${index}`, "code_block", turn, T0 + 300 + index * 100, {
          language: "typescript", code: `await greeting(${index})`, output: "hello", success: true,
          tools: [{ operation: "greeting", status: "success" }]
        }))
      ];
      view.timeline.applyProductEvent(productReply(`reply:${turn}`, turn, "Hello there, wonderful curious friend.", time.now()));
      view.timeline.applyProductEvent(done(turn));
      view.timeline.applyObservation(committed(turn, rows));
      env.assert.deepEqual(rowKeys(view.list), expected, "settlement changed the lane order");
      env.assert.deepEqual([...view.list.children], nodes, "settlement replaced a row");
      env.assert.deepEqual(nodes.flatMap(node => [...node.querySelectorAll(".tool")]), tools, "settlement replaced a tool");
      env.assert.deepEqual(removed(), [], "settlement moved a row");
      // A viewer loading the settled turn gets the same causal lane order.
      const loaded = env.timeline(time);
      loaded.timeline.applySnapshot({ transcript: rows, active_turns: [] });
      env.assert.deepEqual(rowKeys(loaded.list), expected, "the settled snapshot uses recording order");
    }
  },
  {
    name: "a press made before a later reply renders above it, and stays there after settlement",
    async run(env) {
      const time = clock();
      const view = env.timeline(time);
      const turn = "turn-press";
      const { rows } = streamTurn(view, time, turn, {
        // The press happens between the code block and the reply; the
        // workflow that records it publishes it only after the reply began.
        beforeReply: () => { view.pressedAt = time.now(); }
      });
      view.timeline.applyProductEvent(occurrence("trigger:press", "red pressed", view.pressedAt));
      const live = rowKeys(view.list);
      env.assert.ok(live.indexOf(`code:${turn}:0`) < live.indexOf("occ:trigger:press"), `live: ${live}`);
      env.assert.ok(live.indexOf("occ:trigger:press") < live.indexOf(`reply:${turn}`), `live: ${live}`);
      settle(view, time, turn, rows);
      env.assert.deepEqual(rowKeys(view.list), live, "settlement moved a row");
    }
  },
  {
    name: "the user row, reply and tool rows are the same nodes before and after commit, and none is removed",
    async run(env) {
      const time = clock();
      const view = env.timeline(time);
      const removed = env.removals(view.list);
      const turn = "turn-identity";
      const { rows } = streamTurn(view, time, turn);
      const before = {
        user: view.timeline.nodeOf(`input:${turn}`),
        reasoning: view.timeline.nodeOf(`thinking:${turn}:0`),
        code: view.timeline.nodeOf(`code:${turn}:0`),
        reply: view.timeline.nodeOf(`reply:${turn}`)
      };
      const tool = before.code.querySelector(".tool");
      env.assert.ok(before.user && before.reasoning && before.code && before.reply && tool, "every live row rendered");
      settle(view, time, turn, rows);
      for (const [name, node] of Object.entries(before)) {
        const key = node.dataset.key;
        env.assert.equal(view.timeline.nodeOf(key), node, `${name} was replaced`);
        env.assert.ok(node.dataset.transcriptRowId, `${name} carries its committed row`);
      }
      env.assert.equal(before.code.querySelector(".tool"), tool, "the tool row was replaced");
      env.assert.deepEqual(removed(), [], "a row was removed during the turn");
      env.assert.equal(view.list.querySelectorAll(".message.user").length, 1);
    }
  },
  {
    name: "a reply is one row after commit, also when the turn is settled again after a host restart",
    async run(env) {
      const time = clock();
      const view = env.timeline(time);
      const turn = "turn-resettle";
      const { rows } = streamTurn(view, time, turn);
      settle(view, time, turn, rows);
      const reply = view.timeline.nodeOf(`reply:${turn}`);
      // A restart re-follows the turn: the commit is observed again under a
      // new cursor and the reply is published again, here under a fresh id.
      view.timeline.applyObservation(committed(turn, rows));
      view.timeline.applyProductEvent(productReply("restarted-reply-id", turn, "It will be sunny.", time.now()));
      view.timeline.applyProductEvent(done(turn));
      env.assert.equal(view.list.querySelectorAll(".message.assistant").length, 1);
      env.assert.equal(view.timeline.nodeOf(`reply:${turn}`), reply);
    }
  },
  {
    name: "redelivery of the same observation is idempotent, and a replay gap rebuilds to the same DOM",
    async run(env) {
      const time = clock();
      const view = env.timeline(time);
      const removed = env.removals(view.list);
      const turn = "turn-redeliver";
      view.timeline.applyProductEvent(userInput(turn, "hello", time.now()));
      const delta = activity(turn, { type: "assistant_prose_delta", text: "Hi " }, "p1");
      view.timeline.applyObservation(delta);
      view.timeline.applyObservation(delta);
      env.assert.equal(view.timeline.nodeOf(`reply:${turn}`).querySelector(".msg-text").textContent.trim(), "Hi");
      const commit = committed(turn, [
        row("n-user", "user", turn, T0, { text: "hello" }),
        row("n-reply", "assistant_reply", turn, T0 + 10, { text: "Hi there" })
      ]);
      view.timeline.applyProductEvent(done(turn));
      view.timeline.applyObservation(commit);
      const occurrenceEvent = occurrence("trigger:blue", "blue pressed", T0 + 5);
      view.timeline.applyProductEvent(occurrenceEvent);
      const settled = serialize(view.list);
      view.timeline.applyObservation(commit);
      view.timeline.applyProductEvent(occurrenceEvent);
      env.assert.deepEqual(serialize(view.list), settled, "redelivery changed the rows");
      const nodes = [...view.list.children];
      // The authoritative read a replay gap takes: the same committed rows and
      // product lane the streams already delivered.
      view.timeline.applySnapshot({
        transcript: commit.rows,
        product_events: { cursor: sequence, events: [userInput(turn, "hello", T0), occurrenceEvent, done(turn)] },
        active_turns: [], pending_turn_inputs: [], turn_input_applications: [], unknown_turn_terminals: []
      }, view.timeline.epoch());
      env.assert.deepEqual(serialize(view.list), settled, "the replay-gap rebuild changed the rows");
      env.assert.deepEqual([...view.list.children], nodes, "the replay-gap rebuild replaced nodes");
      env.assert.deepEqual(removed(), [], "a row was removed");
    }
  },
  {
    name: "a queued input's row lands after the previous turn's reply",
    async run(env) {
      const time = clock();
      const view = env.timeline(time);
      const first = "turn-first";
      const { rows } = streamTurn(view, time, first, {
        beforeReply: () => view.timeline.showReceipt({
          accepted: true, input_id: "input-next", ingress: { scope: "next_turn" }, state: "deferred_next_turn", text: "and tomorrow?"
        })
      });
      env.assert.equal(view.footer.children.length, 1, "the queued input waits in the footer");
      settle(view, time, first, rows);
      time.advance(1000);
      const second = "turn-second";
      view.timeline.applyObservation(activity(second, {
        type: "turn_input_applied", applications: [{ input_id: "input-next", turn_id: second, committed_message_id: "m-next" }]
      }));
      const keys = rowKeys(view.list);
      env.assert.ok(keys.indexOf(`reply:${first}`) < keys.indexOf(`input:${second}`), `rows: ${keys}`);
      env.assert.equal(view.footer.children.length, 0, "the receipt left the footer");
      const input = view.timeline.nodeOf(`input:${second}`);
      view.timeline.applyObservation(committed(second, [
        row("n-next", "user", second, time.now(), { text: "and tomorrow?" }, { input_id: "input-next" })
      ]));
      env.assert.equal(view.timeline.nodeOf(`input:${second}`), input, "the committed input replaced the row");
    }
  },
  {
    name: "an approval card anchors to its tool call even when the call's display name differs",
    async run(env) {
      const time = clock();
      const view = env.timeline(time);
      const turn = "turn-approval";
      streamTurn(view, time, turn);
      view.timeline.applyObservation(activity(turn, { type: "code_block_started", language: "typescript", code: "await ops.apply_change()" }));
      view.timeline.applyObservation(activity(turn, {
        type: "tool_call_started", call_id: "tc-approval", name: "mcp__parallel__web_fetch_y", args: { url: "x" }
      }));
      time.advance(200);
      view.timeline.applyObservation(activity(turn, { type: "reasoning_delta", text: "Waiting on approval." }, "r2"));
      view.timeline.setApprovals([{
        key: "approval-1", tool: "workbench_ops_apply_change", call_id: "tc-approval", arguments: { target: "x" },
        requesting_session: "laws", requested_at_ms: T0, age_ms: 10
      }], "laws");
      const keys = rowKeys(view.list);
      env.assert.equal(keys[keys.indexOf(`code:${turn}:1`) + 1], "approval:approval-1", `rows: ${keys}`);
    }
  }
];
