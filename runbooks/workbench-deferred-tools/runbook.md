# E2E Scenario: Workbench Deferred Tools — Search, Next-Cell Call, Restart

> **Read [../RULES.md](../RULES.md) first** — especially named screenshots,
> polling, the three-layer cross-check, real-token use, Abort/RCA, restart environment,
> and teardown ownership. This runbook adds only the deferred-tool scenario.


> **Workbench process replacement (FIG-1164, FIG-3035).** The non-destructive
> same-configuration restart is `just agent-workbench-restart <port>`, which keeps the Restate
> journals and the application data. It is verified: the phases below execute it, and no step
> of this row is blocked any more. See the
> [central lifecycle constraint](../RULES.md#agent-workbench-lifecycle-constraint-fig-1164);
> never substitute the destructive reset.

**Purpose.** Prove the production Workbench deferred-tool path with a real model: the
resident `tools.search` capability returns a non-resident utility and persists its grant,
the model calls that utility in a separate next TypeScript cell, and the same grant remains
callable after the Workbench process restarts against the same SQLite state.

**Deterministic companion.** `kiln test --test_output=all //examples/agent-workbench:agent-workbench__unit_test --test_arg=deferred_ --test_arg=--nocapture`
gates ranking, capped preview rendering, typed unavailable outcomes, the scripted-provider
search-observation → call round trip, and SQLite reopen. This browser run judges the real
model, rendered product surface, app API, traces, and on-disk grant database together.

**Real tokens.** Both turns use OpenRouter. Gate on the exact requested SHA-256 digests,
tool-call identities, durable grant row, turn counts, and UI/API/store agreement—not on
the model's surrounding prose.

## Scenario-specific golden rules

1. **Two cells are the contract.** The first turn must contain a completed
   `tools.search` call whose observation names `text.sha256`, followed by a distinct
   `<typescript>` execution cell that calls `text.sha256`. A single cell containing both
   is a link failure, not a passing shortcut.
2. **The search observation is evidence.** Prompt text containing `text.sha256` proves
   nothing. Require the completed search tool result to contain `call_path:
   "text.sha256"`, and save the matching trace extract.
3. **Execution is evidence.** Require a completed deferred call whose raw tool id is
   `workbench_deferred_text_sha256`, plus the exact digest in the assistant reply. A
   model-computed digest without the tool call fails.
4. **Restart means the web process.** Use `bash scripts/agent-workbench-dev.sh restart
   --port <port>` (equivalently `just agent-workbench-restart <port>`) with the same explicit
   `AGENT_WORKBENCH_RUN_DIR` and `AGENT_WORKBENCH_DATA_DIR` — the verified non-destructive
   same-configuration replacement named in this runbook's FIG-1164/FIG-3035 header. Restate,
   the data directory, the session
   id, and `deferred-tool-grants.db` must remain unchanged; never substitute the destructive
   reset.
5. **No second search after restart.** Snapshot the trace byte offset before Phase 3.
   The post-restart slice must contain a completed `workbench_deferred_text_sha256` tool call
   and **no tool-call record named `search_tools`**. Gate on tool-call records, not on the
   string: the literal `tools.search` legitimately appears many times in the slice inside
   `llm_call_started` and `composition_changed` records, because it is in the advertised
   catalogue in the prompt. A `grep` for the string fails a correct run.
6. **Inspect SQLite read-only.** The row for `text.sha256` must exist before restart and
   after restart with byte-identical `grant_json`. Do not edit the database to make a
   gate pass.

## Working material

- Use a free port from this row's allocation in the **3200-3299** range per
  [RULES.md](../RULES.md). Verify it is free before
  boot. Use a fresh per-row directory under the drive's run root with `data` and `run`
  subdirectories, exported as `AGENT_WORKBENCH_DATA_DIR` and `AGENT_WORKBENCH_RUN_DIR`.
  (Earlier wording pinned `/workspace/tmp/fig1116-*/` paths and warned off ports 3056/3057;
  that is an older layout and conflicts with RULES.md.)
- Boot with both variables exported plus `AGENT_WORKBENCH_OPEN=0` and
  `RESTATE_AUTHORITY_ID=<stable-id>`:
  `bash scripts/agent-workbench-dev.sh up --port <port>`. Gate `GET /healthz` → 200. Phase 2
  restarts with the same exports and `… restart --port <port>`. (The `just agent-workbench …`
  recipes name the same operations but do not carry this row's environment.)
- UI truth: rendered session id, idle/running pill, transcript rows, composer, and
  visible assistant digest.
- HTTP truth: `GET /healthz` and `GET /api/state?session_id=<S>`.
- Disk truth: `<data-dir>/deferred-tool-grants.db`, the SQLite session graph in
  `<data-dir>/lash-sessions/durable-core.db`, `<data-dir>/trace.jsonl`, and
  `<data-dir>/lashlang-execution.jsonl`.
- Teardown: `bash scripts/agent-workbench-dev.sh down --port <port>` with the same run/data variables, then
  verify the Workbench process and managed Restate container are gone.

## Phase 0 — Fresh boot and baseline

Require `OPENROUTER_API_KEY`; a missing key is a harness gap → Abort. Choose and record a
free 3200-range port. Require both run/data paths to be absent or empty, boot the stack,
and poll `/healthz`. Open `/?session_id=<S>` using the durable session id exposed by the
app. Require the rendered session id, `/api/state.settings.session_id`, and
`<data-dir>/session-id` to agree. Record the Workbench PID and Restate container id.

Require the initial SQLite query
`SELECT call_path, grant_json FROM deferred_tool_grants ORDER BY call_path` to return no
rows. Record baseline DOM/API/store message counts and the trace byte offsets. Screenshot
`00-ready.png`; save `00-state.json`, `00-grants.json`, and `00-identities.json`.

## Phase 1 — Search observation, then deferred call

Submit one composer turn with this outcome constraint (do not paste a ready-made cell):

> Use the deferred capability search to find a text checksum operation. Obey the
> next-cell rule: first search, inspect its observation, then in a separate cell
> use the discovered operation to compute the SHA-256 of the exact UTF-8 text
> `FIG-1116 before restart`. Return the digest.

Poll for `turn_completed`, idle, and stable transcript/message counts. Then gate, in
order:

1. The completed `search_tools` result in `trace.jsonl` names `text.sha256` in its
   observation. Save the exact record(s) as `01-search-observation.json`.
2. `lashlang-execution.jsonl` shows the search and deferred call in distinct foreground
   cells, in that order. Save the matching records as `01-two-blocks.json`.
3. A completed raw `workbench_deferred_text_sha256` tool call exists after the search
   observation. Save it as `01-deferred-call.json`.
4. The structural gate is `outcome.payload.digest` on the completed deferred call, which must
   equal the locally computed SHA-256 of `FIG-1116 before restart`; that is what proves the
   path. Keep the prose check — the rendered assistant row and `/api/state.messages` contain
   the exact digest — as a **secondary** gate: it reads model prose, which this runbook
   elsewhere tells the judge not to gate on, and a re-prompt may satisfy it. A run that meets
   the structural gate but renders the digest badly — or whose cell finishes with
   `[object Object]` because it interpolated the whole result object instead of the digest
   field — has a presentation miss, not a failed deferred call.
5. SQLite contains a `text.sha256` row. Other rows returned by the same ranked search
   are allowed and must be recorded, not normalized away. Save the ordered result as
   `01-grants.json`; require the `text.sha256` grant to contain the deferred definition,
   plugin source id, and execution binding for `text.sha256`.

Run the three-layer cross-check: relative to Phase 0, the DOM has exactly one new user
and one new assistant row; `/api/state` and the session graph have exactly one committed
message of each role; and the trace has exactly one new completed turn. Record message
ids and counts as `01-crosscheck.json`. Screenshot `01-search-call-complete.png` with the
newest transcript rows visible.

## Phase 2 — Restart with the same durable state

Record the current `trace.jsonl` byte length, Workbench PID, session id, Restate
container id, and SHA-256 of `01-grants.json`. Run
`bash scripts/agent-workbench-dev.sh restart --port <port>` with the same explicit run/data
variables — the non-destructive same-configuration replacement of golden rule 4. It prints
`replaced process; the Restate deployment, its journals and the application data … were
retained`, which is the readiness evidence for this phase. Then poll `/healthz` and reload the
same session URL.

Require a new Workbench PID, unchanged Restate container id, unchanged session id, and
the Phase 1 transcript reconstructed exactly. Query SQLite again and require the
`text.sha256` row and `grant_json` to be byte-identical to `01-grants.json`. Screenshot
`02-restarted.png`; save `02-state.json`, `02-grants.json`, and `02-identities.json`.

## Phase 3 — Call the persisted grant without search

Submit one composer turn:

> Without searching again, call the already-granted `text.sha256` operation to compute
> the SHA-256 of the exact UTF-8 text `FIG-1116 after restart`. Return the digest.

Poll for `turn_completed`, idle, and stable counts. Gate:

1. The post-Phase-2 trace slice contains a completed
   `workbench_deferred_text_sha256` call and contains no `search_tools` call.
2. The rendered assistant row and `/api/state.messages` contain the independently
   computed exact digest.
3. SQLite still contains the byte-identical `text.sha256` grant.
4. The trace contains exactly one new completed turn.

Run the cumulative three-layer cross-check: 2 user rows, 2 assistant rows, 2 committed
messages of each role in `/api/state` and the session graph, and 2 completed turn
executions. Save `03-post-restart-trace.json`, `03-crosscheck.json`, and `03-grants.json`.
Screenshot `03-persisted-grant-call.png` with the second result visible.

## Phase 4 — Teardown and score

Run `just agent-workbench-down <port>` with the same run/data variables. Confirm the
Workbench process and managed Restate container are gone. Preserve the artifact directory.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Fresh identity | health ready; rendered/API/disk session ids agree; grant DB empty | | `00-*` |
| Search observation | completed `search_tools` result names `text.sha256` | | `01-search-observation.json` |
| Two-step handshake | search cell precedes a distinct deferred-call cell | | `01-two-blocks.json` |
| Deferred execution | raw deferred call completes; UI/API show exact digest | | `01-deferred-call.json`, `01-search-call-complete.png` |
| Durable grant | SQLite row contains definition, source, binding | | `01-grants.json` |
| Workbench restart | new PID; same Restate/session/data; transcript reconstructs | | `02-*` |
| Restart persistence | direct call succeeds with no new search; grant JSON unchanged | | `03-post-restart-trace.json`, `03-grants.json` |
| Three-layer projection | DOM, API/store, and trace counts agree pairwise after both turns | | `01-crosscheck.json`, `03-crosscheck.json` |

**Aggregate:** did a real model discover a non-resident operation, call it only in the
next cell, and call the persisted grant again after a Workbench restart without another
search, with UI/API/store/trace agreement throughout?

---

_Abort/RCA and reporting protocol are in [../RULES.md](../RULES.md)._
