# FIG-1306 implementation report

## Verdict

**COMPLETE.** The TypeScript RLM dialect now has a typed, durable,
create-time selection path; a concise production host-API prompt; full judged
runbook parity with Lashlang; registered dual-dialect examples; and an executed
first-shot fluency corpus. The default remains Lashlang, unknown dialects fail
closed at creation, and a resumed session uses its persisted dialect rather than
re-deriving one from process configuration. The choice is asked for at creation
and becomes durable at the session's first commit; a session opened and dropped
before any turn commits has recorded nothing, and the next open may choose
again. Subagent sessions inherit their parent's dialect, so a session tree is
one dialect.

Two independent adversarial reviews then returned APPROVE-WITH-FIXES. Their
findings were concentrated on the two softer axes rather than the durability
machinery: prompt honesty — a named rejection code that does not exist, a
`start()` signature contradicted by this layer's own example, a guardrail list
missing the commonest first-shot rejections — and gate wiring, including the
twin-coverage test that the parity claim rests on, which nothing ran. All are
closed below, and the four that were the second instance of their kind are
closed structurally.

The live model-calling rows are **PREPARED / NOT RUN**. The runbook rules require
provider credentials and an explicit go before incurring model/judge cost; none
was supplied with this implementation request. No credential was read, no live
provider was called, and no substitution was made. This is the preparation path
the implementation spec explicitly permits.

- Branch: `samuel-fig-1306`
- Base: `dc191172e` (the FIG-1305 head this layer stacks on)
- Final head: the report commit at the branch tip
- Push: not performed

An earlier revision of this report named a different base and pre-rebase
commit hashes. The branch was rebased onto `dc191172e` and the interim
blocker-report commit was dropped, so that table described a history the
branch no longer had.

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
- Tampered durable state fails closed in every shape: an unknown id, a
  case-drifted id, a junk extra key, and — since this round — an explicit
  `null`. Absence remains Lashlang, because that is how every pre-layer session
  decodes, so absence and `null` had to be told apart rather than merged.
- Removing the key entirely is the one residual window, and it is bounded. It
  is indistinguishable from a pre-layer session, so it must read as Lashlang.
  Once the session has any execution state the RLM snapshot's `engine` id is
  the second source of truth and disagreement is refused with
  `RlmSnapshotError::EngineMismatch`, so the window is a session that recorded
  `typescript` and has not yet produced a snapshot. Both facts are store-tamper
  only: the field is written on every commit and skipped only when absent.
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
complete twin coverage and lossless sharding — and both halves are now
earned. Before this round a duplicated scenario passed while the script emitted
67 rows, and a shard that dropped a row of 65 stayed green; both mutations are
red now, and CI runs the gate.

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

`crates/lash-typescript/tests/fluency_smoke.rs` compiles, links, and executes
six first-shot programs covering awaited Promise tools, `Object.entries` data
shaping, common string/array iteration, `for...of` over tool-returned data with
a helper call in the body, and `map` with one- and two-parameter callbacks.

The process row is narrower than the others and the earlier claim overstated
it. `start` and `await` cross the effect boundary, so the host answers them and
the process body runs in a separate durable execution a cell-level host cannot
drive: the row shows the primitives lower, link and round-trip, not that
`waitSignal`/`sleep`/`wake` execute. Those execute under suspension in
`dialect.rs::a_process_suspended_inside_for_of_resumes` and `agent_surface.rs`.

The corpus also carries one row that must be rejected. An empty hit list is
evidence only if the list can fill, and nothing previously showed that it
could; the control is asserted to produce exactly one hit, naming
`TS_DESTRUCTURING_UNSUPPORTED`.

Missing-method/rejection hit list:

```json
[]
```

The first harness pass exposed that the test environment had not declared the
signal and sleep host abilities used by its process sample. Enabling those
existing abilities fixed the harness; it was not a missing TypeScript method or
dialect rejection, so it is not included in the hit list.

## Commits

Every hash below is reachable from the branch head.

| Commit | Purpose | Release note |
|---|---|---|
| `605891d42` | Persist typed per-session dialect selection | Added |
| `84e99d34a` | Add the production TS prompt, examples, docs, and executed fluency | Added |
| `30eacb895` | Require complete judged-runbook parity and stable shards | Internal |
| `6d8500781` | Report the implementation | Internal |
| `4343666c4` | Red: walk the prompt's diagnostic codes against the real ones | — |
| `dd8181723` | Fix the phantom code, the `start` convention, and the guardrail list | Fixed |
| `db6eff1c9` | Run the parity-matrix gate in CI and make its own checks bite | — |
| `191570864` | Subagents inherit the parent session's dialect | Fixed |
| `c60c62866` | Pin the ambient dialect at creation; refuse a null dialect | Fixed |
| `11ce41bff` | Split the remote-trigger assertions out of a file at its budget | — |
| final report commit | Regenerate this report against the branch as it stands | Internal |

## Dual-review fix round

Two independent adversarial reviews returned APPROVE-WITH-FIXES with largely
overlapping findings. Every one is closed below.

| Finding | What it was | Red | Fix |
|---|---|---|---|
| A-F1 / B-F1 (High) | The prompt, the assertion pinning it, the snapshot, the runbook's Phase 2 gate and the README all named `TS_FOR_OF_ITERATOR_UNSUPPORTED`, a code that has never existed. The gate could therefore never fire, and the model was told to expect a string it would never see | `4343666c4` | `dd8181723` |
| A-F3 / B-F2 (High) | `start`'s declared second argument was an options bag with an `input` field; the lowerer passes the object's keys through as the process's parameter names. True only when the run parameter happens to be called `input` — as the old fluency row's was, and as the shipped example's is not | `dd8181723` | `dd8181723` |
| B-F3 (Medium) | The same sentence claimed any user-authored call in a `for...of` body rejects. Only mutating, aliasing or passing the iterable does; an over-stated guard suppresses legal code where the battery measures fluency | — | `dd8181723` |
| A-F4 (Medium) | The guardrail list named seven codes and omitted the five shapes a model reaches for first | — | `dd8181723` |
| B-F6 (Medium) | The fluency corpus had no `for...of`, no `map`, and no negative control, so its empty hit list was unearned | — | `dd8181723` |
| A-F10 (Low) | The corpus claimed to execute durable process primitives; the process body cannot run from a cell | — | `dd8181723` |
| A-F2 (High) | The twin-coverage gate — the whole basis of the parity claim — was run by nothing | — | `db6eff1c9` |
| A-F8 / B-F4 (Medium) | A duplicated scenario passed the coverage test while the script emitted 67 rows, and the shard test re-implemented the split instead of calling it | — | `db6eff1c9` |
| A-F11 (Nit) | The shard test never drove the script's own selection or argument parsing | — | `db6eff1c9` |
| B-F5 (Medium) | This layer's rewrite left the cross-dialect fence with no test: deleting it kept the package green while a TypeScript session ran a Lashlang cell | — | `db6eff1c9` |
| A-F6 / B-F7 (Medium) | A TypeScript parent spawned Lashlang subagents, so a row judged "typescript" produced a child prompt that said otherwise | — | `191570864` |
| A-F5 / B-F8 (Medium) | Both example hosts re-asserted the ambient dialect on every open: one direction failed every route, the other was silently ignored | — | `c60c62866` |
| B-F10 (Low) | The workbench's dialect was a `cfg(test)` fork, so no test could reach the TypeScript branch that ships | — | `c60c62866` |
| B-F9 / A-F9 (Low) | An explicit `null` dialect was the one tampered shape that did not fail closed | — | `c60c62866` |
| A-F7 (Low-Medium) | Docs said the choice is made "at creation"; it becomes durable at the first commit | — | `c60c62866` |
| A-F12 / B-F11 (Nit) | This report's base and commit table described a pre-rebase history | — | this commit |

### What the fixes are worth

Four of these were closed structurally rather than one instance at a time,
because the instance was the second of its kind:

- The prompt names codes in prose, which nothing type-checks. A test now walks
  every `TS_` token in the rendered prompt against `DiagnosticCode::ALL`, so
  the class is closed, not the one name. `ALL` is kept complete by a pin that
  reads the enum's own declaration; dropping a variant from it turns that pin
  red.
- The `start` rule is pinned to the lowerer's behaviour, not to the sentence:
  the test links a process whose run parameter is domain-named and asserts the
  prompt's own claim against what actually links and what actually rejects.
- CI's self-test list is enumerated by hand, which is how the matrix gate came
  to exist without ever running. A discovery test now asserts every
  `scripts/test_*.py` is wired; unwiring one turns it red.
- The register/allowlist drift that produced the phantom code has the same
  shape as a count restated in prose. Both matrix weaknesses were confirmed by
  mutation before and after: a duplicated scenario and a lossy shard were green
  before this round and are red now.

Every fix in this round was verified by mutation, not by observing a green
test: reverting the subagent inheritance, deleting the cross-dialect fence,
unwiring the CI entry, dropping a code from `ALL`, and duplicating a matrix
scenario each turn the relevant test red.

## Verification

All Cargo commands used
`CARGO_TARGET_DIR=/workspace/.cargo-target-lash-fig-1306`. No heavy build jobs
were run concurrently.

One note on the canonical run, so its history is not read as a clean single
pass. The first attempt in this round failed one test —
`durable_fault_matrix_fast_gate_executes_all_nonblocked_evidence` — with
`real cargo list probe failed`. That test shells out to a real nested `cargo`
build, so it competes with the outer run for the same target directory. It
passes on its own (245s, exit 0), and the canonical run was repeated once its
artifacts were warm. The row below records the repeated run.

| Gate | Result |
|---|---|
| `cargo check --workspace --all-targets --locked` | PASS |
| `cargo test --workspace --locked` | PASS; 251 suites, 0 failures — canonical final workspace run, including unit, integration, property, UI, simulation, conformance, differential-oracle, schema-congruence, and doctest suites |
| `python3 scripts/test_confidence_gate_ci_contract.py` | PASS; 40 tests, including the new self-test wiring check |
| `cargo nextest run -p lash-protocol-rlm -p lash-typescript -p lash-rlm-types -p lash-subagents -p docs-snippets` | PASS; 567 tests, re-run after the fix round |
| `cargo nextest run -p agent-workbench -p agent-service` | PASS; 152 tests |
| relevant-package `cargo nextest run` iteration | CLOSED; 1,998 tests selected, 1,992 initially passed. Five expected durable-state size goldens were updated for the persisted `dialect` field and all 15 focused agent-scenario tests then passed. One 5-second cancellation test timed out only under suite contention and passed in 0.05 seconds in isolation. The canonical workspace run subsequently passed. |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | PASS (exit 0), re-run after the fix round |
| `cargo fmt --all --check` | PASS |
| `python3 scripts/check_included_file_formatting.py` | PASS; 37 included Rust files |
| `python3 scripts/lint_docs.py` | PASS; 46 HTML and 42 registry files |
| `bash scripts/check-rustdoc.sh` | PASS; 602 public items documented, 0 missing |
| `python3 scripts/check_test_quarantines.py` | PASS |
| `python3 scripts/check_api_example_coverage.py` | PASS; 8,114 entries |
| `bash scripts/check-production-file-size.sh` | PASS |
| `python3 scripts/test_judged_runbook_matrix.py` | PASS; 4 tests, and CI runs it now |
| `python3 scripts/judged_runbook_matrix.py --shard 1/1` | PASS; 65 validated rows |
| TypeScript prompt snapshot test | PASS |
| TypeScript production select/execute/park/resume test | PASS |
| TypeScript creation and restored-state selection tests | PASS; registered TypeScript accepted, unknown Python and mismatch rejected |
| TypeScript executed fluency smoke | PASS; 6 first-shot programs with an empty hit list, plus a rejected control asserted to produce exactly one hit |
| `cargo test -p lash-typescript --test differential_oracle` | PASS in the canonical run |
| SQL schema congruence | PASS in the canonical run; no SQL changed |
| `python3 scripts/release_notes.py check-pr --range dc191172e..HEAD` | PASS (exit 0) |
| `git diff --check dc191172e..HEAD` | PASS |
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
