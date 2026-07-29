# E2E Scenario: Workbench Execution-State Rehydration — Cold Open After a Reference-Only Turn

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface,
> screenshot, polling, real-token, Abort/RCA, and teardown rules. This runbook adds only
> the execution-state rehydration scenario.

**Purpose.** Prove that a replacement Workbench process rehydrates the session's **RLM
execution state** — the Lashlang variables bound by earlier code — from the durable
checkpoint, and that it does so identically on SQLite and PostgreSQL. A checkpoint
component body is stored once and later commits carry only its reference; the reference
must still resolve to a body when a cold process hydrates the session.

**Why this is not the session-resume scenario.** `workbench-session-resume` proves
committed *transcript* nodes return after a process replacement. Transcript survival is
not state survival: a session can render every past message while its bound variables are
gone. This scenario targets the other half of the checkpoint and deliberately places a
**no-new-binding turn** between the binding turn and the restart. RLM requires a
terminating `finish`, so every successful turn executes Lashlang; "runs no code" is not a
satisfiable condition. The relevant distinction is whether that required code changed
execution state.

**Real tokens.** Turns use OpenRouter and are model-nondeterministic. Gate on
`exec_code_started`, the operator's literal marker, and the provider request built before
the post-restart code ran — never on the assistant's ability to recall.

## Scenario-specific golden rules

1. **The marker must live in a variable, not only in the transcript.** The recall gate is
   the traced Lashlang execution plus the pre-execution provider request. "The agent
   answered correctly" is never sufficient on its own: the marker is also in committed
   history, so prose alone proves nothing about execution state.
2. **The middle turn must create no new binding.** It will execute at least the required
   `finish`. Gate on the `exec_code_started` source for that turn: it may read values and
   terminate, but it must contain no assignment, declaration, or mutation. A new binding
   makes the reference-only shape unproven, so retry once with a simpler prompt; a second
   mutation is a scenario-promptability finding → Abort/RCA.
3. **Do not use timeline code-block absence as evidence.** Settled code-block rows are
   expected to render; when using one as UI evidence, assert its presence positively.
   Its absence cannot discriminate a reference-only commit from a dirty executor. The
   browser and `/api/state` prove the turn settled; `trace.jsonl` proves what Lashlang
   executed.
4. **Replace only the web process.** Invoke `agent-workbench-restart` with the same data
   directory and backend environment as boot. The helper preserves the Restate container
   and (in the PostgreSQL pass) the managed Postgres container. Reloading the page,
   changing configuration, or tearing anything else down forfeits the cold-open proof.
5. **Both geometries or no verdict.** Run the whole scenario twice: the default SQLite
   stack and the PostgreSQL stack. A pass in one geometry and a failure in the other is a
   backend-contract divergence → Abort/RCA naming the failing backend; it is not a partial
   pass.
6. **Recall by reading, not by re-assigning.** If the post-restart Lashlang source assigns
   or redefines the variable before reading it, the run learned nothing about hydration.
   Re-prompt once with an explicit "do not assign it" instruction; a second re-assignment
   is a finding about the scenario's promptability, reported as such.
7. **Keep the marker small and single-line.** Bound variables are rendered into the prompt
   in full only while they are small; large values render as a truncated preview and the
   provider-request gate below would then match nothing.

## Working material

- Require `OPENROUTER_API_KEY`; a missing key is a harness gap → Abort.
- SQLite pass:
  `AGENT_WORKBENCH_DATA_DIR=<fresh-tmp> AGENT_WORKBENCH_OPEN=0 just agent-workbench <port>`.
  Gate `GET /healthz` → 200. Teardown: `just agent-workbench-down <port>`. The dev helper
  explicitly forwards `AGENT_WORKBENCH_DATA_DIR` to the workbench process.
- PostgreSQL pass: the same command with `AGENT_WORKBENCH_POSTGRES=1` and a **second fresh
  data directory and port**. Gate the startup trace's `store_backend: "postgres"` before
  running any turn; the helper owns the port-isolated Postgres container and removes it on
  `agent-workbench-down`.
- Browser affordances: the chat composer, the timeline, the busy/idle pill, and the
  rendered session id.
- Backend truth: `GET /api/state` and `POST /api/turn`.
- Durable truth: `<data-dir>/session-id` and `<data-dir>/trace.jsonl`. Trace records use
  serde-flattened payloads, so `type` and `request` sit at the record's top level and
  request messages have the shape `{ "role": ..., "blocks": [{ "kind": ..., "text": ... }] }`.
  `llm_call_started` is a top-level record type. `exec_code_started` is not: select a
  record with `type == "protocol_step"` and
  `payload.diagnostic.phase == "exec_code_started"`; its exact executed source is at
  `payload.diagnostic.payload.code`.
- The bound-variable preamble the runtime builds from live execution state opens with the
  literal sentence `These variables are already bound in lashlang.` Its presence, plus the
  variable name and marker in the same request, is the hydration witness used below.

The browser/API surface does not expose whether the hydrated execution-state body was
present in a commit or only its component reference. This runbook uses the no-new-binding
trace as the public observable that should produce a reference-only commit, then tests the
result by cold hydration. A deterministic store conformance test should separately assert
the exact commit shape (`execution_state == None` with an unchanged
`execution_state_ref`) so that implementation property is gated without backend-specific
blob decoding in a browser run.

## Phase 0 — Boot and identify the durable session

Boot, poll `/healthz`, and open the browser. Record the workbench PID, the rendered
session id, `/api/state.settings.session_id`, and `<data-dir>/session-id`; require all
three ids to agree. In the PostgreSQL pass, additionally require the startup trace to
report the Postgres backend. Record the current trace end offset as the Phase-1 boundary.
Screenshot `00-ready.png`.

## Phase 1 — Bind a variable through executed Lashlang

Choose a short single-line marker such as `FIG636-EXEC-<run-id>`. Submit one turn asking
the agent to run Lashlang that binds a session variable named `fig636_marker` to that
exact literal and then finishes with the single word `stored`. Poll until the pill is idle
and `/api/state.active_turns` is empty.

From trace records after the Phase-1 boundary, require an `exec_code_started` source that
binds `fig636_marker` to the exact marker. Also require `/api/state.messages` to have
gained one ordered user/assistant pair. A failed execution is failed setup, so retry the
phase once; a second failure → Abort/RCA.

Save the matching trace record as `01-bound-exec.json`, save `/api/state` as
`01-bound-state.json`, record the new trace end offset as the Phase-2 boundary, and
screenshot the settled pair as `01-bound.png`.

## Phase 2 — Commit a turn with no new binding

Submit a short conversational turn and explicitly require the agent to answer without
declaring, assigning, or mutating any Lashlang variable. It must still terminate with
`finish`. Poll until idle.

Gate all of the following:

- `/api/state.messages` gained exactly one further ordered user/assistant pair;
- after the Phase-2 boundary, the turn has at least one `exec_code_started` record;
- every such record contains no binding declaration, assignment, or mutation, and the
  terminal record uses `finish`.

Do not consult the timeline's code-block rows for this gate. If the source creates a
binding, retry once with a simpler prompt; a second mutation is a promptability finding →
Abort/RCA. Save the matching records as `02-reference-only-exec.json`, record the current
trace end offset as the restart boundary, and screenshot the settled pair as
`02-no-new-binding-turn.png`.

## Phase 3 — Replace the web process

Run
`AGENT_WORKBENCH_DATA_DIR=<same-tmp> [AGENT_WORKBENCH_POSTGRES=1] just agent-workbench-restart <port>`
and poll `/healthz` until ready. Omit the bracketed PostgreSQL setting only for the SQLite
pass. Require a new PID and an unchanged session id across the rendered page,
`/api/state`, and `<data-dir>/session-id`. Reload the browser and require all four
pre-restart rows to render in their original order. Screenshot `03-reconstructed.png`.

## Phase 4 — Prove the variable returned before the model spoke

Submit one turn instructing the agent to read the existing variable `fig636_marker` and
finish with its value, to run exactly one Lashlang block, and not to assign or redefine
the variable. Poll until idle.

From trace records written **after** the Phase 2 restart boundary, take the first
`llm_call_started` payload and save it as `04-provider-request.json`. Then gate:

- **Hydration.** That request contains the bound-variable preamble sentence, the name
  `fig636_marker`, and the exact marker. This request is assembled from live execution
  state before the turn runs any code, so it witnesses the rehydrated checkpoint rather
  than the model.
- **Execution.** The first `exec_code_started` source after that request references
  `fig636_marker`, contains no assignment or declaration of that name, and terminates with
  `finish`.
- **Agreement.** The rendered assistant answer contains the exact marker and
  `/api/state.messages` matches the rendered transcript.

A run where execution and agreement pass while hydration fails is the dangerous case: the
answer was reconstructed from committed history, not from the checkpoint. Treat it as a
contract violation at store persistence → Abort/RCA. An execution failure naming an
unbound variable is the same finding with a louder symptom.

Save the execution trace as `04-recall-exec.json`, save `/api/state` as
`04-recall-state.json`, and screenshot the final answer as `04-recall.png`.

## Phase 5 — Repeat the whole scenario on PostgreSQL

Tear the SQLite stack down, then run Phases 0–4 again on the PostgreSQL stack with a fresh
data directory, a fresh port, and a new run id. Save the second pass's artifacts under a
`postgres/` prefix. Require both passes to reach the same verdict on every gate; record
any per-gate divergence explicitly, because a backend-specific loss of execution state is
the exact defect class this scenario exists to catch.

## Phase 6 — Teardown and score

Run `just agent-workbench-down <port>` for both stacks and confirm each workbench process,
its Restate container, and (PostgreSQL pass) its Postgres container are gone.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Boot identity | rendered/API/disk session ids agree; Postgres pass reports its backend | | `00-ready.png` |
| Variable bound | `exec_code_started.code` binds the name to the marker | | `01-bound-exec.json`, `01-bound-state.json` |
| No-new-binding turn | one further committed pair; traced code mutates no binding and finishes | | `02-reference-only-exec.json`, `02-no-new-binding-turn.png` |
| Cold reconstruction | PID changed; session id and all four rows survived | | `03-reconstructed.png` |
| Hydration before execution | post-restart provider request carries the bound variable and marker | | `04-provider-request.json` |
| Recall by reading | traced code references the variable without assigning it | | `04-recall-exec.json`, `04-recall.png` |
| Cross-backend agreement | SQLite and PostgreSQL passes reach identical per-gate verdicts | | both artifact sets |

**Aggregate:** did a cold process recover the session's bound Lashlang state — not merely
its transcript — across a no-new-binding turn in both durable geometries?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
