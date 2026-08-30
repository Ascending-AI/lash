# E2E Scenario: Workbench live weather in both RLM dialects

> **Read [../RULES.md](../RULES.md) first** — especially dialect parity, browser-surface
> authority, polling, named-checkpoint screenshots, the three-layer cross-check,
> real-token use, Abort/RCA, and teardown. This runbook adds only the live-weather
> search, parse, and finish scenario.

**Purpose.** Ask the same unassisted current-weather question in fresh Lashlang and
TypeScript sessions and judge whether each dialect can turn live web results into a
finished, source-backed answer without entering a repeated execution-error loop.

**Real tokens.** Both rows use OpenRouter and the bundled Tavily web tools. Current
conditions and exact prose vary. Gate the turn on its terminal outcome, successful tool
evidence, source-backed values, and rendered answer shape rather than an exact sentence.

## Scenario-specific golden rules

1. **Ask exactly the product question.** Submit only
   `What is the current weather in Utrecht, Netherlands?` Do not add source code, parsing
   advice, a preferred weather site, retry advice, or dialect-specific hints. The path from
   search result to parsed values to `finish` is what this scenario judges.
2. **Validation-only authority.** The only agent tool operations permitted are
   `web.search` and `web.fetch`. Any process, filesystem, command-execution, messaging,
   mutation, or other host-affecting operation is a FAIL and triggers Abort/RCA. Browser
   automation and read-only evidence collection by the runbook operator are not agent tool
   operations.
3. **A settled UI is not a finished turn.** After Send, first require the rendered running
   state. Then poll for at most five minutes until the page is idle, `/api/state.active_turns`
   is empty, and the row's trace has exactly one `turn_completed` with
   `outcome.status == "completed"` and `done_reason == "final_value"`. A timeout or any
   other terminal outcome is a FAIL.
4. **Live evidence must support the answer.** Require at least one successful
   `web.search` or `web.fetch` call whose returned content names Utrecht and supplies the
   current-condition facts used in the answer. A forecast-only result, model recollection,
   or plausible-looking unsupported prose does not pass. Save the exact successful tool
   result and source URL before judging the answer.
5. **Probe values, not weather-shaped prose.** Extract the answer's current temperature,
   condition, humidity, and wind speed, with their labels and units. Require:
   - temperature with `°C` or `°F`, within the successful source's displayed rounding;
   - a recognizable condition word or phrase copied or faithfully normalized from the
     source, such as clear, sunny, cloudy, overcast, rain, drizzle, shower, snow, storm,
     fog, mist, haze, or windy;
   - humidity as a percentage from 0 through 100, within one percentage point of the
     source; and
   - wind speed with `km/h`, `mph`, `m/s`, or `kn`, non-negative and within the source's
     displayed rounding after unit conversion.

   Also apply broad sanity bounds of -50 through 60 °C (or -58 through 140 °F) and no more
   than 250 km/h equivalent wind. The source comparison is still mandatory: the bounds
   alone can pass by coincidence.
6. **Reject broken rendering literally.** The rendered assistant text must contain none of
   `undefined`, `NaN`, `` `${` ``, `<temperature>`, `<humidity>`, or `<wind>`,
   case-insensitively where applicable. Also reject any visibly unexpanded template
   placeholder such as `{{weather_value}}`, `{temperature}`, or `<wind>`; ordinary closing
   braces in quoted source data are not placeholders by themselves.
7. **Conversions must agree.** If both Celsius and Fahrenheit are shown for one labeled
   temperature, require `|F - (C × 9/5 + 32)| <= 1.5`. If wind is shown in more than one
   unit, convert each value to km/h and require a maximum difference of 1.5 km/h. Distinct
   values such as current and feels-like temperatures must remain distinctly labeled; do
   not compare unrelated numbers as a pair.
8. **Inspect every execution iteration in order.** From the row's scoped trace and settled
   transcript, save each completed code execution with its ordinal, language, success,
   source, output, and exact error. Collapse whitespace only for comparison. Two or more
   consecutive failed executions with the same non-empty error are a FAIL; quote the
   repeated error in the scorecard. Do not discard failed iterations once a later one
   succeeds.
9. **Each row proves its own identity.** The rendered dialect badge,
   `/api/state.settings.rlm_dialect`, code-block language, and execution events must all
   name the row's dialect. Record the served provider model from the row's model-call
   evidence. A mismatch or unrecorded model substitution is a mislabeled row and triggers
   Abort/RCA.
10. **Run Lashlang before TypeScript, with no shared state.** Use the fixed row allocations
    below and tear the Lashlang instance down completely before booting TypeScript. Each row
    gets its own session id, data directory, run directory, artifacts, ports, Restate and
    Postgres containers, and trace.

## Working material

Require non-empty `OPENROUTER_API_KEY` and `TAVILY_API_KEY` from the repository's ignored
`.env`; missing credentials are a harness gap → Abort. Set
`OPENROUTER_MODEL=openai/gpt-5.6-sol` and `AGENT_WORKBENCH_OPEN=0` for both rows. Never log
credential values.

Use these allocations exactly; they are intentionally explicit rather than relying on the
workbench port-derivation fallback:

| Dialect | Workbench | Restate endpoint | ingress | admin | node | Postgres | Session |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| `lashlang` | 3301 | 11791 | 10790 | 21780 | 21781 | 18142 | `fig2351-weather-lashlang-<run-id>` |
| `typescript` | 3302 | 11801 | 10800 | 21790 | 21791 | 18152 | `fig2351-weather-typescript-<run-id>` |

For each row export all of the following with that table's values before
`just agent-workbench <workbench-port>`:

- `LASH_RUNBOOK_DIALECT=<dialect>`;
- `AGENT_WORKBENCH_DATA_DIR=/workspace/tmp/fig2351-weather/<run-id>/<dialect>/data`;
- `AGENT_WORKBENCH_RUN_DIR=/workspace/tmp/fig2351-weather/<run-id>/<dialect>/run`;
- `AGENT_WORKBENCH_RESTATE_ADDR=127.0.0.1:<endpoint>`;
- `RESTATE_INGRESS_URL=http://127.0.0.1:<ingress>`;
- `RESTATE_ADMIN_URL=http://127.0.0.1:<admin>`;
- `AGENT_WORKBENCH_RESTATE_ADMIN_PORT=<admin>` and
  `AGENT_WORKBENCH_RESTATE_NODE_PORT=<node>`;
- `AGENT_WORKBENCH_RESTATE_ENDPOINT_URL=http://127.0.0.1:<endpoint>`;
- `AGENT_WORKBENCH_RESTATE_CONTAINER=lash-agent-workbench-fig2351-<dialect>-restate`;
- `AGENT_WORKBENCH_POSTGRES=1`, `AGENT_WORKBENCH_POSTGRES_PORT=<postgres>`, and
  `AGENT_WORKBENCH_DATABASE_URL=postgres://lash:lash@127.0.0.1:<postgres>/lash`;
- `AGENT_WORKBENCH_POSTGRES_CONTAINER=lash-agent-workbench-fig2351-<dialect>-postgres`.

Before boot, require the row's data/run/artifact directories not to exist and all six
allocated ports to be free. Never inspect, stop, or reuse anything on ports 3063 or 3067.
Save evidence under
`/workspace/tmp/fig2351-weather/<run-id>/<dialect>/artifacts`. Teardown each row with the
same exported environment and `just agent-workbench-down <workbench-port>`; then require
the workbench port closed and both exact containers absent. Preserve artifacts, but remove
the row's data and run directories only after evidence has been copied.

Browser truth is the scoped page
`/?session_id=<session-id>`, especially the rendered dialect badge, running/idle pill,
`#timeline .message.user`, `#timeline .message.assistant`, completed/failed code blocks,
and nested web-tool rows. HTTP truth is `/healthz` plus
`/api/state?session_id=<session-id>`. Durable truth is the row's non-tombstoned
`lash_graph_nodes` ancestry in its dedicated Postgres container. Trace truth is the row's
`data/trace.jsonl` and `data/lashlang-execution.jsonl`, filtered by the exact session id
even for the TypeScript row (the historical filename is not a language claim).
Compare assistant text without conflating Markdown bytes with visible text: API and durable
message Markdown must agree byte-for-byte, while the DOM must equal that Markdown after the
page's own `renderMarkdownBlocks` projection.

## Phase 0 — Boot and prove the fresh row

Do: boot the row, poll `/healthz` to 200, then open its scoped page with
`wait_until="domcontentloaded"` and explicit waiting assertions.

Expect: the composer is visible; the rendered session id and dialect equal the scoped id
and row; `/api/state.settings` agrees; `web_configured == true`; the transcript is empty;
the page is idle; the API has no active turns; the dedicated Postgres store has no graph
rows for the session; and the trace has no turn, code-execution, or tool-call record for the
session. Record the configured model from state, but treat the served-model evidence after
the turn as authoritative. Save `00-ready.png`, `00-state.json`, `00-identities.json`, and
`00-trace.json`.

## Phase 1 — Ask the unassisted weather question

Do: record the trace boundary, submit exactly
`What is the current weather in Utrecht, Netherlands?`, and require the running pill plus
one active turn before waiting for completion.

Expect: within the five-minute polling budget, golden rule 3's three terminal gates all
hold. Scroll the timeline to its newest row. Require exactly one rendered user row with the
question and one settled assistant row, then capture `01-finished.png`. Save the settled
page text and state as `01-finished-dom.json` and `01-finished-state.json`, and the row's
filtered post-boundary trace as `01-finished-trace.json`.

## Phase 2 — Reconcile execution history and live sources

Do: order all of this turn's code execution completions and nested tool activity by trace
position. Save them as `02-execution-history.json`; save every successful web result used
by the answer, without credentials, as `02-live-sources.json`.

Expect: every code block and execution event names the active dialect; every agent tool is
on the validation-only allow-list; at least one successful web result identifies Utrecht
and supports all four answer facts; and golden rule 8 finds no repeated-identical-error
loop. If it does, stop and quote the exact repeated error rather than continuing to judge
the answer.

## Phase 3 — Judge the rendered answer and cross-check layers

Do: extract the rendered answer's temperature, condition, humidity, and wind into
`03-answer-values.json`, alongside the matching source values, units, conversions,
differences, bounds, and placeholder scan. Keep the raw rendered answer in the same file.

Expect: golden rules 4 through 7 all pass. Then perform the RULES.md three-layer
cross-check: the DOM, `/api/state`, and active durable graph ancestry contain the same one
user/assistant pair by identity and rendered text; the trace contains one matching
completed turn. Any disagreement is a contract violation → Abort/RCA. Save
`03-crosscheck.json` and screenshot the fully scrolled answer as
`03-weather-answer.png`.

## Phase 4 — Teardown and score

Do: tear down the row using its exact exported environment before moving to the next
dialect. Confirm the PID is gone, all allocated ports are closed, and both exact containers
are absent. Save a concise teardown transcript as `04-teardown.txt`.

Expect: no workbench-owned process, listener, or container from the row remains.

Repeat Phases 0 through 4 for TypeScript only after Lashlang teardown passes.

### Per-dialect scorecard

Copy this table once for each dialect and fill every Result and Evidence cell. A specific
rendered value or exact failure replaces a generic “looks good.”

| Gate | Objective gate | Result | Evidence |
| --- | --- | --- | --- |
| Fresh isolated boot | six explicit ports free before boot; scoped DOM/API/store/trace are empty and agree | | `00-*` |
| Dialect and model identity | badge, state, code/execution events name the row; served model recorded | | `00-identities.json`, `01-finished-trace.json` |
| Do → expect completion | running observed; then idle + no active turn + one completed/final-value terminal within five minutes | | `01-finished.png`, state, trace |
| Validation-only tools | every agent tool is `web.search` or `web.fetch` | | `02-execution-history.json` |
| Live source support | successful Utrecht current-condition result supports temperature, condition, humidity, and wind | | `02-live-sources.json` |
| No repeated-identical-error loop | no two consecutive failed executions share a non-empty error; otherwise quote it here | | `02-execution-history.json` |
| Rendered weather shape | concrete temperature/unit, source condition, humidity, and wind/unit; no broken placeholder token | | `03-weather-answer.png`, `03-answer-values.json` |
| Internal consistency | source rounding, sanity bounds, and all displayed unit conversions agree | | `03-answer-values.json` |
| Three-layer fidelity | one user/assistant pair agrees by identity and text across DOM, API, durable graph, and trace | | `03-crosscheck.json` |
| Teardown | exact PID, ports, and containers are gone | | `04-teardown.txt` |

### Matrix summary

| Dialect | Verdict | Rendered answer or exact failing error | Evidence directory |
| --- | --- | --- | --- |
| `lashlang` | | | |
| `typescript` | | | |

**Aggregate:** did both fresh dialect sessions independently search live Utrecht weather,
parse the returned values, finish within budget, and render a source-backed, internally
consistent answer without placeholders or an identical-error retry loop?

## Notes

- **FIG-2350 linkage.** The known TypeScript defect shape is a bound variable whose preview
  appears useful while its value remains a string of dictionary-like page text; later code
  reads `.localtime` from `undefined` and repeats the same failed execution. If the
  TypeScript row reproduces that shape, quote its exact error, mark the row FAIL, and link
  the finding to FIG-2350. Do not add parsing hints, increase the iteration budget, or
  weaken any gate to make the row pass.
- FIG-2352 owns the deterministic benchmark for this behavior. This judged runbook does
  not duplicate that harness or turn its semantic gates into a script.

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
