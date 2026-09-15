# Workbench durable approval runbook

> Follow [`runbooks/RULES.md`](../RULES.md) exactly. It owns browser driving,
> objective gates, three-layer reconciliation, Abort/RCA, screenshots, boot,
> and teardown. This runbook adds only the approval scenarios.


> **Workbench process replacement (FIG-1164, FIG-3035).** The non-destructive
> same-configuration restart is `just agent-workbench-restart <port>`, which keeps the Restate
> journals and the application data. It is verified: the phases below execute it, and no step
> of this row is blocked any more. See the
> [central lifecycle constraint](../RULES.md#agent-workbench-lifecycle-constraint-fig-1164);
> never substitute the destructive reset.

**Purpose.** Prove with the real configured model that a host-gated tool parks
on Lash's real Restate completion-key machinery, the workbench exposes the wait
to an operator, approve resumes successfully, deny reaches the cell as a typed
tool failure, and a parked wait survives a workbench process restart.

**Deterministic companion.** Run
`cargo test -p agent-workbench approval -- --nocapture` plus the three named
tests in `src/main_sections/tests/approvals.rs`. These use a scripted provider
and a file-backed `SqliteEffectHost`; this judged run uses the production
Restate deployment and a real model from `.env`.

## FIG-1346 — deterministic out-of-band completion across reopen and redrive

From the fork root, source `env.sh`, then run:

```sh
cargo nextest run --locked -p agent-workbench -E 'test(async_completion_)'
```

Require **4 passed**: `async_completion_{success,failure,timeout,cancel}_crosses_session_reopen_and_redrive`.
Each row runs the real workbench approval tool with a scripted provider. The tool
records its correlation key and returns Pending. The test aborts and joins the
original turn task, drops the core, advances the injected effect clock past the
interrupted claim's lease, and reopens the ledger, effect host, core, and session.
The reconstructed host resolves the saved key through `core.completions().resolve`,
then redrives the original turn id. No provider network call or sleep drives this
companion.

Gate every row on exactly **one provider invocation**, an accepted callback,
`AlreadyResolved` retaining the exact typed terminal outcome after redrive, the
program's success/failure/cancellation result, one user input in history, retained
program/tool arguments, and identical terminal history after another core/session
reopen. Timeout is an explicit host-delivered `Resolution::Timeout`, not a wall-clock
race. Cancellation is a tool completion, not cancellation of the redriven turn.

| Completion | Typed durable terminal | Program result |
| --- | --- | --- |
| Success | `Resolution::Ok` | `ok=true`, exact supplied value |
| Failure | `Resolution::Err` | `ok=false`, `execution` / `approval_denied` |
| Timeout | `Resolution::Timeout` | `ok=false`, `timeout` / `tool_completion_timeout` |
| Cancel | `Resolution::Cancelled` | `ok=false`, runtime cancellation message |

This companion is deterministic CI evidence, not a judged browser run. Scenarios
A–C below remain the live approval/restart scorecard. The browser
currently offers approve and deny only; it has no timeout/cancel-completion route.
Do not claim the four callback variants were exercised through the browser.
If the required model key is absent, record a Phase 0 harness gap under RULES.md;
do not substitute a scripted model for the live rows.

## Golden rules

1. Use a free port from this row's allocation in the 3200-3299 range per
   [RULES.md](../RULES.md), and a fresh per-row directory under the drive's run root with
   `{data,run,artifacts}` subdirectories. (Earlier wording pinned
   `/workspace/tmp/fig1117-approval-{scenario}-{data,run,artifacts}` and warned off ports
   3056/3057; that is an older layout and conflicts with RULES.md.)
2. Boot with `bash scripts/agent-workbench-dev.sh up --port <port>`, restart with
   `… restart --port <port>`, tear down with `… down --port <port>`. Export the same
   `AGENT_WORKBENCH_DATA_DIR`, `AGENT_WORKBENCH_RUN_DIR`, `AGENT_WORKBENCH_OPEN=0` and
   `RESTATE_AUTHORITY_ID` for the boot and every restart; the `just agent-workbench …`
   recipes name the same operations but do not carry this row's environment.
3. The call must reach the host as raw tool id `workbench_ops_apply_change` from a
   `<typescript>` cell — the practical gate is
   `/api/approvals[].tool == "workbench_ops_apply_change"`. The cell itself spells the
   **authority path** `ops.apply_change`; the raw id is what the approval ledger records, and
   requiring the raw id to appear in the cell text would be wrong. A model description of
   approval is not evidence.
4. At every park checkpoint, the rendered `.approval-card`, `GET /api/state`
   `pending_approvals`, and `GET /api/approvals` must agree on key, tool,
   arguments, requesting session, and cardinality. The session graph must show
   the same one active turn with no premature assistant commit.
5. At every terminal checkpoint, reconcile rendered user/assistant rows,
   `/api/state` messages/product events, the session graph, and exactly one
   completed turn execution in `trace.jsonl`.
6. Poll gates; never decide completion with a fixed sleep. Capture every named
   screenshot under the scenario artifact directory.

## Phase 0 — fresh boot

Verify the chosen browser, Restate endpoint, and workbench ports are free. Load
the repository `.env`, create fresh data/run/artifact directories, export them,
and boot the workbench. Gate on `/healthz`, the listening line, and a hydrated
browser showing `idle`. Require `GET /api/approvals` and
`GET /api/state.pending_approvals` to both be empty. Save
`00-fresh-workbench.png`, `00-state.json`, and `00-approvals.json`.

## Scenario A — approve

Submit this intent (the exact prose may be adjusted only to make the real model
obey the structural request):

> In one TypeScript cell call `ops.apply_change` with target `demo-cluster` and
> change `enable safe mode`, awaiting it so a failure propagates, then
> `finish` the returned record. Do not merely explain the call.

1. Poll until exactly one approval card renders. Save
   `01-approve-parked.png`, `01-approve-state.json`, and
   `01-approve-approvals.json`. Reconcile the card/API key, tool, arguments,
   session, age, active turn, and absence of an assistant terminal commit.
2. Click **approve** on that card. Poll until the card disappears and the turn
   completes. Require the completed raw tool call and result fields
   `status=applied`, `target=demo-cluster`, `change=enable safe mode` in the
   execution/state evidence. Save `02-approved-complete.png`,
   `02-approved-state.json`, and `02-approved-tool-call.json`.
3. Perform the terminal three-layer count-and-identity cross-check.

## Scenario B — deny

Reset to a fresh session and submit:

> In one TypeScript cell call `ops.apply_change` with target `demo-cluster` and
> change `disable audit log`, catching the failure instead of letting it
> propagate. Inspect the thrown error and `finish` a record carrying the code from
> `error.cause.code` and the message from `error.message`. Do not retry it.

1. Gate on one matching approval across DOM, `/api/state`, and
   `/api/approvals`; save `03-deny-parked.png`, `03-deny-state.json`, and
   `03-deny-approvals.json`.
2. Click **deny**. Poll for terminal completion. Require the recorded tool
   failure the cell observed to expose `ok=false`, code `approval_denied`, message
   `the operator denied this change`, source `tool`, and retry disposition
   `never` as typed fields rather than a serialized string. Require no second
   `workbench_ops_apply_change` execution. Save `04-denied-handled.png`,
   `04-denied-state.json`, and `04-denied-tool-failure.json`.
3. Perform the terminal three-layer cross-check. A string containing JSON does
   not pass the typed-failure gate.

## Scenario C — restart while parked

Reset to a fresh session and submit:

> In one TypeScript cell call `ops.apply_change` with target `restart-demo` and
> change `rotate workers`, awaiting it so a failure propagates, then `finish`
> its status. Do not merely explain the call.

1. Gate on one approval across all three host projections and record the
   session id, approval key, active turn id, DOM row identities, committed
   message ids, and trace execution identity. Save
   `05-restart-before.png`, `05-restart-before-state.json`, and
   `05-restart-before-approvals.json`.
2. Replace the process with the verified non-destructive same-configuration restart this
   runbook's FIG-1164/FIG-3035 header names: `just agent-workbench-restart <port>`
   (equivalently `bash scripts/agent-workbench-dev.sh restart --port <port>`) with the same
   exported data/run directories. It keeps the Restate deployment, its journals and the
   application data, and prints the readiness evidence line
   `replaced process; the Restate deployment, its journals and the application data at <dir>
   were retained`. Never substitute the destructive reset. Then gate on
   that line and browser
   reconnection. Require the same session, approval key, arguments, active turn,
   and pre-restart DOM/message identities — comparing the approval card **excluding its
   `waiting Ns` age line**, which necessarily changes across a restart and would make any
   DOM snapshot non-comparable. A redrive may append another
   `turn_started` / `tool_call_started` observation for the same typed call id
   and arguments; require zero tool completions before approval and exactly one
   approvals-ledger row for the original key. Save
   `06-restart-parked.png`, `06-restart-state.json`, and
   `06-restart-approvals.json`.
3. Click **approve**. Poll until the approval disappears and the original turn
   finishes with `applied`. Save `07-restart-approved.png`,
   `07-restart-approved-state.json`, and `07-restart-trace-slice.json`.
4. Reconcile terminal identities and counts. The trace slice after restart must
   contain exactly one typed `tool_call_completed` and one `turn_completed` for
   the original call/turn, and the approvals ledger must still contain exactly
   one row for the original key, decided once. Correlate replayed start
   observations by typed call id/name/arguments; do not count their fresh trace
   record ids as provider executions.

## Scorecard

| Item | Objective gate | Result | Evidence |
|---|---|---|---|
| FIG-1346 companion | four passing reopen/redrive variants; exactly one provider call each; typed terminal and durable history assertions | | focused nextest log |
| Fresh slate | DOM idle; both approval APIs empty | | `00-*` |
| Approve parks | one identical wait across DOM and both APIs; active graph uncommitted | | `01-*` |
| Approve resumes | typed success result; one tool execution; terminal layers agree | | `02-*` |
| Deny parks | one identical wait across DOM and both APIs | | `03-*` |
| Deny is typed | the cell handles typed `approval_denied`; no retry | | `04-*` |
| Restart continuity | same session, turn, key, arguments, and row/message identities | | `05-*`, `06-*` |
| Restart resumes once | one typed tool completion + one turn completion + one decided ledger row | | `07-*` |
| Teardown | workbench and owned containers stopped; ports free | | teardown log |
