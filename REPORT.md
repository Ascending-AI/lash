# FIG-1306 implementation report

## Verdict

**COMPLETE.** The TypeScript RLM dialect has a typed, durable, create-time
selection path; a production host-API prompt whose claims are checked against
the dialect's own behaviour; full judged-runbook parity with Lashlang;
registered dual-dialect examples; and an executed first-shot fluency corpus that
now reaches every behaviour the prompt advertises. The default remains Lashlang,
unknown dialects fail closed at creation, and a resumed session uses its
persisted dialect rather than re-deriving one. Subagent sessions inherit their
parent's dialect, so a session tree is one dialect in v1.

Three review rounds landed on this branch: two independent adversarial reviews
(APPROVE-WITH-FIXES) and one fresh-eyes final verification that returned
**BLOCK**. The blocker was not in the durability machinery — that survived every
attack both rounds threw at it — but in the harness twenty-one of the
thirty-two scenarios run on: **the Agent Workbench never actually served
TypeScript.** That is closed, together with the four smaller findings from the
same review. The mechanism the fix converged on is described below, because it
changed what the operator rules have to say.

The live model-calling rows are **PREPARED / NOT RUN**. The runbook rules require
provider credentials and an explicit go before incurring model/judge cost; none
was supplied. No credential was read, no live provider was called, and no
substitution was made.

- Branch: `samuel-fig-1306`
- Base: `dc191172e` (the FIG-1305 head this layer stacks on)
- Final head: the report commit at the branch tip
- Push: not performed

## The dialect mechanism, as it now stands

Three facts, and the third is the one every earlier revision of this report got
wrong.

1. **The choice is asked for at open; it becomes durable at the session's first
   commit.** Opening a session does not persist protocol turn options. A session
   opened and dropped before any turn commits has recorded nothing, and the next
   open may choose again.
2. **A recorded dialect always wins, silently.** `resolve_rlm_session_dialect`
   refuses only when an open *requests* a dialect different from the one
   recorded; both hosts catch that refusal and reopen with the recorded value.
3. **Therefore an open that runs a turn must carry the ambient dialect.** An
   open that asks for nothing, on a session that has recorded nothing, resolves
   the default and commits `lashlang` permanently.

### The converged-hosts ruling

Both example hosts now implement exactly one mechanism: **ask for the ambient
dialect on every session open, and accept the recorded pin when there is one.**

The earlier "apply at creation only" refinement in the workbench looked tighter
and was wrong: `creating_session_builder` was called from the boot path and from
`reset_chat`, and *both* of those open and drop a handle without running a turn.
The pin evaporated with the handle, and the first real turn — opening with no
dialect, finding nothing recorded — committed Lashlang. `agent-service` was
already correct for the opposite reason: it passed the dialect on every open and
fell back only on a pin conflict, which is precisely the mechanism the workbench
now uses too.

Observably the behaviour is still create-only: the ambient value can only take
effect on a session that has recorded nothing. What changed is that "creation"
now means the open that commits, not the open that constructs.

### The hang discovery

The pre-fix defect did not look like a failing assertion. With a TypeScript
`AppState` the workbench served a *Lashlang* prompt while the fixture's provider
answered — as a TypeScript-configured deployment's model would — with a
`<typescript>` cell. The session cannot execute a cell of a dialect it is not
running, so the turn never reached a terminal state: **the turn hung.** Reverting
the fix reproduces it; the fixture loops rather than failing.

That matters beyond this bug. In production the same shape is worse than a hang:
the model would follow the Lashlang prompt it was actually given, the turn would
succeed, and the row's whole evidence bundle would be labelled `typescript` and
produced by Lashlang. That is the defect class this layer exists to close —
"evidence that disagrees with its own label" — which is why the two new fixtures
assert *both* halves: the prompt the served turn actually received, and the
dialect the session recorded.

### What the operator rules had to change

`runbooks/RULES.md` told the operator that reopening under a different
`LASH_RUNBOOK_DIALECT` is refused, and that a carried-over store fails every
route. After the convergence neither host refuses: a carried-over store stays
green and quietly serves the *recorded* dialect. Read literally, the old text
made green routes read as evidence of a clean store — wrong in the dangerous
direction. The paragraph now states the mechanism and keeps the
fresh-data-directory mandate with its real reason: the pin wins silently, so a
stale store yields a bundle labelled with one dialect and produced by the other,
and the served dialect must be confirmed from the row's own evidence rather than
from the environment.

## Delivered contract

### Typed and durable dialect selection

- `RlmDialect::{Lashlang, Typescript}` is the public typed selection contract;
  its serialized language ids are `lashlang` and `typescript`.
- `RlmCreateExtras::dialect` is optional. Absence selects the ratified Lashlang
  default; serde's deny-unknown contract remains intact.
- `RlmSessionBuilderExt::rlm_dialect` is the public host-facing builder path.
  Agent Workbench and `agent-service` accept `LASH_RUNBOOK_DIALECT`, use the
  typed builder, default to Lashlang, and reject unknown values.
- The selected language id is written into durable protocol state. Rehydration
  treats that state as authoritative and rejects a create-time mismatch.
- Tampered durable state fails closed in every shape: an unknown id, a
  case-drifted id, a junk extra key, and an explicit `null`. Absence remains
  Lashlang, because that is how every pre-layer session decodes.
- Removing the key entirely is the one residual window, and it is bounded: it is
  indistinguishable from a pre-layer session, so it must read as Lashlang, and
  once the session has execution state the RLM snapshot's `engine` id is a
  second source of truth that refuses disagreement
  (`RlmSnapshotError::EngineMismatch`).
- A per-turn `protocol_turn_options` override — public host surface over a
  shallow key merge — cannot re-point a pinned session. The commit persists
  session-level options, not the turn-scoped merge. This is now pinned by a test
  that first proves the override really is carried by the turn.
- Agent Workbench opens every production RLM session through the typed path with
  the ambient dialect, and has fixtures on the TypeScript branch.

### Production TypeScript prompt

The execution section is host documentation, not a TypeScript tutorial: cell
tags, persistent top-level bindings, `console`/`print`/`finish`, every active
tool as a generated `Promise<T>` signature, the typed `defineProcess` / `start`
/ `sleep` / `waitSignal` / `wake` / `registerTrigger` primitives, durable
suspension and `Promise.all` / `Promise.allSettled` first-settled semantics, and
the named v1 guardrails a model actually hits.

`waitSignal` now carries its scope: it is the one primitive in that block that is
process-body-only, and the declaration says so rather than leaving the model to
infer a rule from prose that treated it exactly like `sleep`.

The rendered prompt is pinned at
`crates/lash-protocol-rlm/src/dialect/snapshots/lash_protocol_rlm__dialect__typescript__tests__typescript_execution_section.snap`.
The Lashlang execution-section snapshot remains unchanged.

## Prompt honesty: two checks, two directions

The prompt is prose, and prose is not type-checked. Two tests cover it, in
opposite directions:

- **Codes named in the prompt must exist.** A walker collects every `TS_` token
  in the rendered prompt and rejects any that is not in `DiagnosticCode::ALL`.
  This is what closed the phantom `TS_FOR_OF_ITERATOR_UNSUPPORTED`.
- **Diagnostics the prompt's primitives emit must be spelled in this dialect.**
  The walker matches codes, not identifiers, so it could not see that misusing
  `waitSignal` rejected with "`wait_signal` can only be used inside a process
  body" — a Lashlang identifier the TypeScript reader has never seen. The second
  check misuses every primitive the Host API block declares and fails if the
  resulting model-facing message names a Lashlang-only spelling. It found exactly
  one leak, fixed at its source: linking already knows which surface dialect it
  was handed, so the process-only refusal now names the keyword the author
  actually wrote.

## Judged-runbook parity

`runbooks/parity-matrix.toml` is the machine-readable authority. It expands to
**65 independent rows**: 32 Lashlang rows, the same 32 TypeScript rows, and one
TypeScript-only composite acceptance row. `scripts/judged_runbook_matrix.py`
validates the files and emits stable `I/N` shards as JSON. Both matrix
properties — complete twin coverage and lossless sharding — are mutation-earned:
a duplicated scenario and a lossy shard were green before this round and are red
now, and CI runs the gate.

Every RLM and judge step has a `gpt-5.6-sol` floor. Independent rows may execute
concurrently; judging is sharded; evidence must be fresh; provider-equivalent
substitutions must be recorded per row. No substitutions were used here.

Twenty of the thirty-two ordinary scenarios are `workbench-*`, and the
TypeScript-only composite row boots the Workbench too — twenty-one rows in all
whose TypeScript evidence depended on the blocker above. None of them was run, so
no mislabeled evidence was produced; the fix precedes the battery.

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
codemode turn demonstrating `Promise.all` first-settled behaviour, a pure
`for...of` loop over tool-returned data, a `defineProcess` signal/sleep
lifecycle, an actual worker restart while suspended, and continuation of the same
durable process after restart.

## Examples, documentation, and API coverage

- `examples/codemode-parity/` contains paired Lashlang and TypeScript codemode
  turns and paired durable-process signal/sleep examples.
- `examples/docs-snippets` executes explicit TypeScript selection through the
  public builder and inspects the persisted session payload.
- `docs/rlm.html`, `docs/embedding-turns.html`, and `docs/execution-modes.html`
  document host selection and the durable default.
- `docs/api-example-coverage.toml` registers the new enum, variants,
  `language_id`, and builder method against executable examples; low-level
  factory plumbing is explicitly justified rather than presented as host API.
  The registry gate covers 8,114 entries and passes.

## Executed fluency result

`crates/lash-typescript/tests/fluency_smoke.rs` compiles, links and executes
eight first-shot programs: awaited `Promise` tools, `Object.entries` data
shaping, string/array iteration, `for...of` over tool-returned data with a
helper call in the body, `map` with one- and two-parameter callbacks, the
journaled `Date.now()` / `Math.random()` reads, and a `registerTrigger`
registration.

The last two rows are new this round, and between them they cover three
behaviours the corpus could not reach at all. The corpus previously built a **bare** host
catalog, so it could not reach them at all: they are host operations, not
language builtins — the real RLM host adds the `__typescript_runtime`
`now`/`random` bindings and, when triggers are enabled, the trigger resource
operations. Three behaviours the production prompt advertises were therefore
unmeasured by the corpus whose job is to prove the prompt honest. The environment
now mirrors the real host's, and dropping either binding back out puts both new
rows on the hit list.

The process row remains narrower than the others by construction: `start` and
`await` cross the effect boundary, so it shows the primitives lower, link and
round-trip, not that `waitSignal`/`sleep`/`wake` execute. Those execute under
suspension in `dialect.rs::a_process_suspended_inside_for_of_resumes` and
`agent_surface.rs`.

The corpus also carries one row that must be rejected, asserted to produce
exactly one hit naming `TS_DESTRUCTURING_UNSUPPORTED` — an empty hit list is
evidence only if the list can fill.

Missing-method/rejection hit list:

```json
[]
```

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
| `a045b3c0e` | Regenerate the report after the dual-review round | Internal |
| `2f1cb3fc0` | Ask for the ambient dialect on every workbench open (**F1**) | Fixed |
| `7fe7c6130` | Name the process-only primitive in the reader's own dialect (**F3**) | Fixed |
| `880caac06` | Describe the reopen rule the hosts actually implement (**F2**) | Internal |
| `a553ff101` | Give the fluency corpus the host bindings it was missing (**F4**) | Internal |
| `0292ac006` | Pin that a per-turn override cannot re-point the dialect (**F5**) | Internal |
| `77c67d938` | Clear the CI-only gates the F1 fix left red | Internal |
| final report commit | Regenerate this report against the branch as it stands | Internal |

## Review ledger

### Fresh-eyes final verification (BLOCK) — this round

| Finding | What it was | Red | Fix |
|---|---|---|---|
| F1 (Blocker) | The Agent Workbench never served TypeScript. Only the two opens that never run a turn applied `LASH_RUNBOOK_DIALECT`; every turn-running open asked for nothing, so the first turn committed Lashlang. Twenty-one scenarios would have produced Lashlang evidence under a TypeScript label — and the pre-fix symptom is a **hung turn**, not a failing one | fixture hangs pre-fix | `2f1cb3fc0` |
| F2 (Fix) | `runbooks/RULES.md` claimed reopen under the other dialect is refused and fails every route. Neither host refuses after the convergence, so green routes read as a clean store | — | `880caac06` |
| F3 (Fix) | `waitSignal` was declared with no scope annotation beside `sleep`, which is not process-only; and its refusal named the Lashlang identifier `wait_signal`, which the `TS_`-token walker cannot see | `7fe7c6130` (test written first: one leak) | `7fe7c6130` |
| F4 (Note) | The fluency corpus's bare catalog could not reach `Date.now`, `Math.random` or `registerTrigger`, all three named in the production prompt | binding-removal mutation | `a553ff101` |
| F5 (Note) | A per-turn `protocol_turn_options` override is a write to protocol state that bypasses create-time resolution. It does not land, but nothing pinned that | turn-merge-persist mutation | `0292ac006` |

Findings the review verified as **correct and left alone**: the durable selection
attack surface (eleven shapes, including pre-layer `{}`, explicit `null`,
unregistered `python`, reopen precedence and resume), subagent inheritance in
both directions, the unconstructibility of a mixed tree through that seam, every
advertised static and instance method linking and executing (39 + 34 probes, zero
advertised-but-broken), all eleven named guardrail codes rejecting with exactly
their named code, the `start` calling convention, the reachability of the
runbook's `TS_FOR_OF_UNSUPPORTED` gate, and the `+17`-byte-per-golden MessagePack
cost of the new always-written key.

### Dual adversarial review (APPROVE-WITH-FIXES) — prior round

| Finding | What it was | Fix |
|---|---|---|
| A-F1 / B-F1 (High) | The prompt, its pinning assertion, the snapshot, the runbook's Phase 2 gate and the README all named `TS_FOR_OF_ITERATOR_UNSUPPORTED`, a code that never existed | `dd8181723` (red: `4343666c4`) |
| A-F3 / B-F2 (High) | `start`'s declared second argument was an options bag with an `input` field; the lowerer passes the object's keys through as the process's parameter names | `dd8181723` |
| B-F3 (Medium) | The same sentence over-stated the `for...of` guard, suppressing legal code | `dd8181723` |
| A-F4 (Medium) | The guardrail list omitted the five shapes a model reaches for first | `dd8181723` |
| B-F6 (Medium) | The fluency corpus had no `for...of`, no `map`, and no negative control | `dd8181723` |
| A-F10 (Low) | The corpus claimed to execute durable process primitives it cannot drive from a cell | `dd8181723` |
| A-F2 (High) | The twin-coverage gate the parity claim rests on was run by nothing | `db6eff1c9` |
| A-F8 / B-F4 (Medium) | A duplicated scenario passed while the script emitted 67 rows; the shard test re-implemented the split | `db6eff1c9` |
| A-F11 (Nit) | The shard test never drove the script's own selection or argument parsing | `db6eff1c9` |
| B-F5 (Medium) | The cross-dialect fence had no test: deleting it kept the package green | `db6eff1c9` |
| A-F6 / B-F7 (Medium) | A TypeScript parent spawned Lashlang subagents | `191570864` |
| A-F5 / B-F8 (Medium) | Both hosts re-asserted the ambient dialect on every open, one direction failing every route | `c60c62866`, superseded by `2f1cb3fc0` |
| B-F10 (Low) | The workbench's dialect was a `cfg(test)` fork no test could reach | `c60c62866`; the branch was actually reached for the first time in `2f1cb3fc0` |
| B-F9 / A-F9 (Low) | An explicit `null` dialect did not fail closed | `c60c62866` |
| A-F7 (Low-Medium) | Docs said the choice is made "at creation"; it becomes durable at the first commit | `c60c62866` |
| A-F12 / B-F11 (Nit) | The report's base and commit table described a pre-rebase history | `a045b3c0e` |

Note on `c60c62866`: its fix for A-F5/B-F8 is the refinement that introduced F1.
It is listed as closed because the finding is closed — by `2f1cb3fc0`, on the
mechanism both hosts now share.

### Honest ledger

Things a reader should not have to dig for:

- **`c60c62866` made the workbench worse.** It closed a real finding with a
  refinement that silenced the dialect entirely on every turn-running open. One
  review round passed over it; the fresh-eyes round caught it. The fixtures that
  would have caught it at the time are now in the tree.
- **No workbench test had ever constructed a TypeScript `AppState`.** All
  seventeen fixtures set `RlmDialect::Lashlang`, and the field's own doc comment
  justified itself as existing so that a test *could* reach the TypeScript
  branch. It was never reached until this round.
- **Two CI-only gates were red at the previous head** and are fixed in
  `77c67d938`: clippy under `-D warnings` (three needless borrows introduced by
  routing opens through `open_session(&str)`) and the API example-coverage
  registry (124 anchors pointing at pre-shift line numbers). Neither is visible
  to the pre-commit hooks, and the round that introduced them reported green.
- **Three prompt-advertised behaviours were unmeasured by the corpus that exists
  to measure the prompt** (F4). The prompt was honest about them — the reviewer
  verified all three link and execute with bindings present — but the corpus
  could not have told us.
- **One model-facing internal alias remains.** Against a catalog without the
  runtime bindings the message is "unknown module `__typescript_runtime`" — an
  internal name in a string a model can see. It is unreachable in production (the
  real host always installs those bindings) and is out of this round's scope, but
  it is the same class as F3 and is recorded rather than dropped.
- **The live 65-row judged battery has not been run.** Every parity claim in this
  report is a claim about the harness, not about a judged result.

## Verification

All Cargo commands used `CARGO_TARGET_DIR=/workspace/.cargo-target-lash-fig-1306`,
with no other heavy build job running concurrently.

| Gate | Result |
|---|---|
| `cargo fmt --all --check` | PASS |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | PASS (exit 0) after `77c67d938`; red before it |
| `cargo nextest run -p agent-workbench -p agent-service -p lash-protocol-rlm -p lash-typescript --locked` | PASS; 656 tests run, 656 passed, 13 skipped |
| `cargo test -p lash-runtime --features rlm --lib` | PASS; 230 tests. Run explicitly because the RLM-feature tests, including the new per-turn-override pin, are `cfg(feature = "rlm")` |
| `cargo test --workspace --locked` | PASS (exit 0); the canonical final workspace run, executed alone with nothing else building. The `durable_fault_matrix` test that shells out to a nested `cargo` build did not trip this time |
| `python3 scripts/check_api_example_coverage.py` | PASS (exit 0); 8,114 entries, after re-anchoring 124 references |
| `python3 scripts/lint_docs.py` | PASS; 46 HTML and 42 registry pages |
| `python3 scripts/test_judged_runbook_matrix.py` | PASS; 4 tests |
| `python3 scripts/judged_runbook_matrix.py --shard 1/1` | PASS; 65 validated rows |
| `python3 scripts/release_notes.py check-pr --range dc191172e..HEAD` | PASS (exit 0) |
| Live 65-row judged battery | PREPARED / NOT RUN; no provider-cost authorization |

### Red-side evidence this round

Every fix was preceded or accompanied by a demonstration that the check fails
without it.

| Item | Red-side demonstration |
|---|---|
| F1 | Re-verified this round. With the pre-fix shape restored (turn-running opens asking for no dialect), `a_typescript_workbench_serves_typescript_turns_and_records_the_dialect` never completes — killed at a 420s timeout — because the served prompt is Lashlang, the provider answers with a `<typescript>` cell, and the turn cannot reach a terminal state. On the fixed tree the same test passes in 0.08s |
| F3 | The structural check was written first and reported exactly one leak: "waitSignal at top level: names `wait_signal`" |
| F4 | Removing the `__typescript_runtime` bindings puts row 7 on the hit list ("does not expose operation `random`"); removing `add_trigger_resource_operations` puts row 8 on it ("unknown module `triggers`") |
| F5 | Persisting the turn-scoped merge as session state turns the test red immediately, with the session rejecting its own snapshot engine ("RLM snapshot engine `typescript` is unsupported") |

The prior round's mutations still hold: reverting subagent inheritance, deleting
the cross-dialect fence, unwiring a CI self-test entry, dropping a code from
`DiagnosticCode::ALL`, duplicating a matrix scenario and skewing `select_shard`
each turn the relevant test red.

## Repository state

- Conventional commits with categorized `Release-Notes:` footers and no AI
  attribution.
- No live provider credentials were consumed.
- No push was performed.
