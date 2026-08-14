# FIG-1305 implementation and verification report

Branch: `samuel-fig-1305`

Base: `c03deb8d56a9c552c3250a153ab0e0ea0de195b7` (the reviewed FIG-1304 head)

Implementation commits:

- `3862c50f8ed7e7ce21d2cf6f3713f20bbb16ebb2` — `feat(typescript): add the durable agent surface`
- `82471eb2852f536982e07066df9bee37d830541d` — `fix(typescript): close durable agent review gaps`

## Verdict

**BLOCK / NOT READY TO STACK.**

The TypeScript agent surface, durability paths, standard-library inventory,
resource bounds, and repository gates are implemented and green. One ECMA-262
release blocker remains: `Promise.all` observes aggregate results only after
the entire shared resource-operation batch settles, then selects an error in
input order. JavaScript requires rejection at the first-settled failure. This
cannot be repaired truthfully in the TypeScript lowerer because the shared
aggregate host contract does not expose completion order or early settlement.
The required shared-runtime change is described under
[Remaining release blocker](#remaining-release-blocker).

No semantic deviation is being accepted for that behavior. The dialect README
also labels it as an open FIG-1305 release blocker.

## Delivered surface

### Durable TypeScript agents

- Top-level, statically extractable
  `defineProcess({ name, signals, run: async (...) => { ... } })` declarations.
  Dynamic definitions, process targets, and signal names fail with stable
  `TS_PROCESS_*` diagnostics.
- `start`, `registerTrigger`, one-argument progress `wake`, three-argument named
  signal `wake`, `waitSignal`, `sleep`, and cell-only `finish` lower through the
  existing process and effect opcodes.
- A process `return` is a real function return. Enclosing `finally` blocks run
  before the generated process wrapper finishes the returned value. `finish`
  inside `run` is rejected, so authored process code cannot bypass `finally`.
  An uncaught `throw` fails the process.
- Stored process artifacts retain `CompilationDialect::Typescript`. The linked
  program and compiled-process caches include the dialect and module identity,
  preventing a TypeScript artifact from compiling or resuming as Lashlang.
- TypeScript deferred tool resolution is enabled. Rendered tool signatures use
  `Promise<T>`, and the first-party catalog emits explicit `typescript.tool`
  bindings alongside the Lashlang bindings.

### Await, effects, and iteration

- Top-level `await` accepts tool calls, process handles, `sleep`, `waitSignal`,
  `Promise.all`, and `Promise.allSettled`. The only accepted authored async
  function is the literal `run` field of a static process definition.
- `Promise.all` and `Promise.allSettled` reuse the shared
  `ResourceOperationBatch` machinery. Plain values receive Promise-resolve
  behavior, and `allSettled` produces fulfilled or Error-shaped rejected
  records. Unsupported aggregate shapes reject by name.
- `Date.now()` and `Math.random()` use the journaled
  `LanguageRuntimeValue` effect. Replay consumes the recorded value rather than
  sampling the VM clock or RNG. `new Date()` rejects as `TS_NEW_UNSUPPORTED`.
- Canonical counter loops and `for...of` over arrays and strings are supported.
  String iteration is by Unicode code point. The snapshot-based v1 iterator
  rejects source mutation and user-authored calls in the body; a `continue`
  that crosses `finally` is also rejected instead of changing ordering.

### Standard library and safety

- The advertised inventory contains 55 method names: 37 static methods and 18
  instance method names across String, Array, Object, JSON, Number, and Math.
  Every absent operation is a named static or typed runtime rejection.
- Checked Node v25.2.1 differentials and focused regressions cover coercion,
  UTF-16-sensitive behavior, key ordering, replacement tokens, JSON number
  formatting, numeric edge cases, optional arguments, and the supported method
  inventory. The generated expectation table is byte-stable at SHA-256
  `e3d8192418b534b239d7105fc83a6cb2d6cf324103557044c832586998072ba9`.
- Multiplicative string growth, including `repeat` and `$&`, ``$` ``, and `$'`
  replacement expansion, is sized before allocation. A result beyond 8 MiB
  terminates with uncatchable `MemoryLimitExceeded`; it cannot become a host
  allocation panic or OOM attempt.
- The inherited 64 KiB source cap, nesting budget, parse-stack reservation,
  AST classification, and no-abort contract remain in force for the expanded
  grammar.

## Ticket done-when evidence

| Requirement | Evidence | Result |
| --- | --- | --- |
| Agent primitives in cells and durable processes | `agent_surface.rs` exercises all lowerings and execution shapes; protocol tests exercise start, trigger registration, and named signaling | PASS |
| Trigger registration | `typescript_register_trigger_executes_end_to_end` crosses the TypeScript executor and trigger environment | PASS |
| Signal round-trip | `typescript_signal_round_trip_crosses_protocol_and_process_engine` stores and starts a TS artifact, sends a named signal through the protocol controller, resumes `waitSignal`, and observes terminal output | PASS |
| Production artifact execution | `typescript_artifact_runs_through_process_engine_to_terminal` loads a stored TypeScript artifact through the Restate process engine and reaches terminal state | PASS |
| Suspend/resume in `waitSignal`, `sleep`, and pending `finally` | `durable_processes_resume_across_await_signal_sleep_and_pending_finally` | PASS |
| Suspend/resume during aggregate await | `durable_process_resumes_after_shared_promise_batch` | PASS, subject to the first-settled blocker below |
| Journaled time and randomness | Core journal/replay tests plus TypeScript effect lowering and execution tests | PASS |
| First-party `typescript.tool` bindings and Promise signatures | Tool-catalog and signature-renderer tests | PASS |
| Common first-shot programs avoid missing stdlib | Four executable fluency-smoke programs lower successfully | PASS |
| ECMA-exact accepted `Promise.all` | First-settled rejection is not representable by the current batch result contract | **BLOCK** |

## Adversarial review closure

Two independent review tracks ran after the first implementation cut. The
following findings were reproduced, repaired, and covered by focused tests.

### Durability and process-boundary review

- Production artifact caching linked TypeScript source with Lashlang identity.
  Linking now honors the requested dialect.
- The compiled-process cache key omitted dialect and module identity. Both now
  participate in cache isolation.
- Named process signaling had no `SignalRun` lowering or protocol-to-process
  round-trip proof. The three-argument `wake` path and production-shaped
  integration test now cover it.
- `finish` inside a process could skip authored `finally` blocks. It is now
  statically rejected.
- Resolved Promise aggregate leaves and accepted non-tool shapes could route to
  a host await on the list itself. Plain values now resolve locally; unsupported
  promise kinds reject explicitly.
- `Promise.allSettled` exposed a raw tool error string. Rejections now use the
  same Error-shaped value as an ordinary awaited tool failure.

### ECMA and resource-safety review

- String `for...of` lowered successfully but reached a list-only VM iterator.
  The lowering now snapshots code-point strings into an iterable list.
- A classic-loop `continue` crossing `finally` ran the update too early. That
  unsafe shape now rejects by name.
- Huge finite `String.repeat` counts and replacement-token expansion could
  allocate before the heap meter. Both paths preflight the exact result bound.
- `JSON.stringify` number rendering, `Math.pow` infinity cases, String and Array
  `lastIndexOf` optional arguments, object integer-key ordering, and other
  advertised edge cases were aligned with the Node oracle or removed from the
  advertised surface.
- The review's surviving finding is the shared aggregate settlement-order
  blocker below.

## Remaining release blocker

`crates/lashlang/src/runtime/vm/effects.rs` awaits one
`AbilityOp::ResourceOperationBatch`. The host returns only when all leaves have
settled, after which the VM scans the result vector in input order for an error.

Decisive case:

1. Promise leaf 0 rejects after 5 seconds with reason `A`.
2. Promise leaf 1 rejects after 10 milliseconds with reason `B`.
3. Node rejects after roughly 10 milliseconds with `B`.
4. The current Lash aggregate waits for the batch and reports `A`.

The fix must evolve the shared aggregate-await contract so the VM can observe
settlement order and terminate `Promise.all` at the first rejection while still
preserving replay-deterministic journal semantics. Changing only the TypeScript
lowering would either duplicate the aggregate machine or fake semantics, both
contrary to the FIG-1305 architecture ruling.

## Accepted v1 restrictions and deviations

The full executable register is in `crates/lash-typescript/README.md`. The
agent-surface-specific limits are:

- General async functions reject; v1 accepts top-level await and a static
  process definition's async `run` literal.
- Promise aggregates accept array literals containing top-level tool promises
  and resolved values. Nested aggregates, non-array iterables, and process or
  timer promises inside an aggregate reject.
- Classic `for` accepts the canonical counter form. `for...of` snapshots arrays
  and strings; source mutation and user-authored calls in its body reject until
  a resumable iterator protocol exists.
- Arrays are dense. Hole-creating writes and named or negative index writes
  reject without mutation.
- Lone UTF-16 surrogates are not representable in the v1 UTF-8 value model.
  Literals reject statically; astral indexing and string-backed
  `Object.values`/`Object.entries` reject when they would manufacture a lone
  surrogate. Methods with that unavoidable result are not advertised.
- A single string result is capped at 8 MiB with uncatchable typed exhaustion.
- Classes, modules, generators, destructuring, `for...in`, and the other
  inherited exclusions remain named rejections.

The `Promise.all` first-settled problem is deliberately absent from this list:
it is a blocker, not an accepted deviation.

## Fluency smoke

FIG-1304 rejected recurring first-shot agent shapes: awaited tools, aggregate
promises, ordinary `for` loops, and common data shaping. The post-implementation
smoke keeps four representative programs as executable source in
`tests/fluency_smoke.rs`. All four lower, covering `JSON.stringify`, `join`,
`Object.entries`, `toUpperCase`, `Math.max`, awaited tool batches, and durable
process effects.

No fresh live-model sampling was performed in this worktree. The evidence is a
repurposed, executable first-shot corpus, not a claim about a newly measured
model success rate.

## Compatibility and persistence

- There are no SQL changes, so change-triggered schema congruence is not
  applicable. The existing workspace schema-congruence tests still pass.
- There is no durable format-version bump. Persisted process artifacts now
  carry the compilation dialect needed to select the correct compiler.
- The expanded TypeScript tool metadata changes one stored logical-size label
  from `2.0KB` to `2.1KB`; checkpoint and revision semantics are unchanged.
  This is the only durable transcript snapshot change.
- The checked-in Node differential generator was regenerated and then rerun;
  the second run produced the same bytes and hash.

## Verification

All commands used `CARGO_TARGET_DIR=/workspace/.cargo-target-lash-fig-1305`
where Cargo was involved.

| Gate | Result |
| --- | --- |
| `cargo check --workspace --all-targets --locked` | PASS |
| `cargo test --workspace --locked` | PASS; full unit, integration, property, UI, trybuild, simulation, conformance, and doctest suite |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | PASS |
| `cargo fmt --all --check` | PASS |
| `python3 scripts/check_included_file_formatting.py` | PASS; 37 included Rust files |
| `python3 scripts/lint_docs.py` | PASS; 46 HTML and 42 registry files |
| `bash scripts/check-rustdoc.sh` | PASS; 602 public items documented, 0 missing |
| `python3 scripts/check_test_quarantines.py` | PASS |
| `python3 scripts/check_api_example_coverage.py` | PASS; 8,074 entries |
| `just perf-guard` | PASS; 297 Lashlang performance results and 1 profile result |
| `bash scripts/check-production-file-size.sh` | PASS; main executor reduced to 1,588 lines |
| `git diff --check c03deb8d56a9c552c3250a153ab0e0ea0de195b7..HEAD` | PASS |
| `cargo test -p lashlang --locked` | PASS; 462 unit tests and all package integrations |
| `cargo test -p lash-typescript --locked` | PASS; all 15 agent-surface tests plus dialect, differential, ECMA, fluency, rejection, safety, and conformance suites |
| Focused protocol named-signal integration | PASS |
| Focused Restate TypeScript artifact-to-terminal integration | PASS |
| SQL change-triggered congruence gate | N/A; no SQL changed |

The first full workspace run exposed only gate-driven mechanical corrections:
the TypeScript metadata size label and a deliberately regenerated differential
classification. The final full run passed with zero failures.

## Repository state

- Conventional commits include categorized `Release-Notes:` entries and no AI
  attribution.
- No push was performed.
- This report is the only change in the final documentation commit and is
  committed last, so `REPORT.md` is the branch-tip deliverable.
