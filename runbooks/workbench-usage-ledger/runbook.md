# E2E Scenario: Workbench Usage Ledger — API, Render, Trace, Restart

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface,
> screenshot, polling, real-token, Abort/RCA, and teardown rules. This runbook adds only
> the usage-ledger scenario.


> **Workbench process replacement (FIG-1164, FIG-3035).** The non-destructive
> same-configuration restart is `just agent-workbench-restart <port>`, which keeps the Restate
> journals and the application data. It is verified: the phases below execute it, and the
> block that once stood in front of them is lifted. See the
> [central lifecycle constraint](../RULES.md#agent-workbench-lifecycle-constraint-fig-1164);
> never substitute the destructive reset.

**Purpose.** Prove that the workbench renders Lash's canonical persisted
`SessionUsageReport`, that its counters reconcile with completed LLM calls in the trace,
and that replacing the web process does not reset or mutate the ledger.

**Real tokens.** The browser turn uses OpenRouter, so exact counts vary. Gate on positive
provider-reported counters, canonical arithmetic, and equality between surfaces.

## Scenario-specific golden rules

1. **Use the canonical report.** `/api/state.usage` is `session.usage_report()`, not a
   browser accumulator. Save the entire object before comparing the rendered rail.
2. **Reasoning output is a subset.** For API and trace rows, total tokens are
   `input_tokens + output_tokens + cache_read_input_tokens + cache_write_input_tokens`.
   Never add `reasoning_output_tokens` again.
3. **Reconcile only this turn's trace window.** Record the trace byte offset or record
   count immediately before send. Sum `usage` from every later `llm_call_completed` record
   belonging to the chosen turn, including multiple RLM iterations. Treat the recorded
   boundary as a scan start and apply the turn id as the decisive filter, because idle
   polling can advance the trace file.
4. **Totals dominate rows/calls.** API session totals must equal the sum of
   `by_source_model` rows and be greater than or equal to this turn's completed-call sum.
   Input and output must be non-zero when the trace reports calls.
5. **Restart equality is exact.** After the non-destructive same-configuration replacement,
   require the full `/api/state.usage` JSON object and rendered total/input/output strings to
   equal the pre-restart values. Monotonic-but-different is a failure when no new call ran.
6. **A saturated report is an operator signal.** `/api/state.usage.saturated == true`
   means display aggregation clamped at least one counter. Preserve the raw report and
   durable rows for RCA; do not treat the displayed maximum as an exact billable total.

## Token-usage overflow triage

`token usage counter ... overflowed while accumulating` is deliberately fail closed. An
overflowing, unconfirmed row remains in the resident session's process-local
`shared_token_ledger`, so the next turn on that same resident session re-fails. Do not
retry the turn in place and do not edit the durable token ledger.

Operator remediation is to replace the resident Lash session/runtime and reconstruct it
from the last committed `RuntimePersistence` state. For Workbench, that is
`bash scripts/agent-workbench-dev.sh restart --port <port>` with the same data directory and
store — the verified non-destructive same-configuration replacement, which is the remedy this
section offers and is not blocked. Cold reconstruction clears only the non-durable `shared_token_ledger`; the committed
`RuntimeSessionState.token_ledger`, graph, checkpoint, and session identity remain the
store-authoritative state. If cold reconstruction itself reports an overflow, the bad
rows are already durable: stop, retain the database evidence, and escalate for data
repair rather than looping restarts.

## Working material

- First run `just agent-workbench-attachment-usage-gate <gate-port>` on **its own port**,
  never the browser stack's: the gate derives its managed Postgres port and its container
  names from the port it is given and holds the worktree gate while it runs. Its deterministic
  provider reports fixed non-zero usage, the gate reconciles one `llm_call_completed`
  record, reconstructs the core, and executes against both SQLite and Postgres
  session-store backends on a managed database inside the worktree block. Its
  Postgres offset is `+0..+9` from `LASH_E2E_PORT_BASE`, which is derived by
  `scripts/worktree-gate-env.sh`, selected by the last decimal digit of `<port>`
  (`3042` selects `LASH_E2E_PORT_BASE + 2`).
- Boot with a fresh directory:
  `AGENT_WORKBENCH_DATA_DIR=<fresh-tmp> AGENT_WORKBENCH_OPEN=0 bash scripts/agent-workbench-dev.sh up --port <port>`
  (the `just agent-workbench <port>` recipe is the same command, but it does not export
  `CARGO_TARGET_DIR`, so source the fork's `env.sh` first).
  Require `OPENROUTER_API_KEY`; missing credentials are a harness gap → Abort. Teardown is
  `bash scripts/agent-workbench-dev.sh down --port <port>` with the same environment.
- UI truth: left-rail **usage** total and **tokens** input/output rows, transcript,
  running/idle pill.
- API truth: `GET /api/state`, especially `usage.usage`, `usage.by_source_model`, and
  `usage.entry_count`.
- Disk truth: `<data-dir>/trace.jsonl` and the selected SQLite/Postgres session store — for
  SQLite that is `<data-dir>/lash-sessions/durable-core.db`. `<data-dir>/sessions.json` is
  **not** disk truth for this row: it lists the launcher's own boot-created session and never
  the URL-supplied session that owned the turn, so a driver that reads it fails Phase 0 on a
  correct system.

## Phase 0 — Boot and capture the baseline

Poll `/healthz`, open the browser, and require the rendered session id to agree with the
durable store's. `/api/state` carries no session field — its keys are `active_turns`,
`messages`, `observation`, `pending_approvals`, `pending_turn_inputs`, `product_events`,
`queued_work`, `settings`, `transcript`, `turn_failure_settlements`,
`turn_input_applications`, `usage` — so read the id from `settings.session_id` and from the
durable store, not from a top-level API field.
Save `/api/state` as `00-baseline-state.json`, record the current trace boundary, and
require the rendered **usage** and **tokens** values to equal the baseline report (normally
zero in a fresh directory). Screenshot `00-baseline.png`.

## Phase 1 — Run one attributable turn

Send a short deterministic-shaped instruction with a unique marker, for example:
`Reply with exactly FIG425-USAGE-<run-id> and do not call tools.` Poll until the UI is idle,
`/api/state.active_turns` is empty, and the committed user/assistant pair is present. Do
not gate on the assistant obeying the exact prose constraint; it only identifies the turn.

Save the settled state as `01-settled-state.json`. Screenshot the fully scrolled
transcript as `01-transcript-rendered.png`, then separately scroll the usage rail
into view and screenshot it as `01-usage-rail-rendered.png`. The render gate may
instead use a DOM-text assertion on `#usageTotal` and `#usageBreakdown` for the
expected rendered values.

## Phase 2 — Reconcile API, render, and trace

From trace records after the Phase 0 boundary, select every `llm_call_completed` record for
the marker's turn and save them as `02-llm-calls.json`. Require at least one completed call
and a positive canonical call sum. `llm_call_completed.response.usage` carries the six
counters but **no** `total_tokens` — compute the canonical sum from them (golden rule 2),
and do not read a null field. Then gate:

- `/api/state.usage.entry_count == usage.by_source_model.length`;
- the canonical session total equals the sum of every `by_source_model[].usage.total_tokens`;
- each report row is no larger than the session total;
- the session total is at least the sum of the selected trace call usage;
- report input total (uncached + cache read + cache write) and output are non-zero wherever
  the selected calls report non-zero values;
- `/api/state.usage.usage.total_tokens` equals the canonical sum of that same object's
  input, output, cache-read and cache-write counters. This is a separate gate from the one
  below on purpose: `renderUsageCounters` computes `#usageTotal` from the four parts and
  never reads `total_tokens`, so comparing the render to the server's total would agree even
  if the server's own total had drifted from its own parts;
- rendered **usage** is the locale-formatted API `total_tokens` plus `total`;
- rendered **tokens** is the locale-formatted API input total plus `in ·`, followed by API
  `output_tokens` plus `out`.

Save the normalized arithmetic as `02-reconciliation.json`. Any rendered/API/trace
disagreement is a contract violation → Abort/RCA.

## Phase 3 — Replace the process and prove persistence

Run `bash scripts/agent-workbench-dev.sh restart --port <port>`, the verified
non-destructive same-configuration replacement, then poll `/healthz`. Require the
PID to change while session identity remains fixed. Reload the browser without submitting a turn.
Save
the new state as `03-restarted-state.json` and require its complete `usage` object to equal
`01-settled-state.json.usage`. Require the two rendered usage rows to be text-identical to
Phase 2. Screenshot `03-usage-persisted.png`.

Also require that no new `llm_call_started` or `llm_call_completed` trace record appeared
solely because of restart/state loading.

## Phase 4 — Teardown and score

Run `bash scripts/agent-workbench-dev.sh down --port <port>` with the row's environment and
confirm the workbench and its managed services are gone.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Deterministic companion | fixed-usage SQLite + Postgres gate exits zero | | command log |
| Settled turn | one committed pair and at least one completed LLM call | | `01-*`, `02-llm-calls.json` |
| Report arithmetic | totals equal row sum and dominate selected call sum | | `02-reconciliation.json` |
| Non-zero usage | call/report input and output are positive where calls occurred | | state + trace JSON |
| Render agreement | usage and token rail text exactly formats API counters | | `01-transcript-rendered.png`, `01-usage-rail-rendered.png`, or DOM text for `#usageTotal`/`#usageBreakdown` |
| Restart persistence | PID changed; complete usage object and rail text are unchanged | | `03-*`, command log |
| Restart side effects | restart/state load emits no LLM call | | trace boundary evidence |

**Aggregate:** did the rendered token ledger agree with canonical API and trace arithmetic,
then survive a cold web-process restart without drift or hidden calls?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
