# E2E Scenario: Workbench `continue_as` Frame Boundary

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface, polling,
> named-checkpoint screenshot, **three-layer cross-check**, real-token, Abort/RCA, and
> teardown rules. This runbook adds only the `continue_as` scenario.


> **Workbench process replacement (FIG-1164, FIG-3035).** The non-destructive
> same-configuration restart is `just agent-workbench-restart <port>`, which keeps the Restate
> journals and the application data. It is verified: the phases below execute it, and no step
> of this row is blocked any more. See the
> [central lifecycle constraint](../RULES.md#agent-workbench-lifecycle-constraint-fig-1164);
> never substitute the destructive reset.

**Automated check.** `just workbench-continue-as-budget-gate` runs a scripted RLM session
without a browser or provider network call. It proves the typed context-budget warning, the
warning in the next model request, and the `continue_as` frame boundary. Run it under
`kiln gate lash <fork-name> -- just workbench-continue-as-budget-gate`.

**Purpose.** Referee an RLM agent-initiated `control.continue_as({ task, seed })` tail-call
through the workbench browser surface. The scenario proves that one logical composer turn
can open a fresh `AgentFrame`, carry only its explicit seed into that frame, conclude coherently,
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
token context window, a 21,000-token RLM warning threshold, short assistant answers, and no
more than six synthetic pressure turns. Gate the warning and switch on provider-request,
typed runtime-event, durable-graph, and trace evidence, never on prose.
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
  `outcome.status == "agent_frame_switch"` and
  `outcome.frame_switch.frame_key == <new frame key>`; a distinct follow-frame physical turn
  completes the one composer send. Do not equate physical `turn_completed` count with
  rendered assistant-row count across a frame switch.

Any observed mismatch with this answer key is a **finding and FAIL**. Do not weaken the gate
or reinterpret persistence of old nodes as permission to render old assistant rows.

## Scenario-specific golden rules

1. **The switch is proven structurally.** Require a new `frame_open` graph node whose
   `reason` is exactly `continue_as`; its `frame_key` must equal the trace's
   `outcome.frame_switch.frame_key`. Prose claiming a fresh start proves nothing.
2. **The seed is explicit and inspectable.** Use two distinctive seed values: a baton marker
   needed by the seeded competence probe and a compact supporting fact. Require one RLM seed
   protocol event on the new frame path carrying exactly those two **values**. The key
   spelling is the agent's to choose whenever the organic lever fires, so gate on the values,
   not on the key names; only the explicit prompt in Phase 2 pins `seed_baton`. The
   deliberately non-seeded marker must not occur anywhere in that seed event.
3. **Resolve both frames independently.** Record the old and new frame node ids. No workbench
   endpoint serves frame records, and the stored `frame_open` node carries only `frame_key`,
   `reason`, `assignment`, and `protocol_turn_options` beside its `parent_node_id`, so
   reconstruct the link the way core derives `AgentFrameRecord.previous_frame_node_id`: walk
   the active path and take each `frame_open` node's nearest preceding `frame_open` ancestor
   as its previous frame. The new frame's reconstructed previous frame must be exactly the
   recorded old frame node id. Materialize/read each frame separately: the old view retains
   its pre-switch rows and the new view does not.
4. **Declare the answer before looking.** Save the answer key above with the pre-switch
   transcript snapshot before submitting the switching turn. Compare it to the observed
   post-switch DOM, API/current read model, product log, raw graph, and trace without editing
   the expected file afterwards.
5. **Pressure is bounded by the RLM warning.** Submit two to six distinctive marker turns,
   each with enough inert filler to approach the 21,000-token warning threshold. Stop adding
   filler when the first `rlm_context_budget_warning` status appears. Never fill the
   41,000-token window until provider rejection.
6. **Prove the warning reached the model.** Record the first session-scoped
   `/api/observations` `plugin_runtime` event whose `event.kind` is `status` and whose
   `event.key` is `rlm_context_budget_warning`. Its detail must say
   `warn at 21000` and report at least 21,000 tokens used. Preserve the completed
   prompt usage that crossed the threshold and the following `llm_call_started.request`.
   That request must
   contain the RLM prompt suffix `Past the frame switch threshold` and
   `control.continue_as`; a status badge or assistant prose alone does not pass. If the
   sixth bounded prompt first crosses the threshold, submit one short marker-only probe
   without filler to expose the next model-facing request. Missing warning or request
   evidence is a FAIL.
7. **Exercise a real tool before switching.** At least one pressure turn must produce paired
   successful `tool_call_started` / `tool_call_completed` records for the same call id. Pick a
   read-only tool from the session's advertised catalog: prefer the Parallel web-search MCP
   tool when it is present, otherwise use any other advertised read-only tool. A
   `<typescript>` cell without a tool call does not satisfy this gate.
8. **Try the organic lever once, then guide explicitly.** After the RLM warning appears,
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
  `AGENT_WORKBENCH_CONTINUE_AS_WARN_TOKENS=21000`,
  `AGENT_WORKBENCH_DATA_DIR=/workspace/tmp/fig992a-run/data`, a fresh
  `AGENT_WORKBENCH_RUN_DIR`, `AGENT_WORKBENCH_OPEN=0`, and `RESTATE_AUTHORITY_ID=<stable-id>`.
  `RESTATE_AUTHORITY_ID` is required and must stay stable for one Restate state; without it
  the workbench refuses to start with
  `Error: RESTATE_AUTHORITY_ID is required and must remain stable for one Restate state`.
  Never touch ports 3056, 3057, or
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
- **Layer 3 — trace and runtime stream:** filter `trace.jsonl` and
  `GET /api/observations?session_id=<S>` by session `<S>`. Preserve the
  `rlm_context_budget_warning` `plugin_runtime` event,
  the threshold-crossing prompt's `llm_call_completed` usage, the following
  `llm_call_started` request, tool calls,
  and `turn_completed`, including graph and parent ids. Open the observation stream before
  the first pressure send and save it as it arrives; opening it afterward starts at the
  current cursor. The browser's work rail calls
  `/api/work` during hydration and on its polling
  interval; those reads legitimately emit session-scoped `agent_workbench.api.work.response`
  custom records even before the first turn. Preserve them, but do not count them as runtime
  conversation activity or require a literally empty session-scoped trace at baseline.
- The restart phase runs `bash scripts/agent-workbench-dev.sh restart --port 3200` (equivalently
  `just agent-workbench-restart 3200`), the verified non-destructive same-configuration
  replacement named in this runbook's FIG-1164/FIG-3035 header and in
  [RULES.md](../RULES.md#agent-workbench-lifecycle-constraint-fig-1164). The helper replaces the
  process and reports that the Restate deployment, its journals and the application data were
  retained; that line is the readiness evidence for the phase. Never substitute the destructive
  reset. Teardown remains
  `bash scripts/agent-workbench-dev.sh down --port 3200`, followed by removal of
  `/workspace/tmp/fig992a-run/data` only after all evidence has been copied out.

## Phase 0 — Boot, scope, and baseline

Start from a nonexistent data directory. Boot with the exact environment above, gate
`/healthz`, and open the scoped URL. Require the composer, empty transcript, rendered session
id `<S>`, `/api/state.settings.session_id == <S>`, idle, and no active turns. Require zero
session-scoped graph rows and zero session-scoped turn, RLM warning, or tool-call records.
Passive `agent_workbench.api.work.response` records with an empty result are expected
from the rendered browser surface and must be recorded separately from that activity gate.
Record the workbench PID, Restate container id and `StartedAt`, model, and exact
`AGENT_WORKBENCH_CONTEXT_WINDOW_TOKENS` and
`AGENT_WORKBENCH_CONTINUE_AS_WARN_TOKENS` launch values. The Phase 1 status and
model-facing request prove the RLM session used the warning threshold. Screenshot
`00-scoped-empty.png`; save
`00-identities.json`, `00-state.json`, and `00-trace.json`.

## Phase 1 — Build distinctive context and reach the RLM warning

Submit several bounded turns. Each establishes one literal marker fact and asks for a short
acknowledgement; deterministic inert filler may make each prompt roughly 4,000–6,000 tokens.
One turn must ask the agent to call a small read-only workbench tool before acknowledging its
marker. Choose it from the session's advertised catalog: prefer the Parallel web-search MCP
tool (`mcp__parallel__web_search_*`, call path `parallel.web_search_<digest>`) when it is
advertised, otherwise pick any other advertised read-only tool. Keep `FIG992A-SEED-<run-id>` as the future baton and
explicitly label
`FIG992A-NONSEED-<run-id>` as `unseeded_secret` in old-frame context.

After every send, gate the relevant `turn_completed`, idle, empty active turns, and stable
row/message counts, then run the three-layer cross-check. Poll the trace after each turn and
stop filler immediately when `rlm_context_budget_warning` appears; FAIL if it has
not appeared after the sixth pressure turn plus the permitted short threshold probe from
golden rule 6. Gate the status and model request in golden rule 6, and the successful
paired tool records in golden rule 7. Screenshot
`01-pressure-ready.png`; save `01-pressure-{dom,state,store,trace}.json`,
`01-rlm-budget-warning.json`, `01-warned-model-request.json`, and `01-tool-call.json`.

## Phase 2 — Drive and prove `continue_as`

First submit one organic switch opportunity after pressure without naming `continue_as`:
ask the agent to preserve only the future baton and supporting fact needed to continue the
marker task in a clean context. Gate the completed physical turn(s) and inspect typed trace
evidence. If no switch occurred, record that outcome, then submit exactly one explicit prompt
ending with: `use control.continue_as to start fresh, seed what you need`. Tell it to seed
the baton marker under `seed_baton` plus the supporting fact, and not to seed
`unseeded_secret`.

The seed-key spelling is pinned only on this explicit path. When the organic lever fires
first — the outcome this runbook prefers — the agent chooses its own key, and a run has been
observed seeding `future_baton`. Gate on the seeded **values** (the baton marker and the
supporting fact are present in the seed, the non-seeded marker is not), not on the key
spelling, unless the explicit prompt above was the one that fired.

Before whichever send is expected to switch, save the unchanged boundary answer key and
the pre-switch transcript as `02-boundary-expected.json` and
`02-before-switch-{dom,state,store,trace}.json`. Gate, in this order:

1. a `turn_completed` with `outcome.status == "agent_frame_switch"` and a non-empty
   `outcome.frame_switch.frame_key`;
2. one new raw `frame_open` node with `reason == "continue_as"` and matching `frame_key`;
3. a new-frame RLM seed event containing the baton marker (under `seed_baton` on the explicit
   path, or under whatever key the agent chose on the organic one) and the supporting fact,
   but not `unseeded_secret` or its marker;
4. the new frame's reconstructed previous frame, per golden rule 3, is exactly the recorded
   old frame node id;
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

**4a — seeded fact.** Submit a short prompt asking for the value of the seeded baton — by the
key actually recorded in Phase 2 — without
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
shapes. Any earlier frames must also remain durable.
Screenshot `06-after-restart-reload.png`; save `06-restart-identities.json`,
`06-reload-multiset.json`, `06-frame-graph.json`, and `06-crosscheck.json`.

## Phase 6 — Teardown and score

Run the prescribed `down` command. Confirm the workbench PID is gone, port 3200 refuses
connections, and no `lash-agent-workbench-dev-restate-3200` container remains. Copy all final
extracts first, then remove `/workspace/tmp/fig992a-run/data` and confirm it is absent.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Boot/scope | `/healthz` 200; exact 41,000-token launch; rendered/API session `<S>`; DOM/API/graph and runtime-activity trace empty (passive empty work-poll records allowed and retained) | | `00-scoped-empty.png`, `00-identities.json`, `00-state.json`, `00-trace.json` |
| RLM pressure | 2–6 marker turns; one `rlm_context_budget_warning` status reports at least 21,000 tokens used and `warn at 21000` | | `01-pressure-ready.png`, `01-rlm-budget-warning.json` |
| Model warning | The next model request contains `Past the frame switch threshold` and `control.continue_as` | | `01-warned-model-request.json` |
| Real tool turn | paired successful tool start/completion with one call id | | `01-tool-call.json` |
| Switch lever | organic pressure tried once; actual lever recorded honestly | | `02-lever.json` |
| Frame switch | matching trace switch + `frame_open{reason:"continue_as"}`; follow frame completed coherently | | `02-frame-graph.json`, `02-switch-trace.json` |
| Seed materialized | exact seeded keys/values in new frame; non-seeded marker absent | | `02-seed.json` |
| Previous frame retained | previous frame reconstructed from the active path is the recorded old frame node, and its read model retains pre-switch rows | | `02-frame-graph.json` |
| Boundary rendering | persistent user rows + collapsed old assistant rows match the predeclared answer key across DOM/API/graph/trace | | `02-boundary-expected.json`, `03-boundary-*.json`, `03-crosscheck.json` |
| Seeded competence | reply contains the seeded baton without restating it in the prompt | | `04-seeded-fact.png`, `04-seeded-judge.json` |
| Clean window | reply does not contain the deliberately non-seeded marker | | `05-nonseeded-fact.png`, `05-nonseeded-judge.json` |
| Restart/reload identity | identical row multiset and identical durable pre-switch/new-frame pair plus seed | | `06-after-restart-reload.png`, `06-*.json` |
| Teardown | process/container gone; port closed; state directory removed | | command log |

**Aggregate:** did a real workbench RLM agent, after a model-visible RLM budget warning, tail-call
through `control.continue_as` into a structurally proven clean frame, carry exactly its seed,
render the frame boundary according to the predeclared asymmetric answer key, demonstrate
seeded competence without leaking a deliberately omitted fact, and preserve that truth across
a web-process restart and browser reload?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
