# E2E Scenario: TypeScript codemode parity

> **Read [../RULES.md](../RULES.md) first.** This runbook adds the surface no
> other row covers: the process lifecycle authored first-shot against the
> plugin-tool contract. It still obeys the real-token, screenshot,
> objective-gate, Abort/RCA, and teardown rules.

**Purpose.** Prove first-shot TypeScript fluency for aggregate settlement,
`for...of` agent work, and the complete durable process lifecycle authored on
the shipped `processes.*` leaf tools, including suspension across a real
Workbench worker restart.

## The contract this row judges

TypeScript is the only cell language ([ADR 0096](../../docs/adr/0096-typescript-is-the-sole-rlm-dialect.md)),
so "parity" here is not a second dialect: it is parity between what the
catalogue declares and what the model can author first-shot.

A process is an ordinary uncalled `async` arrow, and every control is a leaf
tool the catalogue declares
([ADR 0095](../../docs/adr/0095-processes-are-values-and-process-controls-are-tools.md)):

| Intent | Shipped spelling |
| --- | --- |
| Define a process | `const approval = async (request) => { ... };` (uncalled, top level) |
| Start one | `await processes.start({ definition: approval, args: { request } })` |
| Signal one | `await processes.signal({ handle, name: "approved", payload })` |
| Wait for its terminal | `await handle`, or `await processes.await({ handle })` |
| List running ones | `await processes.list({})` |
| Wait inside the process | `await waitSignal("approved")` |
| Sleep durably | `await sleep(25)` |
| Report progress | `await processes.emit({ value: { stage: "approved" } })` |

`defineProcess`, a bare `start`, `wake`, `registerTrigger` and the `signals:`
configuration block are **deleted**. There is no signal declaration: the set is
inferred from the `waitSignal` calls the body reaches, and the payload type is
fixed at the await site.

`processes.start` answers the one handle kind — `{"__handle__": "lash", "id":
"p.<incarnation>.<process id>", "process_id": "<process id>"}`. The `id` is
opaque to the cell: a row that asks the model to parse, build or spell it is
judging the wrong contract. Pass the handle itself to `processes.signal`,
`processes.await` or `processes.cancel`.

## Golden rules

1. There is nothing to pin. The session-creation contract carries no language
   (RULES.md), so confirm `typescript` from the row's **own** evidence — the
   rendered prompt, the cell tag, the execution events, the restored engine id
   — never from configuration.
2. Use `gpt-5.6-sol` or newer for every RLM step. Record the actual execution
   and judge models in `00-models.json`; substitutions are evidence, not prose.
3. Ask for outcomes and constraints, not source. The model authors every cell
   first-shot. A missing method, a named `TS_*` rejection, or a reach for a
   deleted form is recorded verbatim in `fluency-hits.json` and fails that row;
   do not extend the dialect or the catalogue during the judged run.
4. Restart with the repository helper and the exact original run/data
   directories. The pre-restart process id, execution-state engine id, and
   post-restart process id must agree.

## Phase 0 — Boot and language gate

Boot a fresh Workbench with unique ports and a fresh persistent data directory.
Export a fresh `RESTATE_AUTHORITY_ID` alongside it — `agent-workbench` refuses
to start without one. Gate `/healthz`, `/api/state`, the rendered session id,
and the prompt/trace language id. Save `00-ready.png`, `00-state.json`, and
`00-models.json`.

## Phase 1 — Aggregate rejection through the shipped web authorities

The assigned Agent Workbench host attaches the free Parallel Search MCP server
as the `parallel` authority in
`examples/agent-workbench/src/main_sections/bootstrap.rs`; its web-search and
web-fetch tools surface as `mcp__parallel__web_search_*` /
`mcp__parallel__web_fetch_*`. It also registers `tools.search` for deferred
discovery in `examples/agent-workbench/src/deferred_tools.rs`.
There are no delayed-rejecting A/B test tools in this shipped catalogue, so
the row must not require an operator to add them.

Ask the model to call the Parallel web-search and web-fetch tools with
deliberately invalid arguments in one `Promise.all`, catch the aggregate
failure, and finish with the fixed marker `aggregate-rejected`. Both are real
host authorities and validate their arguments before making a network request.
Require one aggregate execution, two completed tool attempts, two structured
`invalid_tool_args` failures, and the fixed marker. Do not assert a wall-clock
winner or an A/B marker. Save `01-promise-{dom,state,trace}.json` and
`01-promise.png`.

## Phase 2 — `for...of` agent loop

Ask the model to fetch one array of three work items, iterate that returned
array with `for...of`, and finish with an ordered summary. The loop body may
call helpers; what the documented v1 iterator guard forbids is mutating,
aliasing, or passing the iterable itself. Require one tool outcome, all three
ordered items, and no `TS_FOR_OF_UNSUPPORTED`. Save
`02-for-of-{dom,state,trace}.json` and `02-for-of.png`.

## Phase 3 — Durable process start and suspension

Ask the model — in outcome terms, never by dictating source — to start a
durable process that reports progress, waits for a named decision, sleeps
durably, and returns the decision it was given.

Require, from the executed cell and the trace:

* the process is an uncalled top-level `async` arrow and the start went through
  `processes.start`, with the definition passed as a value;
* the answered handle is the one handle kind, and the cell holds it rather than
  spelling an id;
* `waitSignal` is the wait and there is no `signals:` block anywhere in the
  cell;
* the process artifact's compilation dialect is `typescript`;
* a running handle and a visible waiting state.

Any reach for `defineProcess`, a bare `start(...)`, `wake(...)` or
`registerTrigger(...)` is a fluency hit: record the verbatim text in
`fluency-hits.json` and fail the row. Save
`03-suspended-{dom,state,store,trace}.json` and `03-suspended.png`.

## Phase 4 — Worker restart, signal and resume

Restart the Workbench worker while the process is waiting, using the same run
and data directories. Gate readiness, reopen the same session, and require the
restored execution engine id to remain `typescript`.

In a new full judged codemode turn, ask the model to find the still-running
process, deliver the decision to it, observe the resumed run, and finish with
the returned payload. Require that the delivery went through
`processes.signal` carrying the handle (recovered through `processes.list`, not
rebuilt from a spelled id), one durable run (not a replacement), terminal
success, the pre-restart process id, and a TypeScript cell in the resumed turn.
Save `04-resumed-{dom,state,store,trace,judge}.json` and `04-resumed.png`.

## Phase 5 — Teardown and score

Stop everything started by this row. Write `fluency-hits.json` even when empty.

| Gate | Result | Evidence |
| --- | --- | --- |
| Served language is TypeScript by the row's own evidence | | `00-state.json`, trace |
| Shipped-host aggregate rejection is handled | | `01-promise-*` |
| `for...of` agent loop completes in order | | `02-for-of-*` |
| Process is a lifted arrow started through `processes.start` | | `03-suspended-*` |
| Process artifact and suspended engine are TypeScript | | `03-suspended-*` |
| Worker restart preserves process id and dialect | | `04-resumed-*` |
| Resume is delivered through `processes.signal` on the held handle | | `04-resumed-*` |
| Full resumed judged turn finishes correctly | | `04-resumed-judge.json` |
| No deleted form reached for; hit list recorded | | `fluency-hits.json` |

The exact first-settled rejection ordering remains covered by the TypeScript
conformance tests in `crates/lash-typescript/tests/agent_surface.rs` —
`durable_processes_resume_across_await_signal_sleep_and_pending_finally`,
`uncaught_throw_fails_a_durable_process` and
`durable_process_resumes_after_shared_promise_batch` — and the worked cells this
row's contract table quotes are linked in
`crates/lash-typescript/tests/codemode_parity_examples.rs`, while this live row
covers the host-level aggregate rejection semantics and the lifecycle a model
has to author unaided.
