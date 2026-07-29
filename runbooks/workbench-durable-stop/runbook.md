# E2E Scenario: Workbench Durable Stop — Exact Turn and Process-Await Cancellation

> **Read [../RULES.md](../RULES.md) first** — especially "The browser surface (example
> apps)": browser tooling, objective gate order, screenshots, real-token designation,
> and teardown ownership. This runbook only adds the scenario-specific parts.

**Purpose.** Prove the Agent Workbench Stop control uses Lash's exact-turn,
keyed-promise cancellation primitive end to end. Stop one live turn normally, then start
another, restart only the workbench web process while Restate owns the turn, and Stop it
from the reconstructed UI. Finally, exercise Stop while a foreground turn awaits an
independent process and distinguish that cooperative cancellation from session
revocation. Stop must commit `Cancelled` for both the turn and awaited process; deleting
the session must unwind the dead turn as `SessionDeleted` without cancelling the process.

**Why this matters.** A web-process-local token or tracked Restate invocation id cannot
survive this scenario. The workbench persists only the routing address, Restate owns the
running turn, and `TurnWorkDriver` resolves the reserved gate and terminal promises. A
successful restart case therefore proves the Stop path is not secretly process-local or
using the Restate Admin API.

**Real tokens.** Turns use OpenRouter. Their prose and duration are nondeterministic;
gate on the running affordance, cancel receipt, terminal state, and evidence—not text
quality. This runbook is authored for a deliberate token-spending browser run.

## Scenario-specific golden rules

1. **Stop only after the running gate.** The Stop button is visible and `/api/state`
   reports the exact active turn before it is pressed. A fast completion that wins first
   is a retry of that phase, not cancellation evidence.
2. **HTTP terminal is authoritative.** `POST /api/turn/cancel` must return a cancellation
   receipt whose terminal is committed as stopped/cancelled and whose `cancellation`
   contains a non-empty `request_id`, `origin: "user"`, and the workbench `reason`.
3. **UI and receipt agree.** The UI renders `turn stopped · request <id>` using the same
   request id returned in `terminal.cancellation`; the transcript/API converges on the
   interrupted terminal. Any disagreement is a contract violation → Abort/RCA.
4. **Restart only the web process.** Use `just agent-workbench-restart <port>`, which
   preserves the data directory and Restate container. Tearing down Restate invalidates
   the durability proof.
5. **Break-glass is not success.** Never use Restate Admin cancel/kill to pass a gate. If
   cleanup requires it after an Abort, record that separately; it must not be reported as
   a Lash `Cancelled` terminal.
6. **Revocation is not Stop.** `DELETE /api/session` revokes the old turn's cancellation
   registration because the session no longer exists. It must not emit
   `process.cancel_requested`; the independent process remains visible in `/api/work`
   until its own terminal.

## Working material

- Boot with a fresh durable directory:
  `AGENT_WORKBENCH_DATA_DIR=<fresh-tmp> AGENT_WORKBENCH_OPEN=0 just agent-workbench <port>`.
  Gate `GET /healthz` → 200. The entire Restate stack is port-isolated by default: the
  helper derives its endpoint, ingress, admin port, node port, and container name from
  `<port>`, so concurrent runs on distinct workbench ports do not need manual Restate
  overrides. Teardown:
  `just agent-workbench-down <port>`.
- To judge the same flow on Postgres, add `AGENT_WORKBENCH_POSTGRES=1` to the boot
  command. The dev helper starts a Postgres 16 container on a port derived from
  `<port>`, passes its URL as `AGENT_WORKBENCH_DATABASE_URL`, records a managed-container
  marker beside the Restate marker, and preserves both containers across
  `agent-workbench-restart`. `agent-workbench-down` removes both. Record the Postgres
  container name and require the startup trace payload's `store_backend` to be
  `"postgres"` before Phase 1.
- Browser affordances: chat composer, **stop turn** button, running/idle pill, transcript.
- Backend truth: `GET /api/state`; `POST /api/turn`; `POST /api/turn/cancel`.
  `/api/state.active_turns` exposes routing addresses so reload can restore the Stop
  affordance. The cancel response contains `accepted` and `cancellations[]`, each with
  `address`, gate `outcome`, and authoritative `terminal`.
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
3. its terminal is committed, encodes `TurnStop::Cancelled`, and carries evidence per
   golden rule 2;
4. the UI renders `turn stopped · request <id>` with the same id, returns to idle, and
   hides Stop;
5. `GET /api/state` no longer lists that address and its messages agree with the rendered
   interrupted terminal.

Screenshot `01-cancelled.png`; save the cancel response as `01-cancel-receipt.json`.

## Phase 2 — Restart the web process mid-turn, then Stop

Submit another long-running turn. Gate on Stop plus one `/api/state.active_turns` entry
and record its session/turn ids. Run `just agent-workbench-restart <port>` without
touching Restate. Poll `/healthz` until the replacement process is ready, reload the page,
and gate all of the following before pressing Stop:

- the rendered session id is unchanged;
- `/api/state.active_turns` contains the exact pre-restart address;
- the running pill reads restored/running and **stop turn** is visible;
- `<data-dir>/active-turns.json` contains the same address.

Screenshot `02-restored-running.png`. Press Stop and repeat every receipt/UI/API gate from
Phase 1. Additionally require the terminal evidence request id to be new and the trace to
show the recovered request against the pre-restart turn id. Screenshot
`03-restored-cancelled.png`; save `03-cancel-receipt.json`.

## Phase 3 — Stop over process await, then prove process-survives revocation

Submit a prompt that makes the agent declare and start a process with a long sleep, then
foreground-await its handle. This phase is valid only if `/api/state` shows one active
turn and `/api/work` shows that named process as non-terminal before Stop. If the model
does not produce that shape, retry the phase; ordinary model or tool work is not
process-await evidence.

Press **stop turn** and repeat the Phase 1 receipt gates. Additionally require:

1. the turn terminal is committed `Cancelled`, with the Stop request evidence;
2. the named process reaches terminal `Cancelled` in `/api/work`;
3. that process's events include `process.cancel_requested`;
4. neither terminal waits for the process's original sleep deadline.

Save the pre-Stop and terminal `/api/work` responses as
`04-process-await-running.json` and `05-process-await-cancelled.json`. Screenshot the
committed turn terminal as `05-process-await-cancelled.png`.

Start a second process-await whose process can complete soon enough to observe, but leave
the foreground turn suspended. Record the old session id and process id, then call
`DELETE /api/session?session_id=<old-session-id>` instead of Stop. Gate the semantic
split:

- the old Restate turn completes with the typed deleted-session refusal, not a successful
  or `Cancelled` turn terminal;
- the process remains non-terminal immediately after deletion and stays globally visible
  in `/api/work`;
- its events contain no `process.cancel_requested`;
- it later reaches its own successful terminal with the expected value.

Save the work snapshots as `06-revoked-process-running.json` and
`07-revoked-process-survived.json`, and retain the trace lines containing the
deleted-session refusal. A cancelled process, a missing process, or a stranded old turn
is Abort/RCA.

## Phase 4 — Teardown and score

Run `just agent-workbench-down <port>` and confirm both the workbench process and its
Restate container are gone.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Boot | `/healthz` 200; rendered/API session ids agree | | `00-ready.png` |
| Normal Stop | committed Cancelled terminal + evidence | | `01-cancelled.png`, `01-cancel-receipt.json` |
| Routing persistence | same session/turn address before and after restart | | `02-restored-running.png`, state files/API |
| Restored Stop affordance | running pill + Stop restored from `/api/state.active_turns` | | `02-restored-running.png` |
| Post-restart Stop | committed Cancelled terminal + evidence for original address | | `03-restored-cancelled.png`, `03-cancel-receipt.json` |
| Stop over process await | turn and awaited process both commit Cancelled before the original deadline | | `04-process-await-running.json`, `05-process-await-cancelled.json`, screenshot |
| Revoked turn settlement | old turn completes with typed `SessionDeleted` refusal | | reset response + trace |
| Process survives revocation | process stays globally visible, has no cancel event, and reaches its own terminal | | `06-revoked-process-running.json`, `07-revoked-process-survived.json` |
| UI/API agreement | rendered request ids equal terminal evidence ids; active addresses clear | | screenshots + receipts + `/api/state` |
| No break-glass substitution | no Admin cancel/kill used as a passing action | | command log |

**Aggregate:** did exact cooperative cancellation produce authoritative evidence both
normally and after reconstructing the entire web process, did Stop propagate through a
foreground process await, and did session revocation unwind only the dead turn while the
independent process survived?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
