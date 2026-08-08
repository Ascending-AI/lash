# Workbench durable approval runbook

> Follow [`runbooks/RULES.md`](../RULES.md) exactly. It owns browser driving,
> objective gates, three-layer reconciliation, Abort/RCA, screenshots, boot,
> and teardown. This runbook adds only the approval scenarios.

**Purpose.** Prove with the real configured model that a host-gated tool parks
on Lash's real Restate completion-key machinery, the workbench exposes the wait
to an operator, approve resumes successfully, deny reaches Lashlang as a typed
tool failure, and a parked wait survives a workbench process restart.

**Deterministic companion.** Run
`cargo test -p agent-workbench approval -- --nocapture` plus the three named
tests in `src/main_sections/tests/approvals.rs`. These use a scripted provider
and a file-backed `SqliteEffectHost`; this judged run uses the production
Restate deployment and a real model from `.env`.

## Golden rules

1. Use a free port in the 3200 range. Never touch 3056 or 3057. Use fresh
   `/workspace/tmp/fig1117-approval-{scenario}-{data,run,artifacts}` paths.
2. Boot only with `just agent-workbench <port>`. Export the same
   `AGENT_WORKBENCH_DATA_DIR` and `AGENT_WORKBENCH_RUN_DIR` for every restart.
3. The model must call raw tool id `workbench_ops_apply_change` from a Lashlang
   cell. A model description of approval is not evidence.
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

> In one Lashlang cell call `ops.apply_change` with target `demo-cluster` and
> change `enable safe mode`, unwrap it with `?`, then finish the returned
> record. Do not merely explain the call.

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

> In one Lashlang cell call `ops.apply_change` with target `demo-cluster` and
> change `disable audit log` without `?`. Inspect the failed result and finish
> a record containing its typed failure code and message. Do not retry it.

1. Gate on one matching approval across DOM, `/api/state`, and
   `/api/approvals`; save `03-deny-parked.png`, `03-deny-state.json`, and
   `03-deny-approvals.json`.
2. Click **deny**. Poll for terminal completion. Require the Lashlang result to
   expose `ok=false`, code `approval_denied`, message
   `the operator denied this change`, source `tool`, and retry disposition
   `never` as typed fields rather than a serialized string. Require no second
   `workbench_ops_apply_change` execution. Save `04-denied-handled.png`,
   `04-denied-state.json`, and `04-denied-tool-failure.json`.
3. Perform the terminal three-layer cross-check. A string containing JSON does
   not pass the typed-failure gate.

## Scenario C — restart while parked

Reset to a fresh session and submit:

> In one Lashlang cell call `ops.apply_change` with target `restart-demo` and
> change `rotate workers`, unwrap it with `?`, then finish its status. Do not
> merely explain the call.

1. Gate on one approval across all three host projections and record the
   session id, approval key, active turn id, DOM row identities, committed
   message ids, and trace execution identity. Save
   `05-restart-before.png`, `05-restart-before-state.json`, and
   `05-restart-before-approvals.json`.
2. Send SIGTERM through `just agent-workbench-restart <port>` with the same
   exported data/run directories. Gate on the listening line and browser
   reconnection. Require the same session, approval key, arguments, active turn,
   and pre-restart DOM/message identities. A redrive may append another
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
| Fresh slate | DOM idle; both approval APIs empty | | `00-*` |
| Approve parks | one identical wait across DOM and both APIs; active graph uncommitted | | `01-*` |
| Approve resumes | typed success result; one tool execution; terminal layers agree | | `02-*` |
| Deny parks | one identical wait across DOM and both APIs | | `03-*` |
| Deny is typed | Lashlang handles typed `approval_denied`; no retry | | `04-*` |
| Restart continuity | same session, turn, key, arguments, and row/message identities | | `05-*`, `06-*` |
| Restart resumes once | one typed tool completion + one turn completion + one decided ledger row | | `07-*` |
| Teardown | workbench and owned containers stopped; ports free | | teardown log |
