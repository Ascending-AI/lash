# E2E Scenario: Workbench Execution-State Rehydration — Cold Open After a Reference-Only Turn

> **Read [../RULES.md](../RULES.md) first** — especially the browser-surface,
> screenshot, polling, real-token, Abort/RCA, and teardown rules. This runbook adds only
> the execution-state rehydration scenario.


> **Workbench process replacement (FIG-1164, FIG-3035).** The non-destructive
> same-configuration restart is `just agent-workbench-restart <port>`, which keeps the Restate
> journals and the application data. No step of this row is blocked any more. See the
> [central lifecycle constraint](../RULES.md#agent-workbench-lifecycle-constraint-fig-1164);
> never substitute the destructive reset.

**Purpose.** Prove that a replacement Workbench process rehydrates the session's **RLM
execution state** — the TypeScript variables bound by earlier code — from the durable
checkpoint, and that it does so identically on SQLite and PostgreSQL. The checkpoint is a keyed component set: the execution-state root holds logical
binding names and inline small values or content-addressed leaf references. Large values
live in separate typed MessagePack leaves. A changed root and changed leaves commit in
one transaction; unchanged leaves carry references without bodies. Cold open must resolve
the complete root-plus-leaves set, not merely recover the transcript.

**Current format boundary.** [ADR 0056](../../docs/adr/0056-checkpoint-components-generalize-to-a-keyed-set.md)
was amended by FIG-1728: snapshot v14 removed guest scratch files and file-body leaves.
The current root retains globals and deferred resolutions. Do not interpret the ADR's
historical file paragraphs as a supported file checkpoint API. The ticket's binary-file
gate is obsolete for the same reason (see the version history in
`crates/lash-protocol-rlm/src/executor/snapshot.rs`, under the `RLM_SNAPSHOT_VERSION` constant).

**Why this is not the session-resume scenario.** `workbench-session-resume` proves
committed *transcript* nodes return after a process replacement. Transcript survival is
not state survival: a session can render every past message while its bound variables are
gone. This scenario targets the other half of the checkpoint and deliberately places a
**no-new-binding turn** between the binding turn and the restart. RLM requires a
terminating `finish`, so every successful turn executes code; "runs no code" is not a
satisfiable condition. The relevant distinction is whether that required code changed
execution state.

**Real tokens.** Turns use OpenRouter and are model-nondeterministic. Gate on
`exec_code_started`, the operator's literal marker, and the provider request built before
the post-restart code ran — never on the assistant's ability to recall.

## Scenario-specific golden rules

1. **The marker must live in a variable, not only in the transcript.** The recall gate is
   the traced TypeScript execution plus the pre-execution provider request. "The agent
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
   browser and `/api/state` prove the turn settled; `trace.jsonl` proves what code
   executed.
4. **Replace only the web process.** `just agent-workbench-restart <port>` replaces the
   Workbench process and keeps the Restate container and its journals, the application data
   directory and, in the PostgreSQL pass, the managed Postgres container. Invoke it with the
   same data directory, backend environment and `RESTATE_AUTHORITY_ID` as boot; the launcher
   refuses rather than replacing anything if any of those differ. Reloading the page,
   changing configuration, or tearing anything else down forfeits the cold-open proof.
5. **Both geometries or no verdict.** Run the whole scenario twice: the default SQLite
   stack and the PostgreSQL stack. A pass in one geometry and a failure in the other is a
   backend-contract divergence → Abort/RCA naming the failing backend; it is not a partial
   pass.
6. **Recall by reading, not by re-assigning.** If the post-restart TypeScript source assigns
   or redefines the variable before reading it, the run learned nothing about hydration.
   Re-prompt once with an explicit "do not assign it" instruction; a second re-assignment
   is a finding about the scenario's promptability, reported as such.
7. **Keep the marker small and single-line.** Bound variables are rendered into the prompt
   in full only while they are small; large values render as a truncated preview and the
   provider-request gate below would then match nothing.

## Root-plus-leaves gates

- **Do:** in Phase 1, also ask the agent to bind `fig1196_payload` to a string built from
  512 repetitions of the run's unique marker, and `fig1196_counter` to 137. Ask for only
  `stored` as the answer. **Expect:** executed source creates all three bindings; the
  payload exceeds the 512-byte leaf threshold. Record its expected length
  independently from the operator's marker; save that as `01-payload-oracle.json`.
  Keep `fig636_marker` small for the pre-execution prompt gate.
- **Do:** after Phase 1, submit a turn that binds a **new** name
  `fig1196_counter_next` to `fig1196_counter + 29` and finishes with it.
  **A later cell cannot mutate an earlier cell's binding:** every hydrated session
  global is re-declared `BindingKind::Const` in the next cell's ambient scope
  (`crates/lash-typescript/src/lower/mod.rs`), so `fig1196_counter += 29` always
  fails `TS_ASSIGN_CONST` and `let fig1196_counter = fig1196_counter + 29`
  fails `TS_TEMPORAL_DEAD_ZONE`, whatever keyword Phase 1 used. Dirty the root
  with a new name, not with a mutation; an accepted in-place mutation would be
  the finding. **Expect:** rendered answer contains 166; API and trace
  agree on one additional completed pair, and source does not rebind the payload. Save
  `01-dirty-root.png` and `01-dirty-root-exec.json`. Take the Phase-2 trace boundary after
  this turn. The reconstruction gate expects every pre-restart row this row's
  phases actually committed, counted from `/api/state.messages` before the restart.
- **Do:** preserve Phase 2's no-new-binding turn and Phase 3's cold process replacement.
  **Expect:** both the changed root and retained payload leaf resolve after reopen.
- **Do:** in Phase 4, also ask the agent to read the existing payload and counter, returning
  the payload length and counter without creating or assigning any binding. **Do not ask
  for a SHA-256:** the RLM TypeScript surface exposes no hashing helper, so a digest gate
  can only be answered "unavailable". Take the content witness from the hydrated
  bound-variable preamble instead, whose payload preview must show the run's marker
  repeated and the exact `len=` from the oracle.
  **Expect:** rendered length matches `01-payload-oracle.json` and the counter is 166, the
  executed source reads those variables, and the API agrees. The prompt truncates the
  payload; do not require its full contents in the provider request. Capture
  `04-leaf-recall.png`.
The deterministic `session_lifecycle_growth::flat_commit_growth_after_large_bindings_stabilize`
test separately observes accepted commit inputs with production budget accounting. Run it as

```sh
cargo nextest run -p lash-runtime --all-features --lib session_lifecycle_growth::flat_commit_growth_after_large_bindings_stabilize
```

— the full module path with `-p lash-runtime --all-features --lib`. The obvious shorter
invocations select nothing and report `0 passed`, which reads green: require the run to
report exactly one test passed, or the gate is unobserved. It
checks forty dirty turns retain sixteen large leaf identities, excludes unchanged bodies,
and requires exactly one submitted leaf body after rebinding one large value. Browser
traces establish executed operations; they do not expose exact submitted component bodies.

## Working material

- Require `OPENROUTER_API_KEY`; a missing key is a harness gap → Abort before boot.
- Execute the `typescript` row on SQLite and on PostgreSQL with independent fresh data
  directories, ports, markers, and artifacts. Set a fresh `RESTATE_AUTHORITY_ID` and
  `OPENROUTER_MODEL=deepseek/deepseek-v4-pro` on boot and restart; verify the served
  dialect and model from the request/execution trace. Prompts ask for outcomes and every
  gate reads the TypeScript surface.
- Source the fork's `env.sh` before `just` recipes that invoke Cargo.
- SQLite pass:
  `AGENT_WORKBENCH_DATA_DIR=<fresh-tmp> AGENT_WORKBENCH_OPEN=0 bash scripts/agent-workbench-dev.sh up --port <port>`.
  Gate `GET /healthz` → 200. Restart with `… restart --port <port>` and tear down with
  `… down --port <port>`. `scripts/agent-workbench-dev.sh` is the live path: since FIG-3153
  it builds through `kiln build --config=judged //examples/agent-workbench:agent-workbench`
  and honours `AGENT_WORKBENCH_BIN`. The `just agent-workbench` / `agent-workbench-restart` /
  `agent-workbench-down` recipes are the same operations by their older names. The dev helper
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
  `llm_call_started` and `exec_code_started` are top-level record types: select a record
  with `type == "exec_code_started"`; its exact executed source is at top-level `code`.
- The bound-variable preamble the runtime builds from live execution state opens with the
  literal sentence `These variables are already bound in TypeScript.` Its presence, plus the
  variable name and marker in the same request, is the hydration witness used below.

The browser/API surface does not expose whether the hydrated execution-state body was
present in a commit or only its component reference. This runbook uses the no-new-binding
trace as the public observable that should produce a reference-only commit, then tests the
result by cold hydration. The deterministic growth test separately asserts the submitted keyed component shape:
unchanged execution leaves have references and no bodies; rebinding one large value
submits exactly one new leaf body. No legacy fixed execution-state slot is assumed.

## Phase 0 — Boot and identify the durable session

Boot, poll `/healthz`, and open the browser. Record the workbench PID, the rendered
session id, `/api/state.settings.session_id`, and `<data-dir>/session-id`; require all
three ids to agree. In the PostgreSQL pass, additionally require the startup trace to
report the Postgres backend. Record the current trace end offset as the Phase-1 boundary.
Screenshot `00-ready.png`.

## Phase 1 — Bind a variable through executed TypeScript

Choose a short single-line marker such as `FIG636-EXEC-<run-id>`. Submit one turn asking
the agent to run TypeScript that binds a session variable named `fig636_marker` to that
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
declaring, assigning, or mutating any TypeScript variable. It must still terminate with
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

Run the non-destructive same-configuration replacement, in the shell that still exports this
row's `RESTATE_AUTHORITY_ID`:

```sh
AGENT_WORKBENCH_DATA_DIR=<same-tmp> [AGENT_WORKBENCH_POSTGRES=1] just agent-workbench-restart <port>
```

It keeps the Restate journals and the application data; never substitute
`just agent-workbench-reset`, which deletes exactly the evidence this phase needs. After it
returns, poll `/healthz` until ready. Omit the bracketed PostgreSQL setting only for the
SQLite pass. Require a new PID and an unchanged session id across the rendered page,
`/api/state`, and `<data-dir>/session-id`. Reload the browser and require every
pre-restart row, counted before the restart, to render in its original order. Screenshot
`03-reconstructed.png`.

## Phase 4 — Prove the variable returned before the model spoke

Submit one turn instructing the agent to read the existing variable `fig636_marker` and
call `finish(...)` with its value, to run exactly one TypeScript cell, and not to assign or redefine
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
data directory, a **second port taken from this row's own allocation in the parity matrix and
the RULES.md port budget** (never a port another row may hold — two concurrent rows that both
"pick a fresh port" can collide), and a new run id. Save the second pass's artifacts under a
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
| No-new-binding turn | one further committed pair; traced code mutates no binding and finishes | | `02-reference-only-exec.json` (the authoritative trace evidence; `02-no-new-binding-turn.png` is a checkpoint screenshot only — per golden rule 3 the absence of a code block proves nothing, so the shot corroborates and never carries this gate) |
| Cold reconstruction | PID changed; session id and every pre-restart row survived | | `03-reconstructed.png` |
| Hydration before execution | post-restart provider request carries the bound variable and marker | | `04-provider-request.json` |
| Recall by reading | traced code references the variable without assigning it | | `04-recall-exec.json`, `04-recall.png` |
| Retained large leaf | payload length and marker preview, and `fig1196_counter_next` 166, survive dirty-root and cold reopen | | `01-payload-oracle.json`, `04-provider-request.json`, `04-leaf-recall.png` |
| Cross-backend agreement | SQLite and PostgreSQL passes reach identical per-gate verdicts | | both artifact sets |

**Aggregate:** did a cold process recover the session's bound TypeScript state — not merely
its transcript — across a no-new-binding turn in both durable geometries?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
