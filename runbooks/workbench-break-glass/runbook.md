# E2E Scenario: Workbench Break-Glass Operator Flow

> **Read [../RULES.md](../RULES.md) first.** Its browser, polling, evidence,
> Abort/RCA, and teardown rules apply. This runbook uses the documented Restate Admin
> `KILL` deliberately; unlike ordinary Stop runbooks, that is the fault under test.

**Purpose.** Wedge one foreground turn in the local exec-blocked shape described by
[ADR 0039](../../docs/adr/0039-turn-cancellation-is-a-first-party-work-driver-primitive.md), force-kill its
Restate invocation, then judge whether Workbench tells the truth: no fabricated
`Cancelled`, bounded Stop clears the dangling route with an unknown terminal, and the
same session can run its next turn.

**Store-side companion, not repeated here.** `just restate-postgres-workers-e2e` includes
the `break-glass-negative` gate: after an admin kill, the durable store has neither a
fabricated terminal result nor a Lash `Cancelled` terminal. Run or cite that gate as
scripted evidence. This runbook covers only the operator and browser story.

## Scenario-specific golden rules

1. Boot only with `AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=exec-blocked`. Its first call
   emits a ten-minute foreground TypeScript sleep; its second call deterministically
   finishes `session recovered after break glass`.
2. Identify the exact `WorkbenchTurnWorkflow` invocation by the active turn id. Never
   kill by service prefix, most-recent guess, container, or process id.
3. Use Restate Admin `PATCH /invocations/<id>/kill`, not cooperative cancel. KILL is
   owner destruction, not cancellation evidence, and must never produce a rendered or
   durable `Cancelled` result.
4. Press the Workbench Stop control only after Restate reports the invocation non-active.
   The Workbench renders two cancel affordances and neither is labelled `stop turn`:
   `#stop` is **stop after step** (finish the step, commit its tool calls, then end) and
   `#abort` is **abort** (cancel now and drop uncommitted work). This phase uses **abort**
   (`#abort`), because the owner is already destroyed and there is no step left to finish.
   The expected
   receipt is HTTP 202 with `accepted: true` and a single cancellation whose `status` is
   `cancellation_recorded_terminal_pending` and which carries **no** `terminal` and no
   `terminal_error`: the cancellation is recorded, the terminal never arrives because the
   owner was destroyed. That pending status is exactly what makes the page render
   "turn route cleared · terminal outcome unknown"
   (`examples/agent-workbench/assets/index.html`). This is a passing break-glass result,
   not a successful cancellation.
5. Recovery must reuse the original session. Allow up to 90 seconds for lease fencing
   and successor acquisition, but poll throughout—never sleep to decide readiness.

## Working material

- Choose `<port>` and `<fresh-data-dir>`, then boot:
  `AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=exec-blocked AGENT_WORKBENCH_DATA_DIR=<fresh-data-dir> AGENT_WORKBENCH_OPEN=0 bash scripts/agent-workbench-dev.sh up --port <port>`
  (the `just agent-workbench <port>` recipe is the same command, but it does not export
  `CARGO_TARGET_DIR`, so source the fork's `env.sh` first).
- The helper derives `offset=(<port>-3030)*10` — note the factor of ten — then Restate
  ingress port `8080+offset` and Restate admin port `19070+offset`. For port 3507 that is
  ingress 12850 and admin 23840, not 8557/19547. Read the ports back from the host's own
  `agent_workbench.startup` trace record (`restate_ingress_url`,
  `restate_endpoint_addr`) rather than trusting any arithmetic. That record carries **no**
  admin field, so the admin port does come from the `19070+offset` arithmetic; record
  `ADMIN=http://127.0.0.1:<derived-admin-port>` and then *verify* it with a probe before
  relying on it — `POST $ADMIN/query` with
  `{"query":"SELECT count(*) as n FROM sys_invocation"}` must answer with a row.
- UI: session id, composer, running/idle pill, the two cancel controls (**stop after step**
  `#stop`, **abort** `#abort`), transcript/code execution.
  Backend: `GET /api/state`, `POST /api/turn`, `POST /api/turn/cancel`. Disk:
  `<fresh-data-dir>/active-turns.json`, the durable store, and `trace.jsonl`.
  `<fresh-data-dir>/sessions.json` is **not** disk truth for the session id: it lists the
  launcher's own boot-created session and never the scoped session that runs the row.
- Restate Admin SQL is `POST $ADMIN/query` with JSON `{ "query": "..." }`. Send
  `accept: application/json`; without it the endpoint answers in Arrow IPC and a JSON
  decode fails on the first byte. KILL is
  `PATCH $ADMIN/invocations/<invocation-id>/kill` with an empty body.
- Teardown: `bash scripts/agent-workbench-dev.sh down --port <port>` with the same environment.

## Phase 0 — Boot and identify the session

Poll `/healthz`, open the page, and require the host's own evidence that the scenario is
armed. **Phase 1 is single-shot in a process**: the dev scenario's call counter is process
global (`examples/agent-workbench/src/failure_provider.rs`) and only call index 0 wedges, so
a mis-fired or unobserved Phase 1 is not retryable in place — replace the process with
`bash scripts/agent-workbench-dev.sh restart --port <port>`, which resets the counter and
keeps the data directory, and start the phase again. Require: the process log's `development provider scenario enabled: exec-blocked` line and the
`agent_workbench.startup` trace record, whose `dev_provider_scenario` is `exec-blocked` and
whose `model.model` is `dev/failure-paths`. The page itself renders the model selector as
"provider default" and never prints the model id, so do not gate on the rendered id. Record the rendered, API, and disk session
ids and require equality. Screenshot `00-break-glass-ready.png`.

## Phase 1 — Wedge one exact turn

Submit `enter the deterministic exec block`. Poll until all of these agree:

- the page is running, Stop is visible, and the TypeScript code/execution surface shows a
  foreground `await sleep(600000)` (ten minutes) that has started but not completed;
- `/api/state.active_turns` contains exactly one address for the recorded session;
- `active-turns.json` contains that exact `session_id` and `turn_id`;
- `trace.jsonl` associates the same turn with the running code path.

Record the turn id. Screenshot `01-exec-blocked.png`; save state and matching trace rows
as `01-exec-blocked-state.json` and `01-exec-blocked-trace.json`.

Query Restate by the exact turn id, escaping it as a SQL string literal:

```sql
SELECT id, target_service_name, target_service_key, target_handler_name, status,
       completion_result, completion_failure
FROM sys_invocation
WHERE target_service_name = 'WorkbenchTurnWorkflow'
  AND target_service_key = '<exact-turn-id>'
  AND target_handler_name = 'run'
ORDER BY modified_at DESC
```

Require exactly one active row and save the query response as
`01-restate-invocation.json`. Its `target_service_key` must equal the UI/API turn id.

## Phase 2 — Admin-KILL the invocation

Send `PATCH $ADMIN/invocations/<exact-invocation-id>/kill`. Poll the exact invocation via
`$ADMIN/query` until its status is no longer `pending`, `ready`, `running`, `backing-off`,
or `suspended`. Save the response as `02-killed-invocation.json`.

Before touching Stop, gate that no source claims cancellation:

- the page does not render `turn cancelled` or `turn stopped · request`;
- `/api/state.messages` has no cancellation message for this turn;
- `trace.jsonl` has no committed outcome `cancelled` and no cancellation evidence for
  the killed turn.

The page may still show running because its persisted route is intentionally dangling.
Screenshot `02-killed-route-still-visible.png`.

## Phase 3 — Use Stop to prune the dangling route honestly

Press **abort** (`#abort`) and capture `POST /api/turn/cancel`. The unknown-terminal note is
a plain `div.note` appended by `renderNote`, not part of the state projection, so the next
re-render of the timeline from `/api/state` deletes it — measured at about one second on
screen, and the appearance itself lags the click by several seconds. Do **not** wait fifteen
seconds and then look: arm a DOM-mutation observer on the timeline before the click and
record the note's text and its insert/remove timestamps, or screenshot within about a second
of its appearance. Waiting for the re-render destroys the artifact the gate asks for.
Require exactly one cancellation receipt for the Phase 1 address with:

- HTTP 202 and `accepted: true`, with a gate outcome of `requested` or `already_requested`;
- `status: "cancellation_recorded_terminal_pending"`;
- **no** `terminal` and no `terminal_error` key at all — the request is recorded and the
  terminal never arrives, which is the honest shape after owner destruction;
- no cancellation evidence anywhere in a terminal, because no terminal exists.

Require the page to render `turn route cleared · terminal outcome unknown`, return idle,
and hide Stop. `/api/state.active_turns` and `active-turns.json` must both be empty. The
page must still not say `Cancelled` or `turn stopped · request`. Save the receipt and
state as `03-pruned-receipt.json` and `03-pruned-state.json`; screenshot
`03-route-pruned-unknown.png`.

## Phase 4 — Prove same-session recovery

Without reset, restart, or teardown, submit `prove break-glass recovery`. Poll for the
exact assistant text `session recovered after break glass`, allowing up to 90 seconds for
the successor lease. Require the page to be idle, no active route, and the rendered/API
session id to equal Phase 0. The new turn id must differ from the killed turn id.

Require its trace outcome to be committed normally with no cancellation evidence.
Screenshot `04-same-session-recovered.png`; save state and trace rows as
`04-same-session-recovered-state.json` and `04-same-session-recovered-trace.json`.

## Phase 5 — Teardown and score

Run `bash scripts/agent-workbench-dev.sh down --port <port>` and confirm the Workbench and its port-derived
Restate container are gone.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Deterministic wedge | exact route is running in foreground ten-minute exec | | `01-exec-blocked.*` |
| Exact invocation | Restate row key equals active turn id | | `01-restate-invocation.json` |
| Admin KILL | exact invocation becomes non-active | | `02-killed-invocation.json` |
| No fabricated cancellation | UI/API/trace never report Cancelled or evidence | | `02-*`, `03-*` |
| Honest route pruning | `cancellation_recorded_terminal_pending` with no terminal and no terminal_error; unknown-terminal note; route clears | | `03-pruned-*` |
| Session recovery | same session commits exact second-call response | | `04-same-session-recovered-*` |
| Store-side negative | companion harness reports `break-glass-negative` passed | | harness log |
| Teardown | Workbench and derived Restate container gone | | command log |

**Aggregate:** did the operator kill only the wedged invocation, preserve the distinction
between owner destruction and cancellation, prune the stale route honestly, and recover
the same session for new work?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
