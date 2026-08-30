# E2E Scenario: Workbench Runtime Processes — Session-Independent Lifecycle

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface,
> screenshot, polling, real-token, Abort/RCA, and teardown rules. This runbook adds only
> the Runtime Process lifecycle scenario.

**Purpose.** Prove the ADR 0019 boundary through the Agent Workbench: a Runtime Process
is runtime-owned, survives deletion of the session that started it, remains visible in
the host work rail, and persists its terminal state. Also prove the rail's Cancel control
records `process.cancel_requested` and settles a second process as cancelled through the
global work API.

**Real tokens.** The setup turn uses OpenRouter to define and start processes. Gate on
named process cards, process ids, durable lifecycle/event rows, and API/UI agreement—not
on surrounding model prose.

## Scenario-specific golden rules

1. **Both processes must be running first.** Do not delete the session until `/api/work`
   and the rendered rail show distinct `FIG425_survivor_<runid>` and
   `FIG425_cancellable_<runid>` process cards with non-terminal status.
2. **Delete the owner, not the runtime.** Use the workbench reset affordance (the API
   operation is `DELETE /api/session`; legacy `POST /api/reset` drives the same path).
   Never stop Restate, the process worker, or the web process during this scenario.
3. **The process rail is runtime-wide.** After deletion, the rendered session id must
   change while both original process ids remain visible through `/api/work`. A process
   visible only in a stale screenshot does not pass.
4. **Completion is durable.** The survivor must reach `completed`, with its terminal event
   retained in `<data-dir>/processes.db`, after its originating session has been retired
   from the shared durable catalog.
5. **Cancel is cooperative and evidenced.** Use the cancellable card's **cancel** button.
   Require a `process.cancel_requested` event followed by a `cancelled` terminal for that
   exact process id. Killing a Restate invocation is not a substitute.

## Working material

- Boot with a fresh durable directory:
  `AGENT_WORKBENCH_DATA_DIR=<fresh-tmp> AGENT_WORKBENCH_OPEN=0 just agent-workbench <port>`.
  Gate `GET /healthz` → 200. The entire Restate stack is port-isolated by default: the
  helper derives its endpoint, ingress, admin port, node port, and container name from
  `<port>`, so concurrent runs on distinct workbench ports do not need manual Restate
  overrides. Teardown:
  `just agent-workbench-down <port>`.
- Browser affordances: chat composer, work rail, per-process **cancel**, reset/new-session
  control, rendered session id.
- Backend truth: `GET /api/state`, `GET /api/work`, `POST
  /api/work/{process_id}/cancel`, and `DELETE /api/session` (or the reset control's
  equivalent `POST /api/reset`).
- Disk truth: `<data-dir>/processes.db` tables `processes`, `process_events`, and
  `process_observers`; the shared SQLite session catalog at
  `<data-dir>/lash-sessions/durable-core.db`; and `<data-dir>/trace.jsonl` event
  `agent_workbench.reset.restate.session_deleted`, whose report includes the removed
  observer count.

## Phase 0 — Boot and record owner identity

Require `OPENROUTER_API_KEY`; a missing key is a harness gap → Abort. Boot, poll
`/healthz`, open the browser, and require the rendered session id to match
`/api/state.settings.session_id` and `<data-dir>/session-id`. Record it as the owner
session id. Screenshot `00-ready.png`.

## Phase 1 — Start two durable Runtime Processes

Ask the agent to define and start two explicitly named Lashlang Runtime Processes in one
turn. Process names are Lashlang identifiers, so use underscores; the work rail renders
the definition name verbatim:

- `FIG425_survivor_<runid>` waits roughly 4 minutes, then finishes successfully with a
  literal terminal marker. The longer wait leaves enough time for browser-paced evidence
  collection and owner deletion before it completes;
- `FIG425_cancellable_<runid>` waits several minutes by looping over 2-second sleeps,
  then finishes with a marker that must never be reached. Do not use one multi-minute
  sleep: Restate-suspended sleep observes cooperative cancellation only when the workflow
  is re-invoked. Re-invocation after each short sleep therefore settles cancellation in
  roughly 2 seconds instead of waiting minutes for one suspension to end.

Use this wait shape inside the cancellable definition (with its forbidden marker after
the loop):

```lashlang
elapsed_seconds = 0
while elapsed_seconds < 240 {
  sleep for "2s"
  elapsed_seconds = elapsed_seconds + 2
}
```

Poll `/api/work` until both named rows are non-terminal, capture their full process ids,
and require matching running cards in the rendered work rail. Verify `processes.db`
contains both ids and observer rows for the owner session. Save `01-running-work.json`
and the relevant database extraction as `01-running-store.json`; screenshot
`01-two-running-processes.png`.

## Phase 2 — Delete the originating session

Use the reset/new-session control while capturing its HTTP response, or issue
`DELETE /api/session` from the browser context. Poll until:

- the rendered and `/api/state` session id changes;
- `<data-dir>/lash-sessions/durable-core.db` remains in place, and a query for the old
  session id finds no `session_meta` or `session_head` row and exactly one
  `deleted_sessions` tombstone;
- the session-deleted trace report reports two removed observers;
- `process_observers` has no rows for the deleted session;
- `/api/work` and the rendered work rail still show both original process ids.

The production delete path first revokes the old session's active durable waits and
allows those turns to settle as typed deleted-session refusals; only then does it retire
the old session's catalog rows. This ordering lets a suspended turn replay its existing
journal while ensuring revocation never becomes process-cancellation authority.

The last item is the judged crown-jewel checkpoint: screenshot the new session identity
and still-live process cards together as `02-owner-gone-processes-live.png`. Save the API,
trace, and database extracts as `02-after-delete-*.json`.

## Phase 3 — Cancel one orphan through the work rail

Press **cancel** on `FIG425_cancellable_<runid>` and capture the response as
`03-cancel-receipt.json`; require `accepted: true` and the exact process id. Poll
`/api/work` until that card is terminal/cancelled and its event tail includes
`process.cancel_requested`. Require the same ordered evidence in `process_events` and
require that the forbidden finish marker is absent. Screenshot
`03-orphan-cancelled.png`.

## Phase 3b — Cancel a background session turn and keep working (FIG-884)

Cancelling a *background session turn* — a subagent process, whose input is a session turn
rather than a Lashlang definition — used to wedge the session that started it: the child
turn's registration was only removed after the child await, so a cancelled process left a
permanent "running turn" behind. The session then refused to close the child and rejected
every later turn on it. This phase proves the registration is released by cancellation.

Do:

1. **Record the durable-catalog baseline first.** In
   `<data-dir>/lash-sessions/durable-core.db`, query row counts for the recorded child
   session id in `session_meta`, `session_head`, active `graph_nodes`
   (`tombstoned = 0`), `pending_turn_inputs`, and `queued_work_batches`. Save the
   normalized result as `03b-sessions-before.json` and require at least one non-zero
   count; an empty baseline is not evidence that cleanup worked.
2. In the current session, ask the agent to start a **subagent** that keeps working for
   several minutes (a long-running research/loop prompt), and let it reach a non-terminal
   card in the work rail. Capture its process id **and its child session id** as
   `03b-subagent-running.json` — the child session id is the subagent's
   `child_session_id` in `/api/work` (equivalently the `session_id` on the subagent's
   process row in `processes.db`); record it verbatim, the disk gate names it. Screenshot
   `03b-subagent-running.png`.
3. Press **cancel** on that subagent card. Require `accepted: true` for that exact
   process id and poll until the card is terminal/cancelled with `process.cancel_requested`
   in its event tail (`03b-subagent-cancel-receipt.json`).
4. Without resetting the session, send a normal follow-up turn in the same session and
   start a **second** subagent.
5. **Record the durable-catalog delta.** After the cancelled child has settled, query the
   same tables for the same child id and save the normalized result as
   `03b-sessions-after.json`; save the before/after row-count diff as
   `03b-sessions-delta.txt`.

Expect:

- the follow-up turn completes and renders its answer — a wedged parent manager would
  fail or hang instead;
- the second subagent reaches a non-terminal card and then a terminal state, proving the
  parent's managed-turn registry admitted new child work after the cancellation;
- **durable-catalog gate (objective):** `03b-sessions-before.json` proves the query saw
  the recorded child session, and `03b-sessions-after.json` has zero rows for that id in
  all five listed tables. Reclamation uses the canonical session-delete transaction, so
  the runtime-internal child id also joins `deleted_sessions` and cannot be recreated on
  replay. The tombstone is identity evidence, not leaked live catalog state. Quote the
  child id and the before/after counts when scoring — an unquoted "looked clean" does not
  pass;
- capture the workbench logs with debug-level records enabled: `managed_turn.admission`
  and `managed_turn.release` are emitted by `tracing::debug!`, so an info-only log does
  not prove their absence. The records are log evidence, not a guaranteed event in the
  default `trace.jsonl` sink; if debug logging was not captured, mark this gate
  unverifiable and Abort rather than infer from silence. Then require
  `managed_turn.admission` with `outcome=admitted` for the second
  subagent and `managed_turn.release` with `outcome=released, reason=dropped` for the
  cancelled one. A `managed_turn.admission` with `outcome=denied` and reason
  "already has a running turn" after the cancellation is the exact FIG-884 regression.

Known residual (FIG-872, out of scope here): the cancelled subagent's **own child
session** cannot run further turns — the dropped turn future also loses that child
runtime's session loan. The gate above therefore uses a *new* subagent, not a second turn
on the cancelled child. Screenshot the follow-up answer and the second subagent's terminal
card as `03b-session-still-usable.png`.

## Phase 4 — Let the survivor complete

Without opening or recreating the deleted session, poll until `FIG425_survivor_<runid>`
is terminal/completed in `/api/work` and in the rendered rail. Require its terminal
success event and literal finish marker in `processes.db`; re-query after one more work
refresh to prove the terminal is retained rather than transient. Screenshot
`04-survivor-completed.png`; save `04-terminal-work.json` and
`04-terminal-store.json`.

## Phase 5 — Teardown and score

Run `just agent-workbench-down <port>` and confirm the workbench and its Restate
container are gone.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Processes started | two named non-terminal ids agree in rail, API, and store | | `01-two-running-processes.png`, `01-running-*.json` |
| Owner deleted | new rendered/API session id; old session has no live catalog rows, its host-facing tombstone is present, and observer rows are gone | | `02-owner-gone-processes-live.png`, `02-after-delete-*.json` |
| Runtime independence | both original ids remain live in rail and `/api/work` after delete | | `02-owner-gone-processes-live.png`, API/trace report |
| Global cancel | exact id accepted; `cancel_requested` then cancelled | | `03-orphan-cancelled.png`, `03-cancel-receipt.json`, store events |
| Session survives a cancelled background session turn (FIG-884) | after cancelling a subagent, a follow-up turn answers and a second subagent runs; `managed_turn.release` released, no `already has a running turn` denial | | `03b-subagent-running.png`, `03b-subagent-cancel-receipt.json`, `03b-session-still-usable.png`, trace |
| Cancelled child session left no durable-catalog rows (FIG-884) | `03b-sessions-before.json` has a non-zero baseline for the recorded child id; `03b-sessions-after.json` has zero rows for that id in all five cleanup tables | | `03b-subagent-running.json` (child session id), `03b-sessions-before.json`, `03b-sessions-after.json`, `03b-sessions-delta.txt` |
| Survivor completion | completed terminal and finish marker persist after owner deletion | | `04-survivor-completed.png`, `04-terminal-*.json` |
| No break-glass substitution | no Restate Admin cancel/kill used | | command log |

**Aggregate:** did the workbench visibly and durably preserve Runtime Process ownership at
the runtime layer after deleting its originating session, including both natural
completion and cooperative cancellation?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
