# E2E Scenario: Workbench Durable Stop — Exact Turn and Process-Await Cancellation

> **Read [../RULES.md](../RULES.md) first** — especially "The browser surface (example
> apps)": browser tooling, objective gate order, screenshots, real-token designation,
> and teardown ownership. This runbook only adds the scenario-specific parts.


> **Workbench process replacement (FIG-1164, FIG-3035).** The non-destructive
> same-configuration restart is `just agent-workbench-restart <port>`, which keeps the
> application data. It is verified: the phases below execute it, and no step
> of this row is blocked any more. See the
> [central lifecycle constraint](../RULES.md#agent-workbench-lifecycle-constraint-fig-1164);
> never substitute the destructive reset.

**Purpose.** Prove the Agent Workbench Stop control uses Lash's exact-turn,
keyed-promise cancellation primitive end to end. Stop one live turn normally, then start
another, restart only the workbench web process while the durable engine owns the turn, and Stop it
from the reconstructed UI. Finally, exercise Stop while a foreground turn awaits an
independent process and distinguish that cooperative cancellation from session
revocation. Stop must commit `Cancelled` for both the turn and awaited process through the
workbench's explicit operator-cancellation policy. Deleting
the session closes its scope and cancels processes recorded `Until(session)`; a
`Detached` process would survive that close. Finished-record retention is separate.

**Declared lifetimes and operator cancellation.**
[ADR 0108](../../docs/adr/0108-a-process-lives-until-a-scope-its-start-could-reach.md)
is the lifetime contract. In
`examples/agent-workbench/src/main_sections/bootstrap.rs`,
`SessionProcessAdminPluginFactory` uses `lifetime::session_or_starter`: every VM
process this runbook starts through `processes.start` records `Until(session)`.
`DelegationPluginFactory` uses `lifetime::starter`: a foreground `spawn_agent`
records `Until(turn)`. Host-trigger deliveries explicitly use `Until(owner_session)`
in `examples/agent-workbench/src/host_triggers.rs`.

The current host exposes no Detached start. `processes.await` uses
`CancelHint::Ignore` in `crates/lash-plugin-process-controls/src/lib.rs`: dropping
that wait does not itself cancel an Until(session) process. Workbench Stop and Abort
separately call `cancel_processes_parented_by_turn` from `cancel_turn_with_driver`
in `examples/agent-workbench/src/main_sections/turn_cancel.rs`, requesting
cancellation of each live process whose recorded starter is the addressed turn.
This operator policy explains the two cancelled terminals in Phases 3 and 4; the
process's declared lifetime explains its cancellation on session close in Phase 5.
A process started by another turn or another session is outside this Stop policy.

**Why this matters.** A web-process-local token cannot survive this scenario. The workbench
persists only the routing address, the durable engine owns the running turn, and
`TurnWorkDriver` resolves the reserved gate and terminal promises. A successful restart case
therefore proves the Stop path is not secretly process-local.

**FIG-1445 referee linkage.** A shallow Stop issued ~0.3s into process start proves
immediate keyed-promise resolution but says nothing about cancellation deep inside a
durable sleep (where FIG-1445 recorded evidence stopped ~230s deep). This runbook
therefore separates process-await cancellation into two distinct arms: a shallow-dwell
arm (Stop issued immediately upon start) and an explicit deep-dwell arm (refereeing
FIG-1445, where Stop is issued while the process is ≥120s parked inside a durable sleep).
Separate named phases and scorecard gates distinguish shallow-stop latency from deep-stop
latency so a judge cannot credit the deep claim from the shallow arm.

**Real tokens.** Turns use OpenRouter. Their prose and duration are nondeterministic;
gate on the running affordance, cancel receipt, terminal state, and evidence—not text
quality. This runbook is authored for a deliberate token-spending browser run.

## Scenario-specific golden rules

1. **Stop only after the running gate.** The Stop button is visible and `/api/state`
   reports the exact active turn before it is pressed. A fast completion that wins first
   is a retry of that phase, not cancellation evidence.
2. **HTTP terminal is authoritative.** `POST /api/turn/cancel` must return a cancellation
   receipt whose `terminal.stop.cancelled.evidence` is committed and
   contains a non-empty `request_id`, `origin: "user"`, and the workbench `reason`.
3. **UI and receipt agree.** The UI renders `turn stopped · request <id>` using the same
   request id returned in `terminal.stop.cancelled.evidence`; the transcript/API converges on the
   interrupted terminal. Any disagreement is a contract violation → Abort/RCA.
   Note which receipt that is: **a Stop still pending after the escalation window sends two cancels** —
   `mode=stop` immediately, then `mode=abort` after `STOP_ESCALATION_MS` (20 s) — and the
   rendered `turn stopped · request <id>` carries the escalated **abort's** id, not the first
   receipt's. Capture both receipts and compare the rendered id against the escalated one. The
   escalation also puts a hard 20 s floor under every process-await latency gate in this
   runbook: a gate that reads "within 10 seconds of the Stop request" means the request that
   owns the terminal, and a full UI-driven cancel of a process await cannot beat the
   escalation window.
4. **Restart only the web process.** Use `just agent-workbench-restart <port>` (equivalently
   `bash scripts/agent-workbench-dev.sh restart --port <port>`), the verified non-destructive
   same-configuration replacement named in this runbook's FIG-1164/FIG-3035 header. The data
   directory must remain unchanged. Never substitute the destructive reset.
5. **Break-glass is not success.** Never kill the turn out of band to pass a gate. If
   cleanup requires it after an Abort, record that separately; it must not be reported as
   a Lash `Cancelled` terminal.
6. **Session close follows the recorded lifetime.** `DELETE /api/session`
   first cancels that session's foreground turn with the workbench Abort evidence,
   reason `workbench Abort control`, then closes the session scope. Each live
   `Until(session)` process receives `process.cancel_requested` and ends cancelled.
   A `Detached` process receives no scope-close cancel and stays in host-wide
   `/api/work` until it ends. Processes `Until` another live session are untouched.
   Only after the tombstone does `prune_processes_originated_by` reclaim finished
   records the deleted session started. It does not prune live Detached records or
   other originators' work. Missing rail rows alone are not cancellation evidence.
7. **Distinguish shallow vs deep durable-sleep cancellation (FIG-1445).** The shallow arm
   issues Stop immediately after the process starts (~0.3s in). The deep-dwell arm
   mandates that the process is genuinely parked in durable sleep for ≥120s before Stop
   is issued (verified through `/api/work` lifecycle, events, and timestamps, not
   wall-clock harness busy-waiting). Both shallow and deep cancellation must commit
   `Cancelled` promptly (<10s from request) without waiting for the remainder of the
   durable sleep timer. A judge cannot credit the deep claim from the shallow arm.
8. **Late Stop is a no-op.** After the turn leaves the workbench's active-turn index,
   another UI Stop finds no address and reports no accepted cancellation. The separate
   exact-address controller probe must report `completion_won_race` without changing
   affected inputs, disposition, terminal evidence, or active-address state.
9. **Repeated Stop cannot change the accepted policy.** The first cancellation the gate
   accepts owns the undelivered-input disposition for the rest of the turn. While a Stop
   is pending, a repeat that asks for the same disposition is idempotent and reports
   `already_requested` with the first request's evidence; a repeat that asks for a
   different disposition reports `policy_conflict` naming both the requested and the
   accepted policy and changes nothing — not the terminal, not the durable request row,
   not the queued inputs. A timing escalation (`after_step` to `immediate`) is the one
   thing a repeat may change, and it still carries the accepted disposition. Once the
   turn has ended, rule 8 wins over this one: the repeat is a `completion_won_race`
   no-op even when it names a different policy.

## Working material

- Boot with a fresh durable directory through `scripts/agent-workbench-dev.sh`, which since
  FIG-3153 is the live path and honours `AGENT_WORKBENCH_BIN`:
  `AGENT_WORKBENCH_DATA_DIR=<fresh-tmp> AGENT_WORKBENCH_RUN_DIR=<fresh-tmp-run> AGENT_WORKBENCH_OPEN=0 bash scripts/agent-workbench-dev.sh up --port <port>`
  (`just agent-workbench <port>` predates it and does not carry this row's env).
  Gate `GET /healthz` → 200. Teardown:
  `just agent-workbench-down <port>`.
- To judge the same flow on Postgres, add `AGENT_WORKBENCH_POSTGRES=1` to the boot
  command. The dev helper starts a Postgres 18 container on a port derived from
  `<port>`, passes its URL as `AGENT_WORKBENCH_DATABASE_URL`, and records a managed-container
  marker. The persistence contract requires the container to remain unchanged across the
  Phase 2 `agent-workbench-restart` replacement; `agent-workbench-down` removes it. Record the Postgres
  container name and require the startup trace payload's `store_backend` to be
  `"postgres"` before Phase 1.
- Browser affordances: chat composer, **stop turn** button, running/idle pill, transcript.
- Backend truth: `GET /api/state`; `POST /api/turn`; `POST /api/turn/cancel`; `GET /api/work`.
  `/api/state.active_turns` exposes routing addresses so reload can restore the Stop
  affordance. The cancel response contains `accepted` and `cancellations[]`, each with
  `address`, gate `outcome`, and authoritative `terminal`. The cancel route is
  `POST /api/turn/cancel`.
- Disk evidence: `<data-dir>/session-id` and `<data-dir>/active-turns.json` retain routing
  state across the web-process restart; `trace.jsonl` records
  `agent_workbench.turn.cancel_requested` with the same evidence.

## Phase 0 — Boot and pre-flight

Require `OPENROUTER_API_KEY`; a missing key is a harness gap → Abort. Boot the workbench,
gate `/healthz`, open the browser, and confirm `/api/state.settings.session_id` matches the
rendered session id. Screenshot `00-ready.png`.

## Phase 1 — Stop a live turn without restart

Submit a task likely to expose a long-running window (for example, ask for a researched
comparison that uses web search). Gate on the running pill and visible **stop turn**
button, then poll `/api/state` until `active_turns` contains exactly one address for the
rendered session.

Press **stop turn** while capturing the `POST /api/turn/cancel` response. Gates:

1. response `accepted` is true and has one cancellation for the active address;
2. its gate outcome is `requested` or `already_requested`;
3. the committed terminal attaches to this Stop's request evidence, encodes
   `TurnStop::Cancelled`, and carries evidence per golden rule 2. The terminal need not ride
   on *this* response: the first receipt of an `after_step` Stop on a turn whose step outlasts
   the request answers `cancellation_recorded_terminal_pending`, and the terminal attaches to a
   later receipt for the same request id. Follow the request id, not the response body;
4. the UI renders `turn stopped · request <id>` with the same id, returns to idle, and
   hides Stop;
5. `GET /api/state` no longer lists that address and `/api/state.transcript` agrees with
   the rendered interrupted terminal.

Screenshot `01-cancelled.png`; save the cancel response as `01-cancel-receipt.json`.
After `/api/state` no longer lists the address, another workbench Stop must return
`accepted:false` with an empty `cancellations` list. This session-level route cannot
address an inactive turn.

## Phase 2 — Restart the web process mid-turn, then Stop

Submit another long-running turn. Gate on Stop plus one `/api/state.active_turns` entry
and record its session/turn ids. Then run `just agent-workbench-restart <port>` — the
non-destructive same-configuration replacement of golden rule 4 — poll `/healthz` until the replacement process is ready, reload the page,
and gate all of the following before pressing Stop:

- the rendered session id is unchanged;
- `/api/state.active_turns` contains the exact pre-restart address;
- the running pill reads restored/running and **stop turn** is visible;
- `<data-dir>/active-turns.json` contains the same address.

Screenshot `02-restored-running.png`. Press Stop and repeat every receipt/UI/API gate from
Phase 1. Additionally require the terminal evidence request id to be new and the trace to
show the recovered request against the pre-restart turn id. Screenshot
`03-restored-cancelled.png`; save `03-cancel-receipt.json`.

## Phase 3 — Shallow-dwell Stop over process await (shallow arm)

Submit a prompt that makes the agent declare and start a process with a long sleep (e.g.
`sleep for "300s"`), then foreground-await its handle. This phase is valid only if
`/api/state` shows one active turn and `/api/work` shows that named process as
non-terminal (`lifecycle.state` is `running` or `waiting`) before Stop. If the model does not produce that
shape, retry the phase; ordinary model or tool work is not process-await evidence.

Issue Stop immediately once the process-await is running — the shallow dwell gate is
**< 5 s elapsed**, which an immediate press satisfies with room to spare; that single number
is what the scorecard scores — while capturing the `POST /api/turn/cancel` response. Repeat the Phase 1 receipt
gates and gate shallow-stop latency:

1. the turn terminal is committed `Cancelled`, with the Stop request evidence;
2. the named process reaches terminal `Cancelled` in `/api/work` within 10 seconds of
   the Stop request;
3. that process's events include `process.cancel_requested`;
4. neither terminal waits for the process's original sleep deadline;
5. pre-Stop dwell was shallow (< 5 s elapsed) — the same number as the scorecard row.

Save the pre-Stop and terminal `/api/work` responses as
`04-process-await-running.json` and `05-process-await-cancelled.json`. Screenshot the
committed turn terminal as `05-process-await-cancelled.png`.

## Phase 4 — Deep-dwell Stop over durable sleep (referees FIG-1445)

Submit a prompt that makes the agent declare and start a named process with a durable
sleep of at least 300 seconds, then foreground-await its handle. Require this
spawned process-await shape.

Poll `/api/state` and `/api/work` until:
- `/api/state.active_turns` contains exactly one active address;
- `/api/work` contains the named process with `lifecycle.state` is `running` or `waiting`;
- the process is genuinely parked inside its durable sleep.

**Gate the deep dwell before pressing Stop.** The process must genuinely remain parked in
durable sleep for ≥120 seconds. Poll `/api/work` and the workbench trace to verify that
the process has been continuously running in its durable sleep state for at least 120
seconds since its start/sleep entry, while the foreground turn remains active in await.
Do not busy-wait in the harness; confirm genuine durable parking from `/api/work`
timestamps/events and trace evidence.

Capture the parked pre-Stop state and `/api/work` response as `06a-deep-dwell-running.json`
once the ≥120s dwell requirement is satisfied.

Press **stop turn** while capturing the `POST /api/turn/cancel` response. Gate deep-stop
latency:

1. response `accepted` is true for the active address;
2. the turn terminal is committed `Cancelled`, carrying matching Stop request evidence;
3. the deeply parked process reaches terminal `Cancelled` in `/api/work` within 10
   seconds of the Stop request;
4. that process's events include `process.cancel_requested`;
5. neither terminal waits for the remaining duration of the original sleep timer (e.g.
   the remaining ~180s+ of the 300s timer);
6. the recorded pre-Stop dwell is confirmed ≥120s from `/api/work` process start/event
   timestamps, proving the cancellation was refereed against a deep durable sleep.

Save `06b-deep-dwell-cancelled.json` and screenshot `06b-deep-dwell-cancelled.png`.

## Phase 5 — Session deletion cancels Until(session), then reclaims finished records

This phase uses two sessions to distinguish their scope lifetimes. Neither process
is Detached: both are VM definitions started through `processes.start` with the
workbench's `lifetime::session_or_starter` policy. Record each stored lifetime scope
before deleting anything.

1. In a second session, start a named VM process with a bounded wait and success
   marker, without foreground-awaiting it. Record its session id, process id and
   `Until(second-session)` lifetime. This is the bystander.
2. In the first session, start a named long-sleep VM process and foreground-await
   its handle. Record the session id, turn id, process id and `Until(first-session)`
   lifetime. Require both processes non-terminal before proceeding.
3. Arm API/trace/store observation before the delete, since finished records can
   be pruned before the delete response. Call
   `DELETE /api/session?session_id=<first-session-id>` instead of Stop.

Gate the split:

- the first session's turn commits `Cancelled` carrying the delete's Abort evidence,
  reason `workbench Abort control`, with no stranded active address;
- its process receives `process.cancel_requested` from the session scope close and
  commits `Cancelled` before deletion completes. Capture both facts before pruning;
- the tombstone commits, observer edges are removed, and the workbench's retention
  pass prunes finished first-session-originated records. Quote the
  `api.session.delete.deleted` trace's `process_retention.pruned_processes`; absence
  from `/api/work` by itself cannot prove which lifetime or outcome the process had;
- the second session stays live. Its `Until(second-session)` process receives no
  scope-close cancel, stays in host-wide `GET /api/work` without `session_id`, and
  reaches its expected success marker. Capture the terminal in the rail's 10-second
  visibility window and retain its durable completion evidence.

Save `07-revoked-process-running.json` (both live ids and recorded lifetimes),
`08-revoked-process-settled.json` (first process cancellation and retention;
second process still live, then completed), the delete response, and trace/store
extracts. Screenshot `08-session-scope-split.png`. A cancelled bystander, missing
first-process cancellation evidence, successful first process after close, or a
stranded first turn is Abort/RCA. Detached survival is covered by the deterministic companion laws below; the
workbench exposes no Detached start and this judged phase makes no such claim.

## Companion laws — existing full test paths

Run each once on SQLite, with executed counts, sharing receipts with process-lifecycle
when judging both runbooks together:

```sh
. ./env.sh
kiln test //crates/lash-core:lash-core__unit_test --test_arg=runtime::durable::scope_end_tests::a_session_close_cut_at_any_step_resumes_there_and_deletes_nothing_while_a_process_lives
kiln test //crates/lash-durable-test:core_node_processes__test --test_arg=process_endstates::a_detached_process_outlives_its_deleted_session_and_accepts_host_cancellation::on_sqlite_memory
kiln test //examples/agent-workbench:agent-workbench__unit_test --test_arg=process_work_tests::session_delete_reclaims_the_deleted_sessions_terminal_work
kiln test //examples/agent-workbench:agent-workbench__unit_test --test_arg=process_work_tests::work_rail_keeps_a_nonterminal_process_past_the_retirement_window
kiln test //examples/agent-workbench:agent-workbench__unit_test --test_arg=turn_control_timeout_tests::stop_over_real_process_await_commits_cancelled_terminal
kiln test //crates/lash-sqlite-store:lash-sqlite-store__unit_test --test_arg=durable::tests::a_turn_cancel_is_a_first_winner_row_with_a_wake::memory
```

These pin scope-close cancellation/settlement, Detached survival, finished-record
retention, host rail visibility, explicit operator cancellation over a VM process
await, and first-winner cancellation policy, respectively.
They cannot establish a browser start surface that the workbench does not expose.

## Phase 6 — Teardown and score

Run `just agent-workbench-down <port>` and confirm the workbench process is gone.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Boot | `/healthz` 200; rendered/API session ids agree | | `00-ready.png` |
| Normal Stop | committed Cancelled terminal + evidence | | `01-cancelled.png`, `01-cancel-receipt.json` |
| Repeated Stop policy (rule 9) | matching repeat `already_requested`; conflicting repeat `policy_conflict` naming both policies; durable request row, intent revision and queued inputs unchanged | | `durable::tests::a_turn_cancel_is_a_first_winner_row_with_a_wake::memory` (`crates/lash-sqlite-store/src/durable_tests.rs`) |
| Routing persistence | same session/turn address before and after restart | | `02-restored-running.png`, state files/API |
| Restored Stop affordance | running pill + Stop restored from `/api/state.active_turns` | | `02-restored-running.png` |
| Post-restart Stop | committed Cancelled terminal + evidence for original address | | `03-restored-cancelled.png`, `03-cancel-receipt.json` |
| Shallow-stop latency (process await) | pre-Stop dwell < 5s (press Stop as soon as the await is running); turn and awaited process both commit Cancelled within 10s of the request that owns the terminal, and before the original deadline | | `04-process-await-running.json`, `05-process-await-cancelled.json`, `05-process-await-cancelled.png` |
| Deep-stop latency (FIG-1445 durable sleep ≥120s) | Stop issued while process is ≥120s parked in durable sleep; turn and process commit Cancelled within 10s of Stop request; durable dwell ≥120s recorded in work events/trace | | `06a-deep-dwell-running.json`, `06b-deep-dwell-cancelled.json`, `06b-deep-dwell-cancelled.png` |
| Revoked turn settlement | the deleted session's turn commits `Cancelled` with the delete evidence, reason `workbench Abort control` | | delete response + terminal evidence + trace |
| Declared lifetime and retention | first Until(session) process gets cancel_requested then Cancelled before finished-record pruning; second Until(other-session) process stays live and completes | | `07-revoked-process-running.json`, `08-revoked-process-settled.json`, `08-session-scope-split.png`, delete trace/store |
| UI/API agreement | rendered request ids equal terminal evidence ids; active addresses clear | | screenshots + receipts + `/api/state` |
| No break-glass substitution | no Admin cancel/kill used as a passing action | | command log |

**Aggregate:** did exact cooperative cancellation produce authoritative evidence
normally, after reconstructing the entire web process, under shallow process await, and
deep inside durable sleep (refereeing FIG-1445); did a repeated request preserve the
first accepted policy and report a conflict instead of silently substituting it; and did
session deletion close the declared Until(session) lifetime before finished-record
retention, while another live session's process remained untouched? Record the
explicit operator-cancellation requests separately from the session-scope close.

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
