# E2E Scenario: Host flows

Python execution is pending **FIG-5990** (explicit worker features and separate
sealed helper releases). The Python matrix row is inventory only until it lands.

> **Read [../RULES.md](../RULES.md) first.** This runbook adds the surface no
> other row covers: the process lifecycle authored first-shot against the
> plugin-tool contract. It still obeys the real-token, screenshot,
> objective-gate, Abort/RCA, and teardown rules.

**Purpose.** Prove first-shot fluency in the row's served dialect for aggregate settlement,
ordered agent loops, and the complete durable process lifecycle authored on
the shipped `processes.*` leaf tools, including suspension across a real
Workbench worker restart.

## The contract this row judges

TypeScript and Python are shipped cell dialects
([ADR 0139](../../docs/adr/0139-the-lash-vm-is-a-dialect-free-kernel.md)).
Python tool paths join the binding's module and operation with `_`: `processes_start`,
`workbench_register_trigger`, `chat_reply` and `control_continue_as`.
Each dialect row judges whether the model can author the catalogue's host flows first-shot.

A process is an uncalled top-level `async` arrow (TypeScript) or `async def`
function (Python), which the dialect lowers to
an entry of the cell's document, and every control is an effect the catalogue
declares
([ADR 0139](../../docs/adr/0139-the-lash-vm-is-a-dialect-free-kernel.md)):

| Intent | TypeScript | Python |
| --- | --- | --- |
| Define a process | `const approval = async (request) => { ... };` | `async def approval(request): ...` |
| Start one | `await processes.start({ definition: approval, args: { request } })` | `await processes_start({"definition": approval, "args": {"request": request}})` |
| Wait for its terminal | `await handle`, or `await processes.await({ handle })` | `await handle`, or `await processes_await({"handle": handle})` |
| List running ones | `await processes.list({})` | `await processes_list({})` |
| Wait inside the process for a decision | call the approval-gated `workbench_ops_apply_change` | call the same deferring host tool |
| Sleep durably | `await sleep(25)` | `await asyncio.sleep(0.025)` |

`defineProcess`, a bare `start` and `wake` are **deleted**, and lash has no
process signals, emitted progress or triggers: a decision reaches a process as
the result of a deferring host tool call, which the host resolves through its
completion key ([ADR 0137](../../docs/adr/0137-the-host-owns-events-routing-and-scheduling.md)).

`processes.start` answers the one handle kind — `{"__handle__": "lash", "id":
"p.<incarnation>.<process id>", "process_id": "<process id>"}`. The `id` is
opaque to the cell: a row that asks the model to parse, build or spell it is
judging the wrong contract. Pass the handle itself to `processes.await` or
`processes.cancel`.

## Golden rules

1. Set `AGENT_WORKBENCH_DIALECT` to the row's dialect at boot. Session creation
   carries no dialect. Confirm the startup `dialect`, prompt, cell tag and
   `/api/state.transcript[].content.language` from the row's own evidence.
2. Use the matrix's frontier model; record the served execution and judge
   models in `00-models.json`, including any substitution.
3. Ask for outcomes and constraints, not source. The model authors every cell
   first-shot. A missing method, a named dialect rejection, or a reach for a
   deleted form is recorded verbatim in `fluency-hits.json` and fails that row;
   do not extend the dialect or the catalogue during the judged run.
4. Restart with the repository helper and the exact original run/data
   directories: `bash scripts/agent-workbench-dev.sh restart --port <port>` with
   `AGENT_WORKBENCH_DATA_DIR`, `AGENT_WORKBENCH_RUN_DIR` and `AGENT_WORKBENCH_OPEN=0`
   exported (`up` and `down` are the boot and teardown forms; the
   `just agent-workbench …` recipes do not carry this row's environment). The helper prints
   `replaced process; the managed services and the application data … were retained` — that line is the readiness evidence Phase 4 asks for. The pre-restart process
   id, execution-state engine id, and
   post-restart process id must agree.

## Phase 0 — Boot and language gate

Boot a fresh Workbench with unique ports and a fresh persistent data directory.
Gate `/healthz`, `/api/state`, the rendered session id,
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
deliberately invalid arguments in one aggregate (`Promise.all` in TypeScript, `asyncio.gather` in Python), catch the aggregate
failure, and finish with the fixed marker `aggregate-rejected`. Both are real
host authorities and validate their arguments before making a network request.
Require one aggregate execution, two completed tool attempts, two structured
`invalid_tool_args` failures, and the fixed marker. The failures are **not** visible as a
top-level `outcome`/`error` field on `tool_call_completed`; the marker appears only inside
the record body, so the practical gate is
`grep -c invalid_tool_args trace.jsonl` == 2 over the phase's trace slice. Do not assert a wall-clock
winner or an A/B marker. Save `01-promise-{dom,state,trace}.json` and
`01-promise.png`. Each extract holds the surface named in its suffix, scoped to this phase:
`-dom` the rendered assistant row plus its code-block/tool card elements with their badges;
`-state` the `/api/state` transcript entries for the turn; `-trace` the phase's `trace.jsonl`
slice including both `tool_call_completed` records. An artifact whose contents are left to
the driver's guess is a screenshot of prose, not reproducible evidence.

## Phase 2 — Ordered agent loop

Ask the model to fetch one array of three work items, iterate that returned
collection with `for...of` (TypeScript) or `for` (Python), and finish with an ordered summary. The loop body may
call helpers, and it may also change the iterable, which the loop follows
live under the served dialect's iteration rules. Require one tool outcome and all three
ordered items. Save
`02-for-of-{dom,state,trace}.json` and `02-for-of.png`.

## Phase 3 — Durable process start and suspension

Ask the model — in outcome terms, never by dictating source — to start a
durable process that asks the operator for approval through the Workbench's
approval-gated tool, sleeps durably, and returns the decision it was given.

Require, from the executed cell and the trace:

* the process is an uncalled top-level async function and the start went through
  `processes.start`, with the definition passed as a value;
* the answered handle is the one handle kind, and the cell holds it rather than
  spelling an id;
* the wait is the approval tool's call, parked in the process: `/api/work` shows
  the process `waiting` on that call (`call_id`, `tool_id`) and the Workbench
  approvals panel lists it;
* the creating cell matches the row's dialect: read
  `/api/state.transcript[].content.language` and the startup `dialect`.
  There is no dialect or language field on the process artifact itself — the `/api/work`
  process record carries `input.kind = "lashvm"` (the engine kind) and a `process_name`
  lift hash, and no `trace.jsonl` record carries a `dialect`/`language` field — so the
  artifact is a dialect-free kernel document, while the creating cell proves the source dialect;
* a running handle and a visible waiting state.

Any reach for `defineProcess`, a bare `start(...)` or `wake(...)` is a
fluency hit: record the verbatim text in
`fluency-hits.json` and fail the row. Save
`03-suspended-{dom,state,store,trace}.json` and `03-suspended.png`.

## Phase 4 — Worker restart, approval and resume

Restart the Workbench worker while the process is waiting, using the same run
and data directories. Gate readiness, reopen the same session, and require the
restored cell language to remain the row's served dialect.

Approve the parked call in the Workbench approvals panel: the host resolves its
completion key, which resumes the process. In a new full judged code mode turn,
ask the model to find the process, await it, and finish with the returned
payload. Require that the approval was resolved once through the host's
completion, the handle was recovered through `processes.list` (not rebuilt from
a spelled id), one durable run (not a replacement), terminal success, the
pre-restart process id, and a cell in the row's dialect in the resumed turn.
"One durable run, not a replacement" is read off the **process count and id in
`/api/work`** — one process row, same id across the restart — not off the number of cell
executions: a resumption legitimately redrives the turn and runs another cell without creating
another process.
Save `04-resumed-{dom,state,store,trace,judge}.json` and `04-resumed.png`.

## Phase 5 — Teardown and score

Stop everything started by this row. Write `fluency-hits.json` even when empty.

| Gate | Result | Evidence |
| --- | --- | --- |
| Served language matches the row label by the row's own evidence | | `00-state.json`, trace |
| Shipped-host aggregate rejection is handled | | `01-promise-*` |
| Dialect's agent loop completes in order | | `02-for-of-*` |
| Process is a lifted async function started through `processes.start` | | `03-suspended-*` |
| Creating cell and restored execution state identify the served dialect | | `03-suspended-*` |
| Worker restart preserves process id and execution state | | `04-resumed-*` |
| Resume is delivered by resolving the parked approval call | | `04-resumed-*` |
| Full resumed judged turn finishes correctly | | `04-resumed-judge.json` |
| No deleted form reached for; hit list recorded | | `fluency-hits.json` |

The worked cells this row's contract table quotes are lowered against their
host catalogue by
`the_typescript_host_flow_examples_lower_against_their_host_catalogue`
(`crates/lash-vm-worker/src/embedding.rs`), while this live row
covers the host-level aggregate rejection semantics and the lifecycle a model
has to author unaided.
