# E2E Scenario: Workbench Engine Restart — Stop After Restate Reconnect

> **Read [../RULES.md](../RULES.md) first** — especially browser-surface gates,
> named screenshots, polling, real-token use, Abort/RCA, and teardown ownership. This
> runbook adds only the engine-restart scenario.

**Purpose.** Prove a running Workbench turn reconverges after the Restate engine
container itself is stopped and started while the web process and endpoint worker remain
alive. After reconvergence, the restored Stop control must cancel the original exact
turn and render the same committed `Cancelled` terminal evidence returned by the API. A
subsequent turn must commit normally through the restarted engine.

**Deterministic companion.** `just restate-postgres-workers-e2e` parks a turn in durable
work, stops/starts only Restate, requires the journaled start-gate command path to replay,
cancels that replayed turn with exact evidence, and commits a fresh post-restart turn.
This browser run judges UI reconvergence and the human Stop affordance; it does not
re-implement those scripted assertions.

**Real tokens.** Turns use OpenRouter. Gate on addresses, engine/process identity,
cancellation receipts, committed state, and UI/API agreement—not model prose.

## Scenario-specific golden rules

1. **Bounce only Restate.** Stop and start the same managed Restate container. The
   Workbench PID, data directory, Restate endpoint worker, session id, and active turn
   address must stay unchanged. Removing/recreating the container or restarting the web
   process invalidates this geometry.
2. **Prove the turn is durably parked first.** Before stopping Restate, require the
   running pill, visible **stop turn**, exactly one `/api/state.active_turns` address, and
   a trace event showing the turn entered durable work. A turn that already settled is a
   retry of this phase.
3. **Reconverge before Stop.** After Restate is ready again, reload/poll until the UI and
   `/api/state` show the exact pre-bounce address as running and Stop is visible. Do not
   press a stale button during the engine outage.
4. **Committed cancellation is authoritative.** `POST /api/turn/cancel` must settle as
   `TurnStop::Cancelled` and carry non-empty evidence with `origin: "user"`. The rendered
   `turn stopped · request <id>` must use the same request id.
5. **No Admin API substitution.** Restate Admin cancel/kill is never a passing action.
   Container stop/start creates the fault; Lash's public Stop path settles it.

## Working material

- Boot a fresh port-isolated stack:
  `AGENT_WORKBENCH_DATA_DIR=<fresh-tmp> AGENT_WORKBENCH_OPEN=0 just agent-workbench <port>`.
  Gate `GET /healthz` → 200. The default managed engine container is
  `lash-agent-workbench-dev-restate-<port>`; if
  `AGENT_WORKBENCH_RESTATE_CONTAINER` is set, record and use that exact value.
- Bounce without replacement:
  `docker stop <restate-container>` then `docker start <restate-container>`. Gate the
  container state after each command and poll the port-isolated Restate admin/ingress
  endpoints until ready after start.
- UI: session id, idle/running pill, transcript, composer, **stop turn**.
- HTTP truth: `GET /healthz`, `GET /api/state`, `POST /api/turn`,
  `POST /api/turn/cancel`.
- Disk/trace truth: `<data-dir>/session-id`, `<data-dir>/active-turns.json`, and
  `<data-dir>/trace.jsonl`.
- Teardown: `just agent-workbench-down <port>`.

## Phase 0 — Boot and record identities

Require `OPENROUTER_API_KEY`; a missing key is a harness gap → Abort. Boot and gate the
Workbench and Restate readiness. Record the Workbench PID, Restate container id, rendered
session id, `/api/state.settings.session_id`, and disk `session-id`; require the session
ids to agree. Screenshot `00-ready.png`.

## Phase 1 — Park one exact turn in durable work

Submit a turn explicitly asking for a Lashlang durable sleep of at least 60 seconds
before returning a short result. Poll until all of these agree:

- the running pill and **stop turn** are visible;
- `/api/state.active_turns` contains exactly one address for the rendered session;
- `active-turns.json` contains that exact session/turn pair;
- `trace.jsonl` shows the same turn entered the sleep/durable-effect path.

Record the address and Workbench PID. Screenshot `01-parked-running.png`; save the state
and matching trace records as `01-parked-state.json` and `01-durable-trace.json`.

## Phase 2 — Stop/start the Restate engine only

Run `docker stop <restate-container>` and gate that the container is exited. While it is
stopped, require the Workbench PID and `/healthz` remain live and the disk active-turn
address remains unchanged. Capture `02-engine-down.png` only if the page remains
renderable; a transient browser fetch failure during the outage is evidence to record,
not permission to continue without the post-start gates.

**Reload once during the outage and gate the outage render.** The parked turn is durable
truth; the shell must not contradict it. Require that the reloaded page shows the
connection banner or a connecting/unavailable/reconnecting pill, and that it renders
**neither** an `idle` pill **nor** "no turns yet". A render that is indistinguishable from
an empty, idle session is a failure of this phase even though the engine is down.
Screenshot `02-outage-reload.png`.

Run `docker start <restate-container>`. Poll—not sleep—until its admin and ingress ports
are ready. Require the container id, Workbench PID, session id, and endpoint-worker
address are unchanged from Phase 0/1.

Reload and poll until the page reconverges on the running pill and **stop turn**, and
`/api/state.active_turns` contains the exact Phase 1 address. `active-turns.json` must
still agree. Screenshot `03-reconverged-running.png`; save the state as
`03-reconverged-state.json`.

## Phase 3 — Stop the replayed turn

Press **stop turn** while capturing `POST /api/turn/cancel`. Gate:

1. the response is accepted for the exact Phase 1 address;
2. the gate outcome is `requested` or `already_requested`;
3. the terminal is committed as stopped/cancelled with non-empty `request_id`,
   `origin: "user"`, and the Workbench reason;
4. the UI renders `turn stopped · request <id>` with the same id, returns idle, and hides
   Stop;
5. `/api/state.active_turns` and `active-turns.json` clear the address, and the trace
   records the cancellation against the original turn id.

Save `04-cancel-receipt.json`, `04-cancelled-state.json`, and screenshot
`04-restarted-cancelled.png`.

## Phase 4 — Commit normally after restart

Submit a short turn with a unique literal marker and ask for it in the answer. Poll until
idle and require the new user/assistant pair to agree across the rendered transcript and
`/api/state.messages`, with no active address left. Its turn id must differ from the
cancelled turn. Screenshot `05-post-restart-completed.png`; save state as
`05-post-restart-state.json`.

## Phase 5 — Teardown and score

Run `just agent-workbench-down <port>` and confirm both the Workbench process and managed
Restate container are gone.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Boot identity | Workbench/Restate ready; rendered/API/disk session ids agree | | `00-ready.png` |
| Durable park | one exact address agrees across UI, API, disk, and trace | | `01-parked-*` |
| Engine-only bounce | same container id; Workbench PID and endpoint stay live | | command log, `02-engine-down.png` |
| Outage render | reload during the outage shows the connection state, never `idle` / "no turns yet" | | `02-outage-reload.png` |
| UI reconvergence | exact pre-bounce address restores running pill + Stop | | `03-reconverged-*` |
| Stop after reconnect | committed Cancelled terminal carries matching user evidence | | `04-restarted-cancelled.png`, receipt/state JSON |
| Normal post-restart commit | new turn commits and UI/API transcript agree | | `05-post-restart-*` |
| No break-glass substitution | no Restate Admin cancel/kill used | | command log |

**Aggregate:** after bouncing only the Restate engine, did the unchanged Workbench
reconverge on the original live turn, Stop it with authoritative evidence, and commit new
work normally?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
