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
   retained in `<data-dir>/processes.db`, after its originating session store is gone.
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
  `process_handle_grants`; `<data-dir>/lash-sessions/`; `<data-dir>/trace.jsonl` event
  `agent_workbench.reset.restate.session_deleted`, whose report names orphaned process
  ids.

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
contains both ids and handle grants for the owner session. Save `01-running-work.json`
and the relevant database extraction as `01-running-store.json`; screenshot
`01-two-running-processes.png`.

## Phase 2 — Delete the originating session

Use the reset/new-session control while capturing its HTTP response, or issue
`DELETE /api/session` from the browser context. Poll until:

- the rendered and `/api/state` session id changes;
- the old session's database is absent from `<data-dir>/lash-sessions/`;
- the session-deleted trace report contains both original process ids as orphaned;
- `process_handle_grants` has no grants for the deleted session;
- `/api/work` and the rendered work rail still show both original process ids.

The production delete path first revokes the old session's active durable waits and
allows those turns to settle as typed deleted-session refusals; only then does it remove
the session store. This ordering lets a suspended turn replay its existing journal while
ensuring revocation never becomes process-cancellation authority.

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
| Owner deleted | new rendered/API session id; old store/grants gone | | `02-owner-gone-processes-live.png`, `02-after-delete-*.json` |
| Runtime independence | both original ids remain live in rail and `/api/work` after delete | | `02-owner-gone-processes-live.png`, API/trace report |
| Global cancel | exact id accepted; `cancel_requested` then cancelled | | `03-orphan-cancelled.png`, `03-cancel-receipt.json`, store events |
| Survivor completion | completed terminal and finish marker persist after owner deletion | | `04-survivor-completed.png`, `04-terminal-*.json` |
| No break-glass substitution | no Restate Admin cancel/kill used | | command log |

**Aggregate:** did the workbench visibly and durably preserve Runtime Process ownership at
the runtime layer after deleting its originating session, including both natural
completion and cooperative cancellation?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
