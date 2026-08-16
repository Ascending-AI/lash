# E2E Scenario: Workbench Chat Projection Integrity

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface, polling,
> named-checkpoint screenshot, **three-layer cross-check**, real-token, Abort/RCA, and
> teardown rules. This runbook adds only the chat-projection scenario.

**Purpose.** Referee the *rendered conversation* itself. Every other workbench runbook
gates on a durable or API outcome and treats the transcript as corroboration; this one makes
the transcript the subject. It proves that one unit of conversational work — one composer
send, one trigger wake — projects into **exactly one** rendered row per role, on the live
stream path and on the reload backfill path, and that the render agrees with what the store,
the state API, the product-event log, and the trace all say happened.

**Why this scenario exists.** Two shipped defects rendered a conversation the runtime never
had, and no gate caught either, because every layer was self-consistent and nothing
reconciled them:

- **FIG-972** — one composer send rendered **two** `you` rows. The workbench publishes an
  optimistic user row in its own id namespace (`workbench-user:<turn_id>`) and the runtime
  commits the same text under its own minted id (`m_turn_<turn_id>_input`). Correlating the
  two by id *shape* rather than by `MessageOrigin::TurnInput` left both rendered. See the
  contract note above `WORKBENCH_USER_MESSAGE_ID` in
  [`state.rs`](../../examples/agent-workbench/src/main_sections/state.rs).
- **FIG-984** (fixed, merged) — one trigger-button press rendered the agent reply **twice**.
  Two committed assistant copies of one turn's text reached the browser: the runtime's
  terminal commit `m_turn_<turn_id>_assistant`, materialized by `materialize_terminal_output`
  in [`materialize.rs`](../../crates/lash-core/src/runtime/turn_boundary/materialize.rs) for a
  `TurnFinish::AssistantMessage` outcome, and the workbench's own
  `commit_assistant_transcript` copy `workbench-assistant:<turn_id>`. That helper's
  idempotence guard only looked for **its own** id, so it never saw the runtime's copy. The
  browser deduplicates by message id (`renderedMessages` in
  [`index.html`](../../examples/agent-workbench/assets/index.html)), so two ids carrying one
  text rendered as two rows.

  The discriminator was the turn's **termination**, not the trigger path. `record_turn_output`
  now commits the reply only for the terminations the runtime leaves uncommitted, so exactly
  one committed copy survives either way — but **which id holds it depends on the
  termination**, and this runbook must never assume one:

  | trace `done_reason` | reached by | the single committed assistant id | part kinds |
  |---|---|---|---|
  | `assistant_message` | a queued/wake turn, which runs without `require_finish` and answers as bare prose | `m_turn_<turn_id>_assistant` (runtime) | `Prose` |
  | `assistant_message` with reasoning | the same turn when the model also returned reasoning | `m_rlm_<turn_id>_<iteration>_assistant_response` (RLM protocol) | `Reasoning` + `Prose` |
  | `final_value` | a composer send, where `require_finish` forces the answer through `finish` | `workbench-assistant:<turn_id>` (workbench) | `Text` |

- **FIG-1406** (fixed) — the third row above is where the reply went *missing*. The RLM
  protocol commits the model's prose itself whenever reasoning rides along, and
  `materialize_terminal_output` then mints no runtime copy: the answer is already the
  transcript's last message. The workbench read every plugin-origin RLM message as protocol
  internals and re-admitted only its reasoning, so a reasoned bare-prose turn rendered its
  thinking and lost its answer. `durable_rlm_reply_message_ids` in
  [`chat_projection.rs`](../../examples/agent-workbench/src/main_sections/chat_projection.rs)
  now admits the last plugin-authored assistant prose message of each turn — abandoning it
  the moment an ordinary assistant message follows, because that copy is then the reply.
  Turns are separated there by a **turn change**, not by turn inputs: a wake or drain
  commits only its typed `Event` cause, and an input injected into a running turn commits a
  turn input carrying that same turn's id. So Phase 2b is load-bearing twice over — its
  cumulative counts also catch the mirror failure, where the wake's own reply arrives and
  an **earlier** turn's reply disappears from the projection. This is the mirror defect of
  FIG-984 and needs the same referee: **zero** rendered agent rows over a committed reply
  is as much a projection failure as two, whenever it happens.

Both defects share one shape: **two id namespaces projecting one logical message**. The
gate is therefore not "does the reply look right" but "does every layer agree on how many
messages exist".

**The invariant this runbook referees** is documented in
[the example's README](../../examples/agent-workbench/README.md): *"Either way a completed
turn leaves exactly one committed assistant copy."* The user side has an analogous rule
enforced in code — `suppressed_turn_input_message_ids` in
[`chat_projection.rs`](../../examples/agent-workbench/src/main_sections/chat_projection.rs)
suppresses the runtime's duplicate user copy by matching `MessageOrigin::TurnInput`, never an
id shape (FIG-972). Both regimes now carry unit coverage; this runbook is the judged
browser-surface layer over it, and it exists because the unit tests assert within one surface
while both defects were only visible **between** surfaces.

**Two lanes.** The rendered-surface answer key is driven only with the named development
provider scenarios below. They make no provider network calls and have exact event and text
markers. The existing one-send/one-wake count lane still uses OpenRouter: its prose and
termination style are nondeterministic, so that lane gates on role counts and records ids
rather than requiring a particular id shape.

## Judged rendered-surface answer key

This table is binding. Browser dispatch names come from the in-page
`__LASH_WORKBENCH_TURN_EVENT_HOOK__`; trace `runtime_stream_event` names such as `delta`,
`reasoning_delta`, and `usage` are a different vocabulary and must not be used to infer which
`handleTurnEvent` branch ran.

| Event or committed source | Rendered answer | Settled/reload answer |
|---|---|---|
| committed user message | one `message user` row, role `you` | durable in `/api/state.messages`; identical after reload |
| `assistant_prose_delta` then committed assistant | provisional `message assistant`, replaced by one committed `message assistant` | durable; no provisional duplicate |
| `reasoning_delta` | one `reasoning` disclosure | `/api/state.transcript.type == "reasoning"`; survives reload |
| `code_block_started` | one `code-block` disclosure in running state | same element settles; never a second code row |
| successful `code_block_completed` | the same `code-block`, summary `lashlang completed` | `/api/state.transcript.type == "code_block"`; survives reload |
| failing `code_block_completed` | the same `code-block fail`, with the exact code error | durable code block; survives reload |
| `tool_call_started` | one `tool pending` child **inside its code block** | completion updates it in place by `call_id`, or by the deterministic turn/name/arguments/graph/parent fallback when no id exists |
| successful/failed `tool_call_completed` | the same nested child becomes `tool` / `tool fail` with `completed` / `failed` badge | reload has an honest source-operation/outcome summary for each retained call, not the live call identity or details |
| `final_value` | terminal text/value is appended inside the current `message assistant`; no terminal-value row | committed assistant contains the same rendering; survives reload |
| `tool_value` | intentionally the same rendering as `final_value`; `tool_name` does not create a DOM distinction | committed assistant contains the same value; survives reload |
| committed failure message | one `message event`, role `event`, exact text `turn could not be completed`, no retry button | durable in `/api/state.messages`; same in sender, observer, and reload |
| ordinary provider/turn `error` activity | no `.message.error` row and no retry control in either sender or observer | the later committed failure message is the only rendered failure row |
| `postCommand` HTTP/fetch failure | transient `message error`, role `error`, with `retry turn` because the command recorded `lastRequest` | page-local and absent from backfill; do not confuse it with provider activity or the durable failure row |
| `model_attempt_reset` | removes only prose/reasoning chunks whose correlation ids are named | empty id arrays remove nothing; settled transcript contains no superseded text |
| `retry_status` | transient, turn-owned `message event retry-status` with attempt/max/reason/wait | removed only by the same turn's next request or settlement; delayed completion of another turn cannot clear it |
| committed message attachments | `message-attachments` inside the owning `message user` body | sourced from `message.attachments`, not a stream event; survives reload |
| `usage` | left-rail totals add that turn's `cumulative` counters to the last settled session baseline, keyed by turn id | totals remain monotonic on later turns and equal `/api/state.usage` after settlement |

`/api/state.transcript` has exactly three producible row types:
`["code_block", "message", "reasoning"]`. Tool summaries reload as children encoded on their
code block. The durable executed-call ledger retains only source operation and outcome for its
bounded tail (currently 128 calls); `calls_omitted` renders as an explicit nested omission row.
It does **not** retain call id, live tool name, arguments, result, or duration, so reload must not
claim those fields or exact live tool identity. Terminal values and failures reload through
committed messages; attachments reload from the owning message. Every settled producible
top-level row must return with an identical class histogram. Retry status and client-request
errors are explicitly transient and are judged before settlement, never included in the reload
histogram.

## Scenario-specific golden rules

1. **Count rows, never read prose.** A rendered row is a `#timeline .message` element; its
   role is the `user` / `assistant` class, not the visible `you` / `agent` label. Two rows
   whose bodies are byte-identical are still two rows — identical text is the defect
   signature, never an excuse to merge them.
2. **Reconcile all three layers at every step, and record the split.** Per
   [../RULES.md](../RULES.md), rendered DOM vs durable state vs logs must agree. Do not stop
   at "the DOM has two rows": determine whether the store has one committed assistant
   message (a **render** defect) or two (a **commit** defect projected faithfully), and
   whether the trace shows one execution or two. That split *is* the diagnosis and decides
   the pipeline stage the RCA names. Apply the narrow settlement exception in
   `RULES.md` for a journal-first process-command refusal: the attempt frame/trace preserves
   the provider value while the intent outcome and turn/API/DOM projection carry the typed
   refusal. Count that exact, identity-matched two-row settlement as designed behavior.
3. **One turn execution per unit of work.** Exactly one `turn_completed` trace record per
   composer send and exactly one per Red press. Extra assistant rows over one
   `turn_completed` are projection duplication; extra `turn_completed` records are a
   scheduler or wake-delivery fault, a different failure with a different owner.
4. **A wake adds no user row.** The Red press is a host event, not a chat input. The wake
   turn must add exactly one assistant row and leave the user-row count unchanged. A new
   `you` row for a button press is a projection failure.
5. **Settle by stability, not by a sleep.** After the `turn_completed` and idle gates, poll
   until the row and message counts are unchanged across several consecutive samples before
   counting. A duplicate that lands a second late must still be caught; a fixed sleep either
   misses it or passes by luck.
6. **Reload must preserve every durable identity.** The post-reload message/reasoning/code
   multiset must equal the settled pre-reload multiset exactly. Tool details use the narrower
   durable contract above: retained source operation/outcome summaries plus an explicit omitted
   count. Backfill is a second, independent projection of committed state; never manufacture
   unavailable live fields to make the two paths look identical.
7. **Scope everything to one session id.** Drive `/?session_id=<S>` and scope every read —
   `/api/state?session_id=<S>`, the `graph_nodes.session_id` filter, the product-event map
   key, and the trace's `context.session_id` — to that id. An unscoped read mixes other
   tabs' conversations into the counts and voids the run.
8. **Use the browser dispatch hook for event identity.** Before navigation, install
   `window.__LASH_WORKBENCH_TURN_EVENT_HOOK__ = (event, turnId) => ...` and record a deep copy
   of every call. Correlate DOM checkpoints to that buffer. Trace vocabulary is corroboration,
   not the event-side answer key.
9. **Count nested tool rows where they live.** For each code block record
   `code.querySelectorAll(":scope > .tool")`, each badge, and each available call id from the
   hook. A timeline-child count cannot see tools. One live call must produce one child before
   and after completion even when `call_id` is absent. Reload must reproduce each retained source
   operation/outcome summary and one explicit `calls_omitted` row when the ledger overflowed;
   arguments, results, duration, and stable call identity must remain unavailable.
10. **Separate live from settled classes.** Capture transient retry/client-error/running rows at
    their named checkpoint. For the reload identity gate compare only the settled histogram,
    after `Done`, idle, empty `active_turns`, and count stability. Retry rows are turn-owned: a
    delayed `Done` for turn A must not clear turn B's retry row.

## Deterministic rendered-surface lane

For every phase use a fresh data directory, session id, port, and artifact subdirectory. Boot
with `AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=<scenario> AGENT_WORKBENCH_DATA_DIR=<fresh>
AGENT_WORKBENCH_OPEN=0 just agent-workbench <port>`, require the startup warning to name the
exact scenario and the page to render model `dev/failure-paths`, then install the dispatch hook
before navigation. Teardown with `just agent-workbench-down <port>` before the next phase.

At every named checkpoint save four machine-readable extracts beside the screenshot:

- `*-dom.json`: top-level class histogram, code-block histogram, nested tool count/status, row
  text, and attachment ids;
- `*-state.json`: scoped `/api/state`, including `messages`, `transcript`, `usage`, and
  `active_turns`;
- `*-store.json`: scoped non-tombstoned conversation/transcript graph nodes from
  `<data-dir>/lash-sessions/durable-core.db`;
- `*-events-trace.json`: the in-page hook buffer plus the scoped `turn_completed`, LLM-call,
  code/tool, usage, and retry records from `trace.jsonl`.

Any DOM/API/store/trace disagreement is an Abort/RCA under the shared rules.

### Surface A — reasoning, successful code, and final value

Boot `rendered-surface`; submit any text. Poll the hook for `reasoning_delta`, capture the visible
`reasoning` disclosure as `10-reasoning-live.png`, then poll through
`code_block_started`, `code_block_completed`, and `final_value` to settled/idle. Require exactly
one `reasoning`, one successful `code-block` whose body contains `lashlang completed`, and one
assistant row containing the structured marker `FIG-1350 deterministic final value`. There is
no separate value row. Save `11-code-ok-final-settled.png` and the four `11-*` extracts.

Reload. Require transcript types exactly `["code_block", "message", "reasoning"]` after unique
sorting and require the complete settled class histogram, row bodies, and counts to equal the
pre-reload extract. Capture `12-code-ok-final-reload.png`.

### Surface B — nested tool start/complete and live `tool_value`

Boot `tool-value`; submit any text. Poll the hook for `tool_call_started`, then require exactly
one `.tool.pending` child under the one running `.code-block`, zero top-level `.tool` siblings,
and capture `20-tool-start-live.png`. The deterministic terminal tool remains pending long enough
for this objective poll; never replace the poll with a sleep.

Poll for `tool_call_completed`, `code_block_completed`, and `tool_value`, then settled/idle.
Require the same nested element count to remain one, its badge to be `completed`, the code summary
to report one tool, and the assistant row to contain `FIG-1350 deterministic tool value`. Record
that `tool_value` and `final_value` are DOM-indistinguishable by the answer key: both render their
value into the assistant row. Capture `21-tool-complete-value-settled.png` and the four `21-*`
extracts. Reload and require one nested completed durable-summary child with the same source
operation and outcome. Require its payload to omit call id, arguments, result, and duration;
capture `22-tool-complete-value-reload.png`.

### Surface C — failing code and durable error

Boot `code-failure`; submit any text and poll to settled/idle. Require one `code-block fail` whose
error contains `FIG-1350 deterministic code failure`, one durable `message event` with exact text
`turn could not be completed`, and no settled `.message.error` or retry button. Capture
`30-code-fail-error-settled.png` and the four `30-*` extracts. Reload and require the same failed
code row and durable event row with the same histogram; capture `31-code-fail-error-reload.png`.
An ordinary provider `error` activity before that message must add no row on either the sending
page (even though it has `lastRequest`) or a never-sent observer.

Separately prove the client row without calling a provider: after a successful page hydration,
abort or refuse a composer `POST /api/turn` in browser routing so `postCommand` has a
`lastRequest`. Require one transient `message error` with a `retry turn` button, capture
`32-client-error-retry-live.png`, then remove the route fault and reload. Require that transient
row to be absent and the durable transcript unchanged. This is the reachable client/network
error path; it is not the provider-failure answer.

### Surface D — correlated retry/reset with partial text

Boot `retry-reset-partial`; submit any text. Poll the hook for the exact superseded prose marker,
then for `model_attempt_reset` naming its prose correlation id and `retry_status`. While the retry
row is present require the superseded marker to be absent, the `message event retry-status` body
to name attempt/max/reason/wait, and capture `40-retry-reset-live.png`. Poll to settled/idle and
require exactly one assistant copy containing `FIG-1350 retry replacement`, no superseded marker,
no retry-status row, and one completed turn. Capture `41-retry-replacement-settled.png` and the
four `41-*` extracts. Reload and require the same settled histogram and no superseded marker;
capture `42-retry-replacement-reload.png`.

### Surface E — attachment row and complete live-to-settled identity

Boot `rendered-surface`. Upload a valid PNG through the UI, require its attachment id in the
committed user message's `attachments`, and submit. Capture the live user-row gallery as
`50-attachment-live.png`; after settled/idle capture `51-attachment-settled.png`. Require the
attachment to be inside the `message user` body and require no attachment stream event in the
hook buffer. Reload, require the same attachment id/link/image and complete settled histogram,
and capture `52-attachment-reload.png`.

### Deterministic scorecard

| Item | Objective gate | Verdict | Evidence |
|---|---|---|---|
| Dispatch vocabulary | every event class named by the in-page hook, never inferred from trace names | | hook extracts |
| Reasoning | one durable `reasoning`, same after reload | | `10-*`, `12-*` |
| Code success/failure | one in-place code row per turn with exact success/fail class | | `11-*`, `30-*`, reloads |
| Tool start/complete | one nested child updates pending → completed with or without call id; reload exposes only retained source operation/outcome and explicit omission | | `20-*` through `22-*` |
| Final/tool value | both exact values render inline in assistant; no separate row; intended indistinguishability recorded | | `11-*`, `21-*` |
| Durable/client errors | durable event survives reload without retry; client error is transient and retryable when `lastRequest` exists | | `30-*` through `32-*` |
| Retry/reset | named partial correlation retracted; retry status visible then removed; replacement single-copy | | `40-*` through `42-*` |
| Attachment | row sourced from committed message and identical after reload | | `50-*` through `52-*` |
| Usage | later-turn cumulative counters add to the settled session baseline monotonically; settled rail equals `/api/state.usage` | | every state/hook extract |
| Cross-check | DOM, API, durable graph, and trace agree at every settled checkpoint | | all four extracts per checkpoint |

## Working material

- Require `OPENROUTER_API_KEY`. Boot an empty, port-isolated stack with
  `AGENT_WORKBENCH_DATA_DIR=<fresh-tmp> AGENT_WORKBENCH_OPEN=0 just agent-workbench <port>`
  (or `bash scripts/agent-workbench-dev.sh up --port <port>` with the same environment).
  Gate `GET /healthz` → 200. Teardown on success or Abort is
  `just agent-workbench-down <port>`.
- Pick one run session id `<S>` = `runbook-chatproj-<run-id>` and open `/?session_id=<S>`.
  Gate the rendered session id and `/api/state?session_id=<S>.settings.session_id` against
  `<S>` before sending anything.
- UI affordances: the chat composer and its **send** control, the transcript timeline, the
  running/idle pill, and the left-sidebar **RED** trigger button.
- **Layer 1 — rendered DOM:** `#timeline .message.user` and `#timeline .message.assistant`
  row counts, plus each row's body text. The live stream renders a streaming assistant draft
  that is replaced when the committed copy arrives; only count after the settle gate, or a
  draft inflates the count.
- **Layer 2 — durable state:** the session graph in
  `<data-dir>/lash-sessions/durable-core.db`, table `graph_nodes`, filtered to
  `session_id = <S> AND tombstoned = 0`, reading `node_json` for
  `kind = "event"` nodes whose `event.Conversation.role` is `User` or `Assistant`; the app's
  projection `GET /api/state?session_id=<S>.messages` (roles `user` / `assistant`); and the
  product-event log `<data-dir>/product-events.json`, keyed by `<S>`, whose `event_ids`
  carry the projected message ids.
- **Layer 3 — logs:** `<data-dir>/trace.jsonl`, records with
  `context.session_id == <S>`. Count `type == "turn_completed"`; `context.turn_id` names the
  execution and distinguishes a composer turn (`workbench-turn-…`) from a wake
  (`workbench-queued-…`).
- Role vocabulary differs per surface: the store writes `User` / `Assistant`, the API
  `user` / `assistant`, the DOM classes `user` / `assistant` under `you` / `agent` labels.
  Normalize role before comparing; never treat a label difference as content drift.

Save every named artifact and API/store/trace extract under the run's artifact directory.

## Phase 0 — Boot and scope one session

Boot, gate `/healthz`, and open `/?session_id=<S>`. Require the composer, the RED trigger
button, an empty transcript, the rendered session id `<S>`, and
`/api/state?session_id=<S>` reporting `settings.session_id == <S>` with empty `messages`
and empty `active_turns`. All three layers start at zero: no `graph_nodes` conversation
rows, no `<S>` key in the product-event log, no `<S>` trace records. Screenshot
`00-scoped-empty.png`.

## Phase 1 — One composer send, one pair of rows

Send one short turn containing a unique literal marker, e.g.
`Reply with exactly this and nothing else: FIG985-PLAIN-<run-id> acknowledged`.

Gate, in order: one `turn_completed` for `<S>`; the idle pill and empty `active_turns`; then
the settle gate. Now require **exactly**:

- 1 rendered `.message.user` row and 1 rendered `.message.assistant` row;
- 1 `user` and 1 `assistant` message in `/api/state.messages`;
- 1 `User` and 1 `Assistant` conversation message in the session graph;
- 1 `turn_completed` record.

Record every message id at each layer. This is the FIG-972 regression gate: a second `you`
row, or a second committed `User` message for one send, fails it. Screenshot
`01-one-send.png` and save the four extracts as `01-one-send-{dom,state,store,trace}.json`.

## Phase 2 — Register the watcher, then press RED once

**2a — register.** Send `Let me know when I press a button` and let the agent register the
button watcher. Gate a second `turn_completed`, idle, and the settle gate, then require the
same invariant cumulatively: 2 user rows, 2 assistant rows, 2 committed pairs, 2
`turn_completed`. Screenshot `02-watcher-registered.png`. A duplicate here is already a
failure — do not press RED to "get to the real test".

**2b — one press, one wake.** Press the **RED** button **exactly once**. Gate a
`turn_completed` whose `context.turn_id` starts with `workbench-queued-`, then idle, then
the settle gate. Require **exactly**:

- the user-row count **unchanged** at 2 (golden rule 4);
- 3 rendered `.message.assistant` rows — one new row, not two;
- 3 `assistant` messages in `/api/state.messages` and 3 `Assistant` conversation messages in
  the session graph;
- 3 `turn_completed` records — one wake, not two.

The wake's single committed copy belongs to the **runtime** or to the **RLM protocol** — see
the termination table above — and there is **no** `workbench-assistant:<turn_id>` row in the
graph for it either way. Read the wake's committed assistant node before judging: a
`m_turn_<turn_id>_assistant` node carries one `Prose` part, while a reasoned answer is
committed as `m_rlm_<turn_id>_<iteration>_assistant_response` carrying `Reasoning` **and**
`Prose`. In that second case require, in addition to the counts below, that the wake's
`Prose` text appears verbatim in exactly one rendered agent row and in
`/api/state.messages` — a `reasoning` disclosure with no agent row over a committed
`assistant_response` node is the FIG-1406 defect, not a quiet model. Record which writer
held the copy in `03-red-press-store.json`. Re-read the **earlier** turns' rows here too:
the wake is a turn boundary that commits no turn input, and an earlier reply that was
rendered in Phase 1 or 2a and is missing now fails this phase exactly as a duplicate would. Count rows by role; never gate on a particular id being present, or
the gate fails on correct behavior. Two id-level traps here: `/api/state.messages` carries the
runtime id for this turn while the two composer turns carry workbench ids, and the
product-event log still lists `message:workbench-assistant:<wake_turn_id>` — that is the
**live** agent row, which retires when its turn stops running and has no durable counterpart.
Comparing ids across those two surfaces is therefore invalid; compare counts.

Screenshot `03-after-red-press.png` with the newest rows scrolled into view. If the
assistant count exceeds the `turn_completed` count, capture the **ids** of the surplus
messages from `/api/state` and the store before anything else: the id shapes name the two
writers (`m_turn_<turn>_assistant` = runtime terminal commit;
`workbench-assistant:<turn>` = the workbench's `commit_assistant_transcript`), which is the
whole RCA. Record each surplus message's `part_kinds` (the runtime copy carries a `Prose`
part, the workbench copy a `Text` part) and the wake turn's `done_reason`. Save
`03-red-press-{dom,state,store,trace}.json` and the duplicate ids as
`03-duplicate-ids.json`.

Note which layers agreed. A duplicate that reaches the store means the DOM, `/api/state`, and
`graph_nodes` **all** report the surplus and only the trace dissents — a commit-side defect
faithfully rendered, so the RCA stage is turn terminalization, not render. A surplus the store
does **not** show is the opposite: a render-side defect, and the RCA stage is the event stream
or the browser's id dedupe. Do not report "two rows" without naming which of these it is.

## Phase 3 — Reload and require an identical multiset

Record the pre-reload row multiset (role class + body text per row). Reload the page, gate
the rendered session id and the settle gate, and require:

- the post-reload row multiset **equals** the pre-reload multiset;
- the per-role counts still equal Phase 2b's store and trace counts;
- no reload-only and no live-only row.

The live path renders from the event stream and the backfill path renders from the committed
projection; agreement between them is the gate. Screenshot `04-after-reload.png` and save
both multisets as `04-reload-multiset.json`.

## Phase 4 — Teardown and score

Run `just agent-workbench-down <port>` and confirm the workbench and its port-derived
Restate container are gone.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Boot/scope | `/healthz` 200; rendered and API session id both `<S>`; all three layers empty | | `00-scoped-empty.png` |
| One send, one pair | 1 user + 1 assistant row = 1+1 API = 1+1 store = 1 `turn_completed` | | `01-one-send.png`, `01-one-send-*.json` |
| Watcher registration | cumulative 2+2 rows = 2+2 API = 2+2 store = 2 `turn_completed` | | `02-watcher-registered.png` |
| One press, one wake | exactly 1 `workbench-queued-` `turn_completed`; user rows unchanged | | `03-red-press-trace.json` |
| One press, one new agent row | 3 assistant rows = 3 API = 3 store = 3 `turn_completed` | | `03-after-red-press.png`, `03-red-press-*.json` |
| Reasoned wake reply | the wake's committed `Prose` renders in exactly one agent row and in `/api/state.messages`, whichever writer holds the copy | | `03-red-press-{dom,state,store}.json` |
| Reload identity | post-reload row multiset equals pre-reload multiset | | `04-after-reload.png`, `04-reload-multiset.json` |
| Three-layer cross-check | every step reconciles DOM vs durable vs trace pairwise; no mismatch normalized away | | all four extracts per phase |
| Duplicate attribution | on any surplus row, the surplus message ids and their writing namespaces are recorded | | `03-duplicate-ids.json` |

**Aggregate:** did every unit of conversational work — two composer sends and one button
press — project into exactly one rendered row per role, identically on the live and reload
paths, with the rendered DOM, the durable session graph, the state and product-event
projections, and the turn trace all agreeing on how many messages and how many executions
the session contains?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
