# E2E Scenario: Workbench `continue_as` Frame Boundary

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface, polling,
> named-checkpoint screenshot, **three-layer cross-check**, real-token, Abort/RCA, and
> teardown rules. This runbook adds only the `continue_as` scenario.

**Purpose.** Referee an RLM agent-initiated `control.continue_as({ task, seed })` tail-call
through the workbench browser surface. The scenario proves that one logical composer turn
can open a fresh `AgentFrame`, carry only its explicit seed into that frame, finish coherently,
and remain truthful across the deliberately asymmetric transcript projection and a process
restart.

**Why this scenario exists.** `continue_as` is not another chat reply. It is a terminal
control action: the physical turn that calls it commits a `frame_open` node with reason
`continue_as`, then the same logical composer turn continues in a fresh frame. Nothing from
the old frame is inherited implicitly. The new frame receives only the tool's `task` and
`seed`; the old frame and all of its nodes remain durable history. A UI that renders the raw
graph, a runtime that forgets the seed, or a follow turn that quietly sees old-frame context
would each tell a different and incorrect story.

**Real tokens.** This scenario uses OpenRouter. Model prose and whether pressure alone makes
the agent choose `continue_as` are nondeterministic. Keep the pressure bounded: use a 41,000
token context window, short assistant answers, and no more than six synthetic pressure
turns. Gate the switch and compaction on typed durable and trace evidence, never on prose.
Only the two post-switch competence probes are judged.

## Boundary-rendering answer key — state this before observing

After `continue_as`, the workbench's frame-scoped committed read model contains the new
frame's rows and excludes pre-switch assistant rows. The UI-owned user rows in the product
event log are session-scoped, so all submitted `you` rows — including pre-switch rows and the
switch request — remain rendered. The raw session graph retains both frames and every old
node. The seed is a protocol event in the new frame, not a chat row.

Therefore the expected post-switch shape is:

- **DOM and `/api/state.messages`:** all product-event-backed user rows persist; old-frame
  assistant rows disappear; the coherent assistant reply produced by the follow frame is
  present exactly once.
- **Current-frame read model:** no pre-switch conversation rows; it contains the new-frame
  conversation projection only. The seed protocol event is reachable on the new frame's
  graph path even though it is not a transcript message.
- **Raw durable graph:** the new `frame_open` points back through ancestry to the old frame;
  resolving that old frame still yields the pre-switch rows.
- **Trace:** the switch physical turn is `turn_completed` with
  `done_reason == "agent_frame_switch"` and
  `agent_frame_switch.frame_id == <new frame key>`; a distinct follow-frame physical turn
  completes the one composer send. Do not equate physical `turn_completed` count with
  rendered assistant-row count across a frame switch.

Any observed mismatch with this answer key is a **finding and FAIL**. Do not weaken the gate
or reinterpret persistence of old nodes as permission to render old assistant rows.

## Scenario-specific golden rules

1. **The switch is proven structurally.** Require a new `frame_open` graph node whose
   `reason` is exactly `continue_as`; its `frame_key` must equal the trace's
   `agent_frame_switch.frame_id`. Prose claiming a fresh start proves nothing.
2. **The seed is explicit and inspectable.** Use two distinctive seed values: a baton marker
   needed by the seeded competence probe and a compact supporting fact. Require one RLM seed
   protocol event on the new frame path with exactly those keys and values. The deliberately
   non-seeded marker must not occur anywhere in that seed event.
3. **Resolve both frames independently.** Record the old and new frame node ids. The new
   frame's `previous_frame_node_id` must resolve to the old frame. Materialize/read each
   frame separately: the old view retains its pre-switch rows and the new view does not.
4. **Declare the answer before looking.** Save the answer key above with the pre-switch
   transcript snapshot before submitting the switching turn. Compare it to the observed
   post-switch DOM, API/current read model, product log, raw graph, and trace without editing
   the expected file afterwards.
5. **Pressure is bounded and trace-gated.** Submit several distinctive marker turns (at
   least two, no more than six),
   each with enough deterministic filler to approach the 21,000-token compaction threshold.
   Stop adding pressure as soon as `rolling_history_compaction_needed` appears. Never exceed
   six pressure turns and never fill the window until provider rejection.
6. **Record both rolling-history decision events by scope.** The first
   `rolling_history_compaction_needed` must report `max_context_tokens == 41000`,
   `threshold_tokens == 21000`, and `context_budget_tokens >= threshold_tokens`.
   `rolling_history_prompt_pruned` must carry non-negative dropped/retained counts and be
   turn-scoped. Missing or incorrectly parented decision evidence is a FAIL.
   The decision consumes the prior completed prompt's usage: if the sixth bounded prompt is
   the first to cross 21,000 tokens, submit one short marker-only probe (no more filler) and
   require the events on that probe.

   `rolling_history_compaction_started` and `rolling_history_compaction_completed` are
   intentionally out of scope here. They describe host-invoked `compact_context` lifecycle
   work in standard mode, covered by the slack-clone variant-B compaction runbook and core
   rolling-history regression tests. The workbench is the RLM-only reference host: its
   durable context transition is the agent-driven `control.continue_as` frame switch that
   this runbook exercises, not a host-invoked standard-mode compaction.
7. **Exercise a real tool before switching.** At least one pressure turn must produce paired
   successful `tool_call_started` / `tool_call_completed` records for the same call id (for
   example `web.search` when `/api/state.settings.web_configured` is true). A Lashlang block
   without a tool call does not satisfy this
   gate.
8. **Try the organic lever once, then guide explicitly.** After compaction pressure exists,
   first ask the agent to continue the marker-retention task without naming the tool and
   inspect the trace. If it switches, record `pressure/organic`. If it does not, submit one
   explicit instruction: `use control.continue_as to start fresh, seed what you need`, and
   record `explicit guidance`. Do not keep re-prompting until a desired outcome appears.
9. **Judge only the two competence probes.** The seeded probe must answer with the baton
   marker without the prompt restating it. The non-seeded probe names the fact by alias but
   never includes its marker; its reply must not contain the non-seeded marker. Structural
   gates still precede both judgements.
10. **Scope everything to one session.** Drive `/?session_id=<S>` and filter the state API,
    product-event log, graph database, and trace by `<S>`. Record message ids and physical
    turn ids; never normalize away a cross-layer mismatch.

## Working material

- Require `OPENROUTER_API_KEY` from the checkout's gitignored `.env`. Boot only on port
  `3200` with:
  `AGENT_WORKBENCH_CONTEXT_WINDOW_TOKENS=41000`,
  `AGENT_WORKBENCH_DATA_DIR=/workspace/tmp/fig992a-run/data`, a fresh
  `AGENT_WORKBENCH_RUN_DIR`, and `AGENT_WORKBENCH_OPEN=0`. Never touch ports 3056, 3057, or
  3180. Gate `GET /healthz` to 200 and record the configured model from `/api/state`.
- Use one fresh session id `<S> = runbook-continue-as-<run-id>` and artifact directory
  `<artifacts>`. Save every named screenshot and JSON/text extract below under `<artifacts>`.
- Choose markers before boot:
  `FIG992A-FACT-1-<run-id>`, `FIG992A-FACT-2-<run-id>`,
  `FIG992A-SEED-<run-id>`, and `FIG992A-NONSEED-<run-id>`. The last marker is the value of
  the alias `unseeded_secret`; it must never be copied into `seed`.
- Stable browser affordances are the composer, send control, `#timeline .message.user`,
  `#timeline .message.assistant`, and the idle/running pill. Discover exact compose selectors
  from the served page. Use `wait_until="domcontentloaded"`, explicit waiting assertions,
  count stability, and named-checkpoint screenshots.
- **Layer 1 — DOM:** record each rendered row as role class + body text + any exposed id.
  When cross-checking assistant text against `/api/state`, compare the DOM to the app's
  rendered Markdown projection: pass the API Markdown through the exact
  `renderMarkdownBlocks` function served by the app, then read its user-visible text from an
  off-screen element that still participates in layout. Do not compare raw Markdown bytes
  to DOM text, and do not use `visibility:hidden` (or another non-visible probe whose
  `innerText` is empty) for this assertion.
- **Layer 2 — API and durable graph:** save `/api/state?session_id=<S>`, the `<S>` entry in
  `product-events.json`, and all non-tombstoned `<S>` rows from
  `lash-sessions/durable-core.db.graph_nodes`. Decode `node_json`; reconstruct the active
  ancestry and both frame-scoped read models rather than treating all raw nodes as visible.
- **Layer 3 — trace:** filter `trace.jsonl` by `context.session_id == <S>`. Preserve full
  records for rolling-history events, tool calls, and `turn_completed`, including graph and
  parent ids. The browser's work rail calls `/api/work` during hydration and on its polling
  interval; those reads legitimately emit session-scoped `agent_workbench.api.work.response`
  custom records even before the first turn. Preserve them, but do not count them as runtime
  conversation activity or require a literally empty session-scoped trace at baseline.
- Restart with the same exported data/run directories via
  `bash scripts/agent-workbench-dev.sh restart --port 3200`. Teardown is
  `bash scripts/agent-workbench-dev.sh down --port 3200`, followed by removal of
  `/workspace/tmp/fig992a-run/data` only after all evidence has been copied out.

## Phase 0 — Boot, scope, and baseline

Start from a nonexistent data directory. Boot with the exact environment above, gate
`/healthz`, and open the scoped URL. Require the composer, empty transcript, rendered session
id `<S>`, `/api/state.settings.session_id == <S>`, idle, and no active turns. Require zero
session-scoped graph rows and zero session-scoped turn, rolling-history, or tool-call trace
records. Passive `agent_workbench.api.work.response` records with an empty result are expected
from the rendered browser surface and must be recorded separately from that activity gate.
Record the workbench PID, Restate container id and `StartedAt`, model, and exact
`AGENT_WORKBENCH_CONTEXT_WINDOW_TOKENS` launch value; the first Phase 1
`rolling_history_compaction_needed.max_context_tokens` is the runtime proof that the session
policy delivered that value to the hook. Screenshot `00-scoped-empty.png`; save
`00-identities.json`, `00-state.json`, and `00-trace.json`.

## Phase 1 — Build distinctive context and reach compaction pressure

Submit several bounded turns. Each establishes one literal marker fact and asks for a short
acknowledgement; deterministic inert filler may make each prompt roughly 4,000–6,000 tokens.
One turn must ask the agent to call a small read-only workbench tool such as
`web.search({ query: "Lash runtime GitHub", limit: 1 })` before acknowledging its marker.
Require `/api/state.settings.web_configured == true` before choosing this example. Keep
`FIG992A-SEED-<run-id>` as the future baton and explicitly label
`FIG992A-NONSEED-<run-id>` as `unseeded_secret` in old-frame context.

After every send, gate the relevant `turn_completed`, idle, empty active turns, and stable
row/message counts, then run the three-layer cross-check. Poll the trace after each turn and
stop pressure immediately when `rolling_history_compaction_needed` appears; FAIL if it has
not appeared after the sixth pressure turn plus the permitted short threshold probe from
golden rule 6. Gate the payloads and scope/parentage in golden rule 6, and the successful
paired tool records in golden rule 7. Screenshot
`01-pressure-ready.png`; save `01-pressure-{dom,state,store,trace}.json`,
`01-rolling-history.json`, and `01-tool-call.json`.

## Phase 2 — Drive and prove `continue_as`

First submit one organic switch opportunity after pressure without naming `continue_as`:
ask the agent to preserve only the future baton and supporting fact needed to continue the
marker task in a clean context. Gate the completed physical turn(s) and inspect typed trace
evidence. If no switch occurred, record that outcome, then submit exactly one explicit prompt
ending with: `use control.continue_as to start fresh, seed what you need`. Tell it to seed
the baton marker under `seed_baton` plus the supporting fact, and not to seed
`unseeded_secret`.

Before whichever send is expected to switch, save the unchanged boundary answer key and
the pre-switch transcript as `02-boundary-expected.json` and
`02-before-switch-{dom,state,store,trace}.json`. Gate, in this order:

1. a `turn_completed` with `done_reason == "agent_frame_switch"` and a non-empty
   `agent_frame_switch.frame_id`;
2. one new raw `frame_open` node with `reason == "continue_as"` and matching `frame_key`;
3. a new-frame RLM seed event containing `seed_baton` and the supporting fact, but not
   `unseeded_secret` or its marker;
4. the new frame's `previous_frame_node_id` resolves exactly to the recorded old frame;
5. the old-frame read model still contains the pre-switch marker rows, while the new-frame
   read model excludes them;
6. the distinct follow-frame physical turn completes and the logical turn's one rendered
   assistant reply is coherent with its `task` and explicit seed.

Record whether `pressure/organic` or `explicit guidance` fired the switch. Screenshot
`02-after-switch.png`; save `02-lever.json`, `02-frame-graph.json`, `02-seed.json`, and
`02-switch-trace.json`.

## Phase 3 — Referee boundary rendering across three layers

With the answer key already fixed, settle the live projection and capture the post-switch row
multiset. Require every pre-switch product-event-backed user row to remain in the DOM and
`/api/state.messages`, every pre-switch assistant row to be absent from both, and the new
frame's coherent assistant reply to appear once. Require the product-event log, current-frame
read model, raw graph, and trace to show their respective answer-key shapes exactly. Record
the pairwise comparison, including the intentional raw-history/current-projection asymmetry.

A pre-switch assistant row that remains rendered, a missing user row, a seed rendered as a
chat row, or disagreement between DOM and API is a product defect → capture RCA evidence and
Abort. Screenshot `03-boundary-rendering.png`; save
`03-boundary-{dom,state,product,store,trace}.json` and `03-crosscheck.json`.

## Phase 4 — Post-switch competence and clean-window proof

**4a — seeded fact.** Submit a short prompt asking for the value of `seed_baton` without
including its value. After structural settlement, judge the single reply: it must contain
`FIG992A-SEED-<run-id>`. Screenshot `04-seeded-fact.png`; save
`04-seeded-{dom,state,store,trace,judge}.json`.

**4b — deliberately non-seeded fact.** Submit a prompt asking whether it knows the exact
value formerly assigned to `unseeded_secret`; do not include that value or marker in the
prompt. After structural settlement, judge the single reply: it must **not** contain
`FIG992A-NONSEED-<run-id>`. A refusal, honest uncertainty, or `UNKNOWN` passes; recovering the
marker is a clean-window leak and FAIL. Screenshot `05-nonseeded-fact.png`; save
`05-nonseeded-{dom,state,store,trace,judge}.json`.

Run the three-layer cross-check after each probe. The old raw frame still retaining the
non-seeded marker is expected; only its appearance in the new-frame reply fails the judge.

## Phase 5 — Restart, reload, and durable identity

Record the complete post-switch DOM row multiset, current frame node id, both frame records,
and their relevant raw nodes. Restart only the workbench web process with the same data/run
directories. Require a changed workbench PID, unchanged Restate container id and `StartedAt`,
and `/healthz` recovery. Reload the scoped page, gate the rendered session id and idle phase,
then settle by stability.

Require the post-reload row multiset to equal the pre-restart multiset exactly. Require the
same current frame node id; the same `continue_as` reason and previous-frame link; the same
seed event; and the independently resolved pre-switch/new-frame pair to retain its Phase 3
shapes. Earlier compaction frames may also exist and must not be discarded.
Screenshot `06-after-restart-reload.png`; save `06-restart-identities.json`,
`06-reload-multiset.json`, `06-frame-graph.json`, and `06-crosscheck.json`.

## Phase 6 — Teardown and score

Run the prescribed `down` command. Confirm the workbench PID is gone, port 3200 refuses
connections, and no `lash-agent-workbench-dev-restate-3200` container remains. Copy all final
extracts first, then remove `/workspace/tmp/fig992a-run/data` and confirm it is absent.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Boot/scope | `/healthz` 200; exact 41,000-token launch; rendered/API session `<S>`; DOM/API/graph and runtime-activity trace empty (passive empty work-poll records allowed and retained) | | `00-scoped-empty.png`, `00-identities.json`, `00-state.json`, `00-trace.json` |
| Bounded pressure | 2–6 marker turns; `rolling_history_compaction_needed` has 41000/21000 budget fields | | `01-pressure-ready.png`, `01-rolling-history.json` |
| Rolling-history scope | needed/pruned are turn-scoped with typed payloads retained; host-invoked standard-mode started/completed lifecycle is out of scope | | `01-rolling-history.json` |
| Real tool turn | paired successful tool start/completion with one call id | | `01-tool-call.json` |
| Switch lever | organic pressure tried once; actual lever recorded honestly | | `02-lever.json` |
| Frame switch | matching trace switch + `frame_open{reason:"continue_as"}`; follow frame completed coherently | | `02-frame-graph.json`, `02-switch-trace.json` |
| Seed materialized | exact seeded keys/values in new frame; non-seeded marker absent | | `02-seed.json` |
| Previous frame retained | previous-frame id resolves and its read model retains pre-switch rows | | `02-frame-graph.json` |
| Boundary rendering | persistent user rows + collapsed old assistant rows match the predeclared answer key across DOM/API/graph/trace | | `02-boundary-expected.json`, `03-boundary-*.json`, `03-crosscheck.json` |
| Seeded competence | reply contains the seeded baton without restating it in the prompt | | `04-seeded-fact.png`, `04-seeded-judge.json` |
| Clean window | reply does not contain the deliberately non-seeded marker | | `05-nonseeded-fact.png`, `05-nonseeded-judge.json` |
| Restart/reload identity | identical row multiset and identical durable pre-switch/new-frame pair plus seed | | `06-after-restart-reload.png`, `06-*.json` |
| Teardown | process/container gone; port closed; state directory removed | | command log |

**Aggregate:** did a real workbench RLM agent, under bounded rolling-history pressure, tail-call
through `control.continue_as` into a structurally proven clean frame, carry exactly its seed,
render the frame boundary according to the predeclared asymmetric answer key, demonstrate
seeded competence without leaking a deliberately omitted fact, and preserve that truth across
a web-process restart and browser reload?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
