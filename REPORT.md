# FIG-1306 implementation report

## Verdict

**COMPLETE.** The TypeScript RLM dialect now has a typed, durable,
create-time selection path; a concise production host-API prompt; full judged
runbook parity with Lashlang; registered dual-dialect examples; and an executed
first-shot fluency corpus. The default remains Lashlang, unknown dialects fail
closed at creation, and a resumed session uses its persisted dialect rather than
re-deriving one from process configuration.

The live model-calling rows are **PREPARED / NOT RUN**. The runbook rules require
provider credentials and an explicit go before incurring model/judge cost; none
was supplied with this implementation request. No credential was read, no live
provider was called, and no substitution was made. This is the preparation path
the implementation spec explicitly permits.

- Branch: `samuel-fig-1306`
- Base: `4f33a8f5a`
- Implementation head before this report: `c672ed010`
- Final head: the report commit at the branch tip
- Push: not performed

## Delivered contract

### Typed and durable dialect selection

- `RlmDialect::{Lashlang, Typescript}` is the public typed selection contract.
  Its serialized language ids are `lashlang` and `typescript`.
- `RlmCreateExtras::dialect` is optional. Absence selects the ratified Lashlang
  default; serde's deny-unknown contract remains intact.
- `RlmSessionBuilderExt::rlm_dialect` provides the public host-facing builder
  path. Agent Workbench and agent-service accept `LASH_RUNBOOK_DIALECT`, use the
  typed builder, default to Lashlang, and reject unknown values.
- The selected language id is written into durable protocol state. Rehydration
  treats that state as authoritative and rejects a create-time mismatch.
- The core plugin factory receives persisted protocol turn options before it
  constructs the session, allowing the RLM factory to instantiate the recorded
  dialect rather than consult mutable configuration.
- A production-path integration test creates a TypeScript session, executes
  `<typescript>finish(42)</typescript>` through the real executor, parks and
  resumes it, then executes a second TypeScript cell that observes the persisted
  binding and returns `43`. The resumed prompt remains TypeScript-only.
- Contract tests accept `typescript`, reject unregistered `python` at creation,
  and pin restored dialect mismatch rejection.

### Production TypeScript prompt

The execution section is deliberately host documentation, not a TypeScript
tutorial. It covers:

- cell tags, persistent top-level bindings, `console`, `print`, and `finish`;
- every active tool as a generated `Promise<T>` signature;
- typed `defineProcess`, `start`, `sleep`, `waitSignal`, `wake`, and
  `registerTrigger` host primitives;
- durable process suspension/resumption and the supported `Promise.all` /
  `Promise.allSettled` first-settled semantics;
- named v1 guardrails a model is likely to hit: classes, generators, async
  functions, mutable capture, for-of iterator aliasing, method inventory, and
  `new Date`.

The rendered prompt is pinned at:

`crates/lash-protocol-rlm/src/dialect/snapshots/lash_protocol_rlm__dialect__typescript__tests__typescript_execution_section.snap`

The existing Lashlang execution-section snapshot remains pinned and unchanged.

## Judged-runbook parity

`runbooks/parity-matrix.toml` is the machine-readable authority. It expands to
**65 independent rows**: 32 Lashlang rows, the same 32 TypeScript rows, and one
TypeScript-only composite acceptance row. `scripts/judged_runbook_matrix.py`
validates the files and emits stable `I/N` shards as JSON. The matrix tests prove
complete twin coverage and lossless sharding.

Every RLM and judge step has a `gpt-5.6-sol` floor. Independent rows may execute
concurrently; judging is sharded; evidence must be fresh; provider-equivalent
substitutions must be recorded per row. No substitutions were used here.

| Scenario | Lashlang | TypeScript |
|---|---|---|
| `agent-service-branching` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `docs-quickstart` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `graceful-drain` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `process-operations` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `request-abandon` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `rlm-cell-boundary` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `session-lease-triage` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `slack-clone-bot` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `slack-clone-mcp-client-depth` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `tictactoe-full-game` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `version-bump-recreation` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `workbench-attachments` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `workbench-break-glass` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `workbench-chat-projection` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `workbench-continue-as` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `workbench-cron-schedule` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `workbench-deferred-tools` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `workbench-durable-stop` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `workbench-engine-restart` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `workbench-execution-state-rehydration` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `workbench-failure-paths` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `workbench-inbox-world` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `workbench-process-lifecycle` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `workbench-reconnect-resilience` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `workbench-session-id-retirement` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `workbench-session-isolation` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `workbench-session-resume` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `workbench-shared-session-multi-tab` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `workbench-trigger-lifecycle` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `workbench-turn-ingress` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `workbench-usage-ledger` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `workflow-editor-authoring` | PREPARED / NOT RUN | PREPARED / NOT RUN |
| `typescript-codemode-parity` | N/A | PREPARED / NOT RUN |

The TypeScript-only row is not a reduced smoke test. It requires a judged full
codemode turn that demonstrates `Promise.all` first-settled behavior, a pure
for-of loop over tool-returned data, a `defineProcess` signal/sleep lifecycle,
an actual worker restart while suspended, and successful continuation of the
same durable process after restart. Its runbook defines evidence, screenshot,
and rejection-hit artifacts.

## Examples, documentation, and API coverage

- `examples/codemode-parity/` contains paired Lashlang and TypeScript codemode
  turns and paired durable-process signal/sleep examples.
- `examples/docs-snippets` executes explicit TypeScript selection through the
  public builder and inspects the persisted session payload.
- Agent Workbench centralizes production RLM session opening through the typed
  selection path and records the chosen dialect at startup.
- `docs/rlm.html`, `docs/embedding-turns.html`, and
  `docs/execution-modes.html` document host selection and the durable default.
- `docs/api-example-coverage.toml` registers the new enum, variants,
  `language_id`, and builder method against executable examples. Low-level
  factory plumbing is explicitly justified rather than presented as host API.
- The registry gate covers **8,114 public API entries** and passes.

## Executed fluency result

`crates/lash-typescript/tests/fluency_smoke.rs` now compiles, links, and executes
four first-shot programs covering awaited Promise tools, `Object.entries` data
shaping, common string/array iteration, and the durable-process host surface.

Missing-method/rejection hit list:

```json
[]
```

The first harness pass exposed that the test environment had not declared the
signal and sleep host abilities used by its process sample. Enabling those
existing abilities fixed the harness; it was not a missing TypeScript method or
dialect rejection, so it is not included in the hit list.

## Commits

| Commit | Purpose | Release note |
|---|---|---|
| `3014ff6d9` | Record the ruled create-time selection blocker before implementation | Internal |
| `c0f37a791` | Persist typed per-session dialect selection | Added |
| `673e63a5a` | Add the production TS prompt, examples, docs, and executed fluency | Added |
| `c672ed010` | Require complete judged-runbook parity and stable shards | Internal |
| final report commit | Replace the blocker report with final implementation evidence | Internal |

## Verification

All Cargo commands used
`CARGO_TARGET_DIR=/workspace/.cargo-target-lash-fig-1306`. No heavy build jobs
were run concurrently.

| Gate | Result |
|---|---|
| `cargo check --workspace --all-targets --locked` | PASS |
| `cargo test --workspace --locked` | PASS; canonical final workspace run, including unit, integration, property, UI, simulation, conformance, differential-oracle, schema-congruence, and doctest suites |
| relevant-package `cargo nextest run` iteration | CLOSED; 1,998 tests selected, 1,992 initially passed. Five expected durable-state size goldens were updated for the persisted `dialect` field and all 15 focused agent-scenario tests then passed. One 5-second cancellation test timed out only under suite contention and passed in 0.05 seconds in isolation. The canonical workspace run subsequently passed. |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | PASS |
| `cargo fmt --all --check` | PASS |
| `python3 scripts/check_included_file_formatting.py` | PASS; 37 included Rust files |
| `python3 scripts/lint_docs.py` | PASS; 46 HTML and 42 registry files |
| `bash scripts/check-rustdoc.sh` | PASS; 602 public items documented, 0 missing |
| `python3 scripts/check_test_quarantines.py` | PASS |
| `python3 scripts/check_api_example_coverage.py` | PASS; 8,114 entries |
| `bash scripts/check-production-file-size.sh` | PASS |
| `python3 scripts/test_judged_runbook_matrix.py` | PASS; 2 tests |
| `python3 scripts/judged_runbook_matrix.py --shard 1/1` | PASS; 65 validated rows |
| TypeScript prompt snapshot test | PASS |
| TypeScript production select/execute/park/resume test | PASS |
| TypeScript creation and restored-state selection tests | PASS; registered TypeScript accepted, unknown Python and mismatch rejected |
| TypeScript executed fluency smoke | PASS; 4 first-shot programs, empty hit list |
| `cargo test -p lash-typescript --test differential_oracle` | PASS in the canonical run |
| SQL schema congruence | PASS in the canonical run; no SQL changed |
| `python3 scripts/release_notes.py check-pr --range 4f33a8f5a..HEAD` | PASS before report; rerun against the final tip below |
| `git diff --check 4f33a8f5a..HEAD` | PASS before report; rerun against the final tip below |
| `just perf-guard` runtime leg | PASS |
| `just perf-guard` Lashlang 500-iteration leg | BASELINE-LIMITED; exits 1 with 155 budget failures. The committed report at the exact base records the same 155 failures (and 162 before its cache fix). This change did not edit Lashlang or performance budgets. |
| Live 65-row judged battery | PREPARED / NOT RUN; explicit provider-cost authorization was not supplied |

The full canonical workspace run was deliberately performed once, after the
focused nextest corrections. The performance recipe's runtime leg is green;
the Lashlang leg retains the exact failure count documented at this branch's
verified base and is recorded here rather than hidden or recalibrated.

## Repository state

- The implementation is committed as conventional commits with categorized
  `Release-Notes:` footers and no AI attribution.
- The final report is committed at the tip after final release-note and diff
  validation.
- No live provider credentials were consumed.
- No push was performed.
