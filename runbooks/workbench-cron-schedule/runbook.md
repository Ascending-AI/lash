# E2E Scenario: Workbench Cron Schedule to Turn

> **Read [../RULES.md](../RULES.md) first** — especially browser automation, polling,
> named-checkpoint screenshots, real-token use, the three-layer cross-check, Abort/RCA,
> and teardown ownership. This scenario is the judged cron companion to
> [../workbench-trigger-lifecycle/runbook.md](../workbench-trigger-lifecycle/runbook.md)
> and [../workbench-chat-projection/runbook.md](../workbench-chat-projection/runbook.md).

**Purpose.** Prove one recurring schedule end to end through the browser-facing reference
host: a chat turn makes the agent register `cron.Schedule`, Restate arms and fires the
schedule, the trigger store reserves exactly one process delivery per tick, the process
emits exactly one durable wake, queued work drains once, and the resulting turn projects
as exactly one new agent row. Then prove the same registration can be disabled, resumed
under the same identity, and deleted without a late or doubled tick.

**Model-judged registration.** The schedule exists only if the agent authors and executes
the Lashlang registration. The sidebar's **try scheduling** card is explanatory and has no
action. If the first prompt yields no valid registration, save the first turn's evidence
and make one sharper retry. If that retry also yields no valid registration, mark the run
**VOID (model registration)**, not product FAIL, and teardown. Once registration exists,
every later gate is a product gate.

**Reset boundary.** Session reset and cron cancellation are covered by the session-id
retirement/reset scenario. Do not reset in this runbook; disable and delete the captured
registration directly.

## Scenario-specific golden rules

1. **One identity throughout.** Capture `subscription_key`, `subscription_id`,
   `source_key`, and Restate `job_key = <session-id>:<source-key>` after registration.
   Re-enable must preserve all four. A replacement registration is not a pass.
2. **Count one causal chain per tick.** For each accepted `fired_at`, require exactly one
   `cron.restate.run`, one trigger occurrence, one delivery/process id, one wake delivery,
   one completed queued-work batch, one `queued_work.restate.start`, one queued
   `turn_completed`, and one new rendered/API/store assistant message carrying the marker.
   Save ids and join them; equal counts without matching identities are insufficient.
3. **Reconcile conversation layers per tick.** Apply the three-layer rule after the
   registration turn and after every tick: DOM role rows, `/api/state.messages`, committed
   `graph_nodes` conversation messages, and `turn_completed` traces must have pairwise-equal
   deltas. A cron wake adds no user row.
4. **Poll scheduled boundaries, never sleep as logic.** Read Restate `next_execution_time`
   before every positive or negative window. Poll the objective surfaces until the tick
   chain settles or until that advertised boundary plus 20 seconds passes. Record the
   latency from advertised time to `cron.restate.run`, occurrence persistence, wake
   enqueue, queued-turn start, assistant commit, and rendered row. A timeout is FAIL.
5. **Absence needs a clock and all layers.** After disable and delete, retain the last
   advertised next-execution boundary. Continuously poll until that boundary plus 20
   seconds while requiring no new cron run, occurrence, delivery, process wake, queued
   batch, completed turn, API/store message, or DOM row. Also require Restate `info` to be
   `null`. A quiet transcript alone proves nothing.
6. **A second tick is mandatory.** Do not infer recurrence from a re-armed timestamp.
   Observe two complete consecutive chains before disabling, each adding exactly one agent
   row. Accumulated or double-fired work is FAIL.
7. **Capture before mutation.** Save the full registration, Restate info, database rows,
   state projection, DOM extract, and trace slice for each phase before clicking the next
   lifecycle control.

## Evidence map

Use one session id `<S> = runbook-cron-<run-id>` and one distinctive marker
`FIG996-CRON-<run-id>` everywhere.

| Layer | Honest evidence | Required join |
|---|---|---|
| Rendered UI | registrations rail; `#timeline .message.user` and `.assistant`; enabled action styling | registration name/key shown; each tick adds one assistant row containing `<marker>` and no user row |
| App API | `GET /api/triggers?session_id=<S>` and `GET /api/state?session_id=<S>` | registration identity/config and message ids/counts match the UI/store |
| Restate object | empty-body `POST <ingress>/WorkbenchCronJob/<url-encoded-job-key>/info` | `source_key` and cron expression match registration; `next_execution_time` advances and `last_fired_at` equals the tick; disabled/deleted reads `null` |
| Trigger store | `<data-dir>/triggers.db`: `trigger_subscriptions`, `trigger_occurrences`, `trigger_deliveries` | subscription `source_key` joins each occurrence; occurrence id joins exactly one delivery and its `process_id` |
| Process registry | `<data-dir>/processes.db`: `processes`, `process_events`, `process_wake_deliveries` | delivery `process_id` joins one `process.wake` event/delivery for `<S>`; record sequence/state |
| Session store | `<data-dir>/lash-sessions/durable-core.db`: `queued_work_batches`, `queued_work_items`, `graph_nodes` (`node_json.kind == "event"`, conversation at `event.Conversation`) | queued item payload joins the wake/process identity; batch is completed/drained; committed assistant marker count advances by one |
| Trace | `<data-dir>/trace.jsonl`, filtered by `context.session_id == <S>` | per tick: one `agent_workbench.cron.restate.run`, one `.emit_completed`, one `queued_work.restate.start`, and one `turn_completed`; join `fired_at`, process ids, and queued turn id |

`WorkbenchCronJob.info` is the honest schedule read: it is the object's shared handler over
the Restate-persisted `cron_state`, not the workbench's in-memory set. Use the port-derived
Restate ingress advertised by the dev runner metadata/log; do not assume the default port.
URL-encode the object key because its source key contains punctuation.

Save each query as JSON rather than relying on terminal output. SQLite extracts should be
JSON arrays produced with `sqlite3 -json` (or an equivalent read-only client) and must
include the stored `record_json`/payload JSON needed to establish identities.

## Working material

- Require `OPENROUTER_API_KEY`. Use port `3180` only, data directory
  `/workspace/tmp/fig996-state/data`, and a fresh artifact directory. Boot with
  `AGENT_WORKBENCH_DATA_DIR=/workspace/tmp/fig996-state/data AGENT_WORKBENCH_OPEN=0 just
  agent-workbench 3180`. Gate `GET /healthz` to 200.
- Drive Chromium with a PEP 723 Playwright script under the artifact directory and `uv run`.
  Navigate with `wait_until="domcontentloaded"`, then explicit assertions.
- Use the cron expression `* * * * *` in UTC: a minute schedule has an upper wait near 60
  seconds, so use a 90-second positive gate and the advertised boundary plus 20 seconds for
  negative observation. Do not substitute a seconds cron; this scenario judges the
  user-facing recurring-reminder shape.
- UI affordances: chat composer and **send**, transcript, running/idle pill, registrations
  rail with **disable**, **re-enable**, and **delete**, and rendered session id.
- Teardown on success, FAIL, or VOID is `just agent-workbench-down 3180`. Preserve evidence,
  then remove `/workspace/tmp/fig996-state` and confirm port 3180 and its port-derived
  Restate container are gone.

## Phase 0 — Boot and scope an empty session

Before boot, require the scoped data directory not to exist. Boot, gate `/healthz`, open
`/?session_id=<S>`, and require the rendered session id and
`/api/state?session_id=<S>.settings.session_id` both equal `<S>`. Require the composer,
the decorative **try scheduling** card, an empty transcript, an empty registrations rail,
empty `state.messages`, empty `active_turns`, no subscription rows for `<S>`, no
conversation rows for `<S>`, and no scenario-execution trace records for `<S>`. Preserve
but do not count the expected ambient `agent_workbench.api.work.response` records created
by the rendered work rail's polling.

Save `00-baseline-{state,triggers,store,trace}.json` and screenshot
`00-scoped-empty.png`.

## Phase 1 — Ask the agent to register one recurring reminder

Send this outcome-level request, substituting the marker and a stable literal name/key:

> Schedule a recurring reminder named `fig996-cron-reminder` every minute in UTC. Each
> tick must wake this chat with the literal marker `<marker>`. Keep exactly one enabled
> registration and tell me when it is registered.

Wait for the registration turn to terminalize, the idle pill, empty `active_turns`, and
stable transcript/message counts. If no valid registration appears, save
`01-first-attempt-*` and retry once with: “Use `cron.Schedule({ expr: "* * * * *", tz:
"UTC" })`, an explicit process input from `trigger.event`, a stable literal
`subscription_key`, and `wake` containing `<marker>`; execute the registration now.” A
second registration failure voids the run.

Poll until `/api/triggers?session_id=<S>` returns exactly one enabled registration named
`fig996-cron-reminder` with source type `cron.Schedule`, expression `* * * * *`, timezone
`UTC`, and a stable literal `subscription_key`. Require the registrations rail to show the
same name/config and a **disable** action. Query Restate `info` and require matching
`source_key`/expression, nonempty future `next_execution_time`, and nonempty
`next_execution_id`. Require exactly one live durable subscription row matching the API.

Cross-check the registration chat turn across DOM, state, store, and trace counts. Save
`01-registration-{dom,state,api,subscription,restate,trace}.json` and screenshot
`01-registered.png`.

## Phase 2 — Observe two real ticks and exact tick-to-turn chains

Take the Phase 1 counts/ids as baseline. For tick 1, poll Restate `info`, trace, all three
databases, `/api/state`, and the DOM until the chain settles. Bound the wait to the saved
`next_execution_time` plus 20 seconds (90 seconds maximum). Require:

- `last_fired_at` equals this tick and `next_execution_time` advances;
- exactly one new cron run and one matching emit-completed trace;
- exactly one new occurrence for the captured source key and exactly one delivery whose
  process id appears in emit-completed;
- exactly one process wake delivery/event for that process and target `<S>`;
- exactly one new completed queued-work batch containing that wake, exactly one new
  `queued_work.restate.start`, and exactly one queued `turn_completed`;
- no new user row/message and exactly one new assistant row/message at DOM, API, and store,
  with the assistant text containing `<marker>`; and
- all ids/count deltas remain stable across consecutive polls before capture.

Record the six latency points named in golden rule 4. Save `02-tick-1-{dom,state,cron,
trigger,process,queue,store,trace,latency}.json` and screenshot `02-first-tick.png` with the
newest transcript row and registration visible.

Repeat the entire gate against tick 1's counts and the newly advertised
`next_execution_time`. The second tick must have a distinct `fired_at`, occurrence id,
delivery/process id, wake id, queued batch/turn id, and assistant message id, while adding
the same exact count deltas. Save `03-tick-2-{dom,state,cron,trigger,process,queue,store,
trace,latency}.json` and screenshot `03-second-tick.png`.

## Phase 3 — Disable and prove the next boundary is silent

Capture tick 2's `next_execution_time` before mutation. Click **disable** on the captured
registration. Poll until API and durable subscription show the same identity with
`enabled: false`, the rail shows **re-enable**, and Restate `info` returns `null`. Save
`04-disabled-{api,subscription,restate}.json` and screenshot `04-disabled.png`.

Continuously poll all tick-chain counters through the captured boundary plus 20 seconds.
Require every counter and DOM/API/store role count to remain at the tick 2 baseline and
Restate `info` to remain `null`. Save timestamped samples plus the final extracts as
`05-disabled-silence-{samples,state,cron,trigger,process,queue,store,trace}.json` and
screenshot `05-disabled-silent.png`.

## Phase 4 — Re-enable the same registration and require ticks to resume

Click **re-enable**. Poll until API and the durable row show the captured identity enabled,
the rail shows **disable**, no second subscription exists, and Restate `info` again exposes
a future `next_execution_time`. Save `06-reenabled-{api,subscription,restate}.json`.

Observe one full tick using the Phase 2 causal-chain gate. Require exactly one new identity
at every per-tick layer and exactly one new marker-bearing assistant row/message, with no
new user row. Save `06-resumed-tick-{dom,state,cron,trigger,process,queue,store,trace,
latency}.json` and screenshot `06-reenabled-fired.png`.

## Phase 5 — Delete and prove permanent silence

Capture the resumed tick's `next_execution_time`. Click **delete** on the captured
registration and accept confirmation. Poll until the captured key is absent from
`/api/triggers`, its durable subscription is tombstoned or absent from the live-row query,
the registrations rail reads `none in this session`, and Restate `info` returns `null`.
Save `07-deleted-{api,subscription,restate}.json` and screenshot `07-deleted.png`.

Continuously poll all tick-chain counters through the captured boundary plus 20 seconds.
Require no new cron run, occurrence, delivery/process, wake, queued batch/turn,
`turn_completed`, message, or row, and require Restate `info` to remain `null`. Save
`08-deleted-silence-{samples,state,cron,trigger,process,queue,store,trace}.json` and
screenshot `08-deleted-silent.png`.

## Phase: non-current remains live; retirement cancels

This phase distinguishes a valid non-current schedule from a retired-session orphan. Do not use reset for the pointer rotation because reset also deletes the old session.

1. On current session `S0`, register one `cron.Schedule` with a two-second expression. From the `agent_workbench.cron.restate.sync_upserted` record whose trace context is scoped to `S0`, capture its exact `payload.job_key` as `J` and require that `J` has the `{S0}:` prefix.
2. Record the current count of `agent_workbench.cron.restate.run` records whose payload has both `job_session_id == S0` and `job_key == J`.
3. Open a second scoped tab bound to a fresh session `S1` and make `S1` the active/current workbench session through the supported UI affordance; leave `S0` alive and undeleted, and do not cancel `J`.
4. Wait for two schedule intervals. PASS only if the scoped run-record count for `(S0, J)` increases by at least two, both new records say `decision_basis == "session_store_meta_present"` and `session_state == "live"`, and there is no scoped `agent_workbench.cron.restate.zombie_cancelled` record. This is the non-current-live gate.
5. Delete `S0` through the supported session-delete path.
6. PASS only if the supported delete path emits exactly one scoped typed cancellation record for `(S0, J)` and `WorkbenchCronJob/J/info` returns `null` after that record. Capture the exact cancel/sync event name and payload from the trace during the run and pin them in the execution report; for the current supported path this is `agent_workbench.cron.restate.cancel` with `payload.job_key == J` and `payload.reason == "reset"` in `S0`'s trace context. This is the retired-cancel gate.
7. Record the scoped run and occurrence counts after that cancellation, then wait two more schedule intervals. PASS only if neither count changes and no delivery, queued wake, or assistant tick output attributable to `(S0, J)` appears. This is the post-retirement-silence gate.

Overall PASS requires ticks to continue after pointer rotation and stop only after deletion. A cancellation before deletion is "cancelled because non-current" and fails the phase; absence of the typed delete-path cancellation or any tick after it also fails the phase.

Tick-time `agent_workbench.cron.restate.zombie_cancelled` is out of scope for this browser runbook: the supported session-delete path proactively cancels the session's cron jobs, so no scheduled tick can observe the retired session. That crash-window defense-in-depth is owned by the FIG-1018 `cron_tick_decision_cancels_a_retired_session_with_typed_trace` and `cron_tick_cancels_a_retired_session_with_typed_decision` tests in `examples/agent-workbench/src/restate_cron_tests.rs`; FIG-1071 extends the decision with registration disposition.

## Phase 6 — Reload identity, teardown, and score

Record the pre-reload transcript multiset (role class + body text + rendered identity),
reload `/?session_id=<S>`, gate the same session id and stable counts, and require the
post-reload multiset to equal the pre-reload multiset. Require the registration to remain
absent in UI/API/store and no new trace/tick-chain row during reload. Save
`09-reload-identity.json` and screenshot `09-after-reload.png`.

Run `just agent-workbench-down 3180`; confirm the workbench port is closed and the
port-derived Restate container is absent. Preserve the artifact directory, then remove
`/workspace/tmp/fig996-state` and confirm it is gone.

| Item | Objective gate | Verdict | Evidence |
|---|---|---|---|
| Boot/scope | `/healthz` 200; rendered/API session ids equal `<S>`; UI/API/store/trace baselines empty | | `00-scoped-empty.png`, `00-baseline-*.json` |
| Registration | one enabled cron registration agrees in rail, API, durable row, and Restate object | | `01-registered.png`, `01-registration-*.json` |
| First tick | one joined cron run → occurrence/delivery → process wake → queued drain → turn → marker row | | `02-first-tick.png`, `02-tick-1-*.json` |
| Recurrence | second distinct tick adds exactly one joined chain and one agent row | | `03-second-tick.png`, `03-tick-2-*.json` |
| Disable silence | same registration disabled; Restate object null; advertised next window adds nothing | | `04-disabled.png`, `05-disabled-silent.png`, `05-disabled-silence-*.json` |
| Re-enable | same identity re-armed; next tick adds exactly one chain and row | | `06-reenabled-fired.png`, `06-reenabled-*.json`, `06-resumed-tick-*.json` |
| Delete silence | live registration absent; Restate object null; advertised next window adds nothing | | `07-deleted.png`, `08-deleted-silent.png`, `08-deleted-silence-*.json` |
| Three-layer projection | every registration/tick delta reconciles DOM, API/store, and trace pairwise | | per-phase DOM/state/store/trace extracts |
| Reload identity | post-reload transcript multiset equals pre-reload; registration stays absent | | `09-after-reload.png`, `09-reload-identity.json` |
| Teardown | port 3180 closed; port-derived Restate container absent; scoped state directory removed | | teardown section in the execution report |

**Aggregate:** did one agent-authored minute schedule recur through two exact
schedule-to-tick-to-wake-to-turn chains, stop atomically while disabled, resume under the
same durable identity, disappear permanently when deleted, and preserve an exact transcript
on reload?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
