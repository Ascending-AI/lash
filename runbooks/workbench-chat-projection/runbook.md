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
- **FIG-984** — one trigger-button press rendered the agent reply **twice**. Two committed
  assistant copies of one turn's text reach the browser: the runtime's terminal commit
  `m_turn_<turn_id>_assistant`, materialized by `materialize_terminal_output` in
  [`materialize.rs`](../../crates/lash-core/src/runtime/turn_boundary/materialize.rs) for a
  `TurnFinish::AssistantMessage` outcome, and the workbench's own
  `commit_assistant_transcript` copy `workbench-assistant:<turn_id>` in
  [`restate.rs`](../../examples/agent-workbench/src/restate.rs). That helper's idempotence
  guard only looks for **its own** id, so it never sees the runtime's copy. The browser
  deduplicates by message id (`renderedMessages` in
  [`index.html`](../../examples/agent-workbench/assets/index.html)), so two ids carrying one
  text render as two rows.

  The discriminator is the turn's **termination**, not the trigger path: the runtime writes
  its terminal copy only for a `TurnFinish::AssistantMessage` outcome, which the trace
  records as `done_reason: "assistant_message"`. A turn that finishes through Lashlang
  (`done_reason: "final_value"`) leaves only the workbench's copy and renders correctly. The
  trigger wake is the *reliable* repro because a wake with nothing to execute answers as bare
  prose; treat the button path as the reproduction, and `done_reason` as the mechanism.

Both defects share one shape: **two id namespaces projecting one logical message**. The
gate is therefore not "does the reply look right" but "does every layer agree on how many
messages exist".

**The invariant this runbook referees** is already documented in
[the example's README](../../examples/agent-workbench/README.md): *"Canonical assistant rows
use `workbench-assistant:<turn_id>` in both the live product event and durable session
transcript, so a live/canonical pair is one row, never two."* The user side enforces it in
code — `suppressed_turn_input_message_ids` in
[`chat_projection.rs`](../../examples/agent-workbench/src/main_sections/chat_projection.rs)
suppresses the runtime's duplicate user copy by matching `MessageOrigin::TurnInput`. There is
no assistant-side equivalent. The unit suite does not close the gap either: it asserts the
`workbench-assistant:*` id is unique **within the product-event log**, which stays true while
a second assistant row exists in the durable transcript under a different id. That is exactly
why this scenario counts across layers instead of within one.

**Real tokens.** Turns and the wake go through OpenRouter, so prose and termination style
are nondeterministic. No exact model wording is an answer key. The answer key is **counts
and identities**: rendered rows per role, committed conversation messages per role,
projected messages, and completed turn executions.

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
   the pipeline stage the RCA names.
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
6. **Reload must be an identity, not a re-derivation.** The post-reload row multiset must
   equal the pre-reload multiset exactly. Backfill is a second, independent projection of
   the same conversation: a duplicate that appears **only** after reload, or **only** before
   it, localizes the defect to one of the two paths and is as much a failure as a duplicate
   in both.
7. **Scope everything to one session id.** Drive `/?session_id=<S>` and scope every read —
   `/api/state?session_id=<S>`, the `graph_nodes.session_id` filter, the product-event map
   key, and the trace's `context.session_id` — to that id. An unscoped read mixes other
   tabs' conversations into the counts and voids the run.

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

Screenshot `03-after-red-press.png` with the newest rows scrolled into view. If the
assistant count exceeds the `turn_completed` count, capture the **ids** of the surplus
messages from `/api/state` and the store before anything else: the id shapes name the two
writers (`m_turn_<turn>_assistant` = runtime terminal commit;
`workbench-assistant:<turn>` = the workbench's `commit_assistant_transcript`), which is the
whole RCA. Record each surplus message's `part_kinds` (the runtime copy carries a `Prose`
part, the workbench copy a `Text` part) and the wake turn's `done_reason`. Save
`03-red-press-{dom,state,store,trace}.json` and the duplicate ids as
`03-duplicate-ids.json`.

Note which layers agreed: a duplicate that reaches the store means the DOM, `/api/state`,
and `graph_nodes` will **all** report the surplus and only the trace dissents. That is a
commit-side defect faithfully rendered — the RCA stage is turn terminalization, not render.
The product-event log is a third opinion worth capturing: it records only the workbench's
own publishes, so it can undercount the rendered rows even while the render is correct.

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
