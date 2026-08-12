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
2. **Prove execution, not prompt echo.** Before stopping Restate, require the running
   pill, visible **stop turn**, exactly one `/api/state.active_turns` address, an
   `exec_code_started` trace record for that turn, and its `/api/work` entry with
   `lifecycle: "running"`. LLM request trace records echo the prompt and are never gate
   evidence. A turn that already settled is a retry of this phase.
3. **Reconverge before Stop.** After Restate is ready again, reload/poll until the UI and
   `/api/state` show the exact pre-bounce address as running and Stop is visible. Do not
   press a stale button during the engine outage.
4. **Committed cancellation is authoritative.** `POST /api/turn/cancel` must settle as
   `TurnStop::Cancelled` and carry non-empty evidence with `origin: "user"`. The rendered
   `turn stopped · request <id>` must use the same request id.
5. **No Admin API substitution.** Restate Admin cancel/kill is never a passing action.
   Container stop/start creates the fault; Lash's public Stop path settles it.
6. **Pin the cancellation surface.** The baseline declares and starts a process with a
   long sleep, then foreground-awaits its handle. The named turn-scoped variant uses a
   top-level durable sleep with no process declaration, `start`, handle, or process
   `await`. Do not accept whichever shape the model happens to choose; these are
   different cancellation surfaces.

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
  `POST /api/turn/cancel`, `GET /api/work`.
- Disk/trace truth: `<data-dir>/session-id`, `<data-dir>/active-turns.json`, and
  `<data-dir>/trace.jsonl`.
- Teardown: `just agent-workbench-down <port>`.

## Phase 0 — Boot and record identities

Require `OPENROUTER_API_KEY`; a missing key is a harness gap → Abort. Boot and gate the
Workbench and Restate readiness. Record the Workbench PID, Restate container id, rendered
session id, `/api/state.settings.session_id`, and disk `session-id`; require the session
ids to agree. Screenshot `00-ready.png`.

## Phase 1 — Park one exact turn in durable work

Submit a shape-pinned prompt: ask the agent to execute one Lashlang block that declares
and starts a named process whose body sleeps for at least 60 seconds, then
foreground-awaits the returned handle. Explicitly require this **spawned process-await**
shape; ordinary tool work or a top-level turn-scoped sleep does not qualify.

Poll until all of these agree:

- the running pill and **stop turn** are visible;
- `/api/state.active_turns` contains exactly one address for the rendered session;
- `active-turns.json` contains that exact session/turn pair;
- `trace.jsonl` contains an `exec_code_started` record for that exact turn after the
  prompt was submitted; an LLM request or response containing prompt text such as
  `sleep for "300s"` does not count;
- `/api/work` contains that named process with `lifecycle: "running"`.

Record the address and Workbench PID. Screenshot `01-parked-running.png`; save the state
and matching trace/work records as `01-parked-state.json`, `01-durable-trace.json`, and
`01-running-work.json`, including the named process id. If the required process-await
shape was not produced, use the public Stop control, wait at most 10 seconds for its
committed terminal, and retry Phase 1 with a fresh turn. Abort/RCA if two attempts fail
to produce the pinned shape.

### Named variant — turn-scoped suspended sleep

Run the complete scenario a second time with fresh Phase 0 identities and variant-labeled
artifacts. In its Phase 1 prompt, require one Lashlang block whose top level directly
sleeps for at least 60 seconds and then returns a short result. Explicitly forbid a
process declaration, `start`, handle, or process `await`. This variant is admitted only
when the same exact-turn `exec_code_started` gate passes, its recorded block has the
direct top-level sleep shape, `/api/state` and disk still agree on the one running turn,
and `/api/work` contains no spawned process for that turn. Apply the same bounded
two-attempt shape retry. Through Phases 2–3, retain the no-process assertion and require
the turn-scoped sleep itself to settle `Cancelled`.

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
still agree. For the baseline shape, `/api/work` must show the exact Phase 1 process id
still at `lifecycle: "running"`. Screenshot `03-reconverged-running.png`; save the state
and work snapshot as `03-reconverged-state.json` and `03-reconverged-work.json`.

## Phase 3 — Stop the replayed turn

Press **stop turn** while capturing `POST /api/turn/cancel`. Gate:

1. the response is accepted for the exact Phase 1 address;
2. the gate outcome is `requested` or `already_requested`;
3. the terminal is committed as stopped/cancelled with non-empty `request_id`,
   `origin: "user"`, and the Workbench reason;
4. the UI renders `turn stopped · request <id>` with the same id, returns idle, and hides
   Stop;
5. `/api/state.active_turns` and `active-turns.json` clear the address, and the trace
   records the cancellation against the original turn id;
6. for the baseline shape, `/api/work` shows the exact Phase 1 process at terminal
   `Cancelled`, with a `process.cancel_requested` event, before its original sleep
   deadline.

Save `04-cancel-receipt.json`, `04-cancelled-state.json`,
`04-cancelled-work.json`, and screenshot `04-restarted-cancelled.png`.

## Phase 4 — Commit normally after restart

Submit a short turn with a unique literal marker and ask for it in the answer. Prose-only
instructions are not a reliable shape constraint: a model may wrap even this marker
request in a durable process. Poll for either a completed turn or a running `/api/work`
process for at most 60 seconds. If a process appears, do not wait for its declared sleep
deadline: use the public Stop control, require its committed terminal within 10 seconds,
and retry once with a fresh marker. If neither completion nor the unexpected-process
signal appears within 60 seconds, or the second attempt also wraps the marker in a
process, Abort/RCA.

For the successful attempt, require idle and the new user/assistant pair to agree across
the rendered transcript and `/api/state.messages`, with no active address left. Its turn
id must differ from the cancelled turn. Screenshot `05-post-restart-completed.png`; save
state as `05-post-restart-state.json`.

## Phase 5 — FIG-1117 worker-restart lease companion

### Replace the Workbench worker inside the lease TTL

Run this companion after Phase 4, with Restate healthy. It is a separate worker-restart
geometry: do not count the engine bounce above as either arm. Record the configured session
lease TTL and use monotonic timestamps to prove both post-loss commits start before the dead
worker's original lease could expire.

1. Start a shape-pinned long turn, record its exact session/turn address and the Workbench
   PID, and gate a real `exec_code_started` record. Run `just agent-workbench-restart <port>`
   without changing the data directory or Restate. Require the PID and boot incarnation to
   change while the session and turn address remain exact.

   Record the lease identity triple on both sides of the restart:
   `owner_id`, `incarnation_id`, `executor_id`. All three must change across a real process
   replacement — a new boot mints a new incarnation, and each runtime open inside it mints a
   new executor id. This is what makes the replacement worker a *foreign* claimant to the
   dead worker's still-live lease row, which is precisely the geometry both arms below
   exercise. An unchanged incarnation means the restart did not happen; an unchanged
   executor under a changed incarnation means the identity is being derived from something
   process-stable, which is a **FAIL**.
2. **Same-turn successor arm:** allow Restate to redrive that exact turn. Require its user and
   assistant nodes to commit exactly once, the UI/API/store projections to agree, and the
   trace to show the replacement worker observed the still-live holder before the original
   TTL elapsed. The `session_execution_lease.busy` evidence must name the *dead* worker's
   executor as `holder_executor_id` and the replacement's as `claimant_executor_id`: a
   redrive is not reentry, and identical triples there would mean the runtime handed a
   rebuilt open its predecessor's identity. Save `06a-same-turn-{state,store,trace}.json` and report this arm separately.
3. **New-turn-within-TTL arm:** immediately submit a fresh marker turn through the replacement
   worker, while the timestamp is still inside that same original TTL window. Require its
   distinct turn id and ordered user/assistant nodes to commit exactly once, with UI/API/store
   agreement. A busy-lease error is a failure; a typed `HeadRevisionConflict` is acceptable
   only for an actual overlapping writer and must carry complete loser evidence. Save
   `06b-new-turn-{state,store,trace}.json` and report this arm separately.

The deterministic companions are
`same_turn_successor_within_dead_lease_ttl_commits_under_head_cas` and
`new_turn_within_dead_lease_ttl_commits_under_head_cas`. Restoring the former busy-holder
refusal must make the new-turn arm fail; record that RED proof with the run.

## Phase 6 — Teardown and score

Run `just agent-workbench-down <port>` and confirm both the Workbench process and managed
Restate container are gone.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Boot identity | Workbench/Restate ready; rendered/API/disk session ids agree | | `00-ready.png` |
| Durable park | pinned shape has `exec_code_started`; one exact running address agrees across UI, API, disk, trace, and work API | | `01-parked-*`, `01-running-work.json` |
| Engine-only bounce | same container id; Workbench PID and endpoint stay live | | command log, `02-engine-down.png` |
| Outage render | reload during the outage shows the connection state, never `idle` / "no turns yet" | | `02-outage-reload.png` |
| UI reconvergence | exact pre-bounce address restores running pill + Stop | | `03-reconverged-*` |
| Stop after reconnect | committed Cancelled terminal carries matching user evidence; baseline process reaches Cancelled with `process.cancel_requested` | | `04-restarted-cancelled.png`, receipt/state/work JSON |
| Normal post-restart commit | new turn commits and UI/API transcript agree | | `05-post-restart-*` |
| FIG-1117 same-turn successor | replacement boot commits the exact redriven turn inside the dead lease TTL | | `06a-same-turn-*` |
| FIG-1117 new turn inside TTL | replacement boot commits a distinct new turn inside the same dead lease TTL | | `06b-new-turn-*` |
| No break-glass substitution | no Restate Admin cancel/kill used | | command log |

**Aggregate:** after bouncing only the Restate engine, did the unchanged Workbench
reconverge on the original live turn, Stop it with authoritative evidence, and commit new
work normally?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
