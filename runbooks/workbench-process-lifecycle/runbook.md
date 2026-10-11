# E2E Scenario: Workbench Runtime Processes — Declared Scope Lifetimes

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface,
> screenshot, polling, real-token, Abort/RCA, and teardown rules.

**Purpose.** Prove [ADR 0108](../../docs/adr/0108-a-process-lives-until-a-scope-its-start-could-reach.md)
through the Agent Workbench: a process's recorded lifetime determines what session
deletion cancels. Provenance and observers do not choose its lifetime. Also prove
global operator cancellation and the host's retention of finished records.
[ADR 0110](../../docs/adr/0110-the-engine-owns-process-recovery.md) owns recovery,
not a guarantee that every process survives its originating session.

**Declared lifetimes.** The workbench installs `SessionProcessAdminPluginFactory`
with `lifetime::session_or_starter` in
`examples/agent-workbench/src/main_sections/bootstrap.rs`. A named VM definition
started through `processes.start` from a session therefore records `Until(session)`.
The factory's policy is resolved against `StartCx` and stored as `LifetimeDecision`;
asking the model for a survivor does not change it. `DelegationPluginFactory` uses
`lifetime::starter`: a foreground turn's `spawn_agent` records `Until(turn)`.
Host-trigger delivery also explicitly chooses `Until(owner_session)` in
`examples/agent-workbench/src/host_triggers.rs`; host provenance is not detachment.

The workbench exposes no Detached start: `processes.start` accepts only definition,
definition_id, args and label, and rejects additional fields. This judged row uses
the declared lifetimes above. Detached survival is deterministic companion evidence;
asking the model for a survivor or substituting a host trigger cannot demonstrate it.

**Real tokens.** Use the matrix's economy model and record the served model from
provider evidence. Gate on names, ids, lifetime rows and API/UI agreement.

**Store axis.** The matrix emits this row for SQLite and PostgreSQL.
Follow [the PostgreSQL procedure](../postgres-store.md) for the PG boot,
companion selector, table mapping, startup record and post-host preflight.
Apply every phase and scorecard gate below unchanged; store selection does
not change process lifetimes or session-deletion expectations.

## Scenario-specific golden rules

1. **Record the actual lifetime before acting.** All three Phase 1 VM starts must
   record `Until` the originating session. Do not infer a lifetime from a name,
   originator or observer row.
2. **Cancel one process before deleting the session.** This separates explicit
   operator cancellation from the scope-close cancellation of the other live process.
3. **Session deletion closes its scope.** A live `Until(session)` process receives
   `process.cancel_requested` and ends cancelled. A `Detached` process receives no
   scope-close cancel and stays live in host-wide `GET /api/work` until it ends.
4. **Retention is separate.** After the tombstone, the workbench calls
   `prune_processes_originated_by` in
   `examples/agent-workbench/src/main_sections/host_ingress.rs`, from
   `examples/agent-workbench/src/main_sections/session_fence.rs`. It prunes only
   finished records the deleted session originated. Live Detached rows and other
   originators' rows are excluded. Capture terminal evidence before pruning;
   absence from the rail alone proves neither cancellation nor completion.
5. **Use the reset/new-session control.** Require the rendered session label to
   change with the API. Never stop the web process to simulate session deletion.
6. **The host rail is runtime-wide.** Query `/api/work` without `session_id` after
   deletion. An explicit old-session query is fenced. Terminal rows normally leave
   the rail after `WORK_RAIL_RETIRED_WINDOW_MS` (10 seconds); that visibility window
   does not delete a durable record. Session-delete retention can remove it sooner.

## Companion laws — existing full test paths

Run each once on SQLite; require an executed count of one for each selector:

```sh
. ./env.sh
kiln test //crates/lash-core:lash-core__unit_test --test_arg=runtime::durable::scope_end_tests::a_session_close_cut_at_any_step_resumes_there_and_deletes_nothing_while_a_process_lives
kiln test //crates/lash-durable-test:core_node_processes__test --test_arg=process_endstates::a_detached_process_outlives_its_deleted_session_and_accepts_host_cancellation::on_sqlite_memory
kiln test //examples/agent-workbench:agent-workbench__unit_test --test_arg=process_work_tests::session_delete_reclaims_the_deleted_sessions_terminal_work
kiln test //examples/agent-workbench:agent-workbench__unit_test --test_arg=process_work_tests::work_rail_keeps_a_nonterminal_process_past_the_retirement_window
```

The first law pins session-close cancellation and the close waiting for its
`Until(session)` child to end across every close-step cut. The second pins Detached
survival and host cancellation after deletion; it does not establish a workbench
Detached start affordance or natural completion. The third pins finished-record
retention while live Detached and other originators' work stays. The fourth pins
live host-rail visibility regardless of age and the terminal visibility window.

## Working material

- Boot with fresh data and run directories through the launcher:
  `AGENT_WORKBENCH_DATA_DIR=<fresh-data> AGENT_WORKBENCH_RUN_DIR=<fresh-run> AGENT_WORKBENCH_OPEN=0 bash scripts/agent-workbench-dev.sh up --port <port>`.
  Gate `GET /healthz` → 200; teardown with `just agent-workbench-down <port>`.
- Browser: chat composer, work drawer, per-process **cancel**, reset/new-session,
  rendered session id. Open the work drawer before each work checkpoint screenshot.
- API: `GET /api/state`, `GET /api/work`, `POST /api/work/{process_id}/cancel`,
  and the reset control's `POST /api/reset`.
- Disk: `<data-dir>/lash-sessions.db`, including `processes`, `process_events`,
  `process_observers`, `session_meta`, `session_head`, `control_intents` and
  `deleted_sessions`; `<data-dir>/trace.jsonl` and `<data-dir>/session-id`.

## Phase 0 — Boot and record owner identity

Require `OPENROUTER_API_KEY`; a missing key is a harness gap → Abort. Boot, poll
`/healthz`, open the browser, and require the rendered session id to match
`/api/state.settings.session_id` and `<data-dir>/session-id`. Record the originating
session id. Screenshot `00-ready.png`.

## Phase 1 — Start three Until(session) VM processes

Ask the agent to define and start three named VM processes through `processes.start`,
without foreground-awaiting their handles. An uncalled top-level `const` bound to an
`async` arrow declares the immutable definition. Pass its name as the start's label.

- `FIG425_session_bound_<runid>` loops over 2-second sleeps for 240 seconds, then
  returns a literal marker that must never be reached after deletion.
- `FIG425_operator_cancel_<runid>` uses the same loop, with another forbidden marker.
- `FIG425_finished_<runid>` returns a literal success marker immediately.

Use the loop below for both live processes; `sleep` takes milliseconds:

```typescript
let elapsedSeconds = 0;
while (elapsedSeconds < 240) {
  await sleep(2000);
  elapsedSeconds += 2;
}
```

Require both long-lived ids non-terminal in `/api/work`, matching rendered cards,
and the third id completed. Record all ids, observer rows and their stored lifetime
scope. Require each scope to be the recorded originating session; store labels use
`until` rather than the prose spelling `Until(session)`. Save `01-running-work.json`
and `01-running-store.json`; screenshot `01-processes.png`.

## Phase 2 — Cancel one live process through the work drawer

Press the `FIG425_operator_cancel_<runid>` card's **cancel** button and capture
`POST /api/work/<process_id>/cancel`: require `accepted: true` for the exact id.
Require ordered `process.cancel_requested` then `process.cancelled` evidence in
`process_events`, a cancelled card, and absence of its forbidden success marker.
The session-bound process must still be non-terminal. Save `02-cancel-receipt.json`
and `02-before-delete-store.json`; screenshot `02-operator-cancelled.png`.

Capture the completed process's success marker and the explicitly cancelled
process's terminal before deletion: both records are eligible for retention cleanup.

## Phase 3 — Delete the session and observe its scope close

Use reset/new-session while capturing its response. Poll until the rendered and API
session ids change and the old session has a deletion tombstone, no live
`session_meta`/`session_head` rows, and no `process_observers` edges.

Require `FIG425_session_bound_<runid>` to receive a scope-close cancellation and end
cancelled before physical deletion completes. Capture its cancel and terminal events
while closing, before retention can prune them. Its forbidden marker must be absent.
The `api.session.delete.deleted` trace's `process_retention.pruned_processes` must
account for the finished session-originated records, including the two Phase 2
terminals; require their records to be no longer retained. Do not equate a missing
`/api/work` row with proof of the terminal. Save `03-after-delete-*.json` with the
terminal/cancel evidence and retention report; screenshot `03-session-closed.png`.

## Phase 4 — Cancel a background session turn and keep working (FIG-884)

Cancelling a *background session turn* — a subagent process, whose input is a session turn
rather than a process definition — used to wedge the session that started it: the child
turn's registration was only removed after the child await, so a cancelled process left a
permanent "running turn" behind. The session then refused to close the child and rejected
every later turn on it. This phase proves the registration is released by cancellation.

The workbench's `DelegationPluginFactory` uses `lifetime::starter`
(`examples/agent-workbench/src/main_sections/bootstrap.rs`): a `spawn_agent` started
by this foreground turn records `Until(turn)`, not `Until(session)` or `Detached`.
Keep the starting turn open until the child is running and the cancel is accepted;
ending that turn already requests cancellation of its child. Cancellation of the
process does not itself delete the child's session.

Do:

1. In the current session, ask the agent to start a **subagent** that keeps working for
   several minutes (a long-running research/loop prompt), and let it reach a non-terminal
   card in the work rail. Pin the shape in the prompt the way Phase 1 pins its wait shape:
   the request must produce a `spawn_agent` subagent — a process whose `/api/work` row
   carries a `child_session_id` — not a plain Lash VM process. Prose alone does not pin
   it: an economy run asked in prose produced `identity_kind = lash_vm` with no
   `child_session_id`, completing in 0.6 s, and the gate could not be read from that turn.
   If the first attempt yields a Lash VM process, re-prompt once naming the subagent
   explicitly, then stop; a bounded single retry, not repeated re-prompting. Capture its process id **and its child session id** as
   `03b-subagent-running.json` — the child session id is the subagent's
   `process.child_session_id` in the `/api/work` row; record it verbatim, the disk gate names it. Screenshot
   `03b-subagent-running.png`.
   **Before cancellation, record the durable-catalog baseline.** In
   `<data-dir>/lash-sessions.db`, query row counts for the recorded child
   session id in `session_meta`, `session_head`, active `graph_nodes`
   (`tombstoned = 0`), `pending_turn_inputs`, and `queued_work_batches`. Save the
   normalized result as `03b-sessions-before.json` and require at least one non-zero
   count; an empty baseline is not evidence that cleanup worked.

2. Cancel that subagent card through `POST /api/work/<process_id>/cancel`, as in Phase 2.
   Require `accepted: true` for that exact
   process id and poll until the card is terminal/cancelled with `process.cancel_requested`
   in `process_events` (`03b-subagent-cancel-receipt.json`).
3. Without resetting the session, send a normal follow-up turn in the same session and
   start a **second** subagent — same pinned shape and same bounded single retry as step 1.
4. **Record the durable-catalog delta.** After the cancelled child has settled, query the
   same tables for the same child id and save the normalized result as
   `03b-sessions-after.json`; save the before/after row-count diff as
   `03b-sessions-delta.txt`.

Expect:

- the follow-up turn completes and renders its answer — a wedged parent manager would
  fail or hang instead;
- the second subagent reaches a non-terminal card and then a terminal state. There is no
  per-session turn registry to deny it: the child session turn is initialized and run
  through the ordinary turn path, so its admission and progress are themselves the proof
  that nothing stale blocks new child work after the cancellation;
- **durable-catalog gate (objective):** `03b-sessions-before.json` proves the query saw
  the recorded child session. FIG-3377 changed the aftermath: lash never deletes a
  session because a process was cancelled, so `03b-sessions-after.json` must still show
  the child's `session_meta`/`session_head` rows for that id and must **not** show a
  `deleted_sessions` tombstone for it. What cancellation settles is the turn, not the
  session: the child's `pending_turn_inputs` row is left in a terminal state
  (`completed`/`cancelled`), never open — no claimable input remains for a later turn to
  pick up. The retained child is host-visible: it enumerates under the session catalog's
  `caused_by = process(<process_id>)` filter and may be retired by an explicit host
  session delete, which is the only path that tombstones it. Quote the child id, the
  retained row counts, and the open-input count when scoring — an unquoted "looked
  clean" does not pass;
- a second subagent reaches a non-terminal card after cancellation; any refusal of
  new child work on the grounds that a turn is still running is the FIG-884 regression.

The cancelled subagent's child session is retained durable and reusable: a host can open
it (for example via the `caused_by` catalog listing) and run an ordinary follow-up turn
on it. The gate above still uses a *new* subagent for the parent-session check, since the
parent's wedge was the stale turn registration, not the child's durability. Screenshot
the follow-up answer and the second subagent's terminal card as
`03b-session-still-usable.png`.

## Phase 5 — Teardown and score

Run `just agent-workbench-down <port>` and confirm the workbench is gone, including
on Abort or a blocked prerequisite. Fill every verdict with PASS, FAIL, BLOCKED or
NOT REACHED; never credit an unexecuted gate.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Declared lifetimes | all three VM starts record Until the originating session | | `01-running-store.json`, `01-processes.png` |
| Global cancel | exact id accepted; cancel_requested then cancelled before deletion | | `02-cancel-receipt.json`, `02-operator-cancelled.png`, store events |
| Session scope close | session-bound id receives cancellation and ends cancelled; forbidden marker absent | | `03-after-delete-*.json` |
| Finished-record retention | tombstone commits; finished session-originated rows pruned with counted retention report; observers gone | | `02-before-delete-store.json`, `03-after-delete-*.json`, `03-session-closed.png` |
| Session survives a cancelled background session turn (FIG-884) | follow-up answers and second subagent runs after cancellation | | `03b-subagent-running.png`, `03b-subagent-cancel-receipt.json`, `03b-session-still-usable.png` |
| Cancelled child retained (FIG-3377) | recorded child baseline nonzero; metadata/head retained; no tombstone or open input | | `03b-sessions-before.json`, `03b-sessions-after.json`, `03b-sessions-delta.txt` |
| No break-glass substitution | no out-of-band cancel or kill used as passing evidence | | command log |

**Aggregate:** did each declared lifetime determine cancellation, did finished-record
retention stay separate from cancellation?

_Stop triggers and the Abort/RCA reporting protocol are in [../RULES.md](../RULES.md)._
