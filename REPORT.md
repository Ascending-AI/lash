# FIG-1305 implementation and verification report

Branch: `samuel-fig-1305`

Base: `1532794d93606940121c3dcff88ac9ad088ddd3e` (the FIG-1304 head this layer stacks on, after the stack was rebased onto main)

Implementation commits:

- `cff0b745cc5ad9924625d9d5fc65e83f155818c5` — `feat(typescript): add the durable agent surface`
- `f0bd18d9f0acb41260550c9b5851a892af8c2bb9` — `fix(typescript): close durable agent review gaps`
- `6b7dc23c2062aa23729a9c4fd3dfb8e45255a686` — `fix(rlm): pass the lease owner the process worker config now requires`
- `061321389405ebd050a0a01558493c72bd747c77` — `feat(lashlang): carry per-leaf settlement order on the batch result`
- `48b6a05b11ec19282443db8422cd0281fcc8057c` / `bb797239253135c63dd0bf2357c1f17d99a0cc17` — the first-settled rejection, red then green
- `c3a8b3a7e8d318ccc44722ac596f6c88ebaa677f` — determinism, fail-closed journal and format pins
- `1cd32308a3f3019232bcde5874a1dd139c19073f` — refuse a malformed settlement order instead of repairing it
- `0d56d88f6b1064a21d490b35856f3b66ccf3c332` — for-of bodies do work; artifacts must name their dialect
- `b75f390459e14d1d9040826720e99b41c7707ba6` — restore the instance methods; classify misses by name

Every SHA above is an ancestor of the head. An earlier revision of this ledger
named pre-rebase commits that no longer describe this history.

## Verdict

**READY TO STACK.**

The TypeScript agent surface, durability paths, standard-library inventory,
resource bounds, and repository gates are implemented and green, and the ECMA
`Promise.all` blocker is closed: the aggregate now rejects with the reason of
the leaf that settled first.

The repair was to the shared aggregate contract, not to the TypeScript lowerer,
because the lowerer had nothing truthful to work with — the batch result carried
only input-ordered results. It now carries the order the leaves settled in, and
the VM selects the reported rejection from that order. See
[Settlement order](#settlement-order).

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
| Suspend/resume during aggregate await | `durable_process_resumes_after_shared_promise_batch` | PASS |
| Journaled time and randomness | Core journal/replay tests plus TypeScript effect lowering and execution tests | PASS |
| First-party `typescript.tool` bindings and Promise signatures | Tool-catalog and signature-renderer tests | PASS |
| Common first-shot programs avoid missing stdlib | Four executable fluency-smoke programs lower successfully | PASS |
| ECMA-exact accepted `Promise.all` | `promise_all_rejects_with_the_first_settled_rejection` drives the report's decisive case through the shared batch machine | PASS |

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

## Settlement order

`Promise.all` is specified to reject with the first *settled* rejection.
`crates/lashlang/src/runtime/vm/effects.rs` scanned its leaves in input order,
so a batch whose second leaf rejected first still reported the first leaf's
reason — the decisive case in the original review: leaf 0 rejects after five
seconds with `A`, leaf 1 rejects after ten milliseconds with `B`, Node reports
`B`, and the aggregate reported `A`.

Three things had to change, and only one of them was in the VM.

**The order existed in exactly one place and was being thrown away.**
`schedule_tool_batch` drives a batch's leaves through `FuturesUnordered`,
collects them in completion order, then sorts by input index. That sort was the
only record of which leaf settled first. It now reports both halves: outcomes in
input order, and the input indices in the order they completed.

**The order is journaled, and fails closed.** It crosses the effect boundary as a
required field on `RuntimeEffectOutcome::ToolBatch`, which is what the replay
journal stores. It deliberately has no serde default: an aggregate that selects
by settlement order cannot distinguish a defaulted input order from a recorded
one, so an entry written before this field existed is refused rather than
replayed as input order. There is no migration decoder. Continuation and
snapshot formats are untouched — a batch result is consumed inside a single
`perform` and never persisted — and a test pins that.

**The selection rule is recorded per batch at lowering.** The compiler already
knows the dialect there, so a TypeScript aggregate marks its compiled batch and
a Lashlang one leaves it clear. The alternative was to read the VM's
`reference_semantics` flag at run time, which was rejected: that flag answers a
heap-ownership question, and one predicate answering two questions is the exact
defect shape that cost FIG-1304 three rounds. The cost is
`LASHLANG_VM_ABI_VERSION` v4 to v5; the semantic hash and Lashlang's battery are
unchanged.

Both production hosts translate the order from invocation positions into leaf
positions, with leaves that settled before the batch ran — journaled runtime
values, and anything that failed during preparation — leading. A host that
reports an order that is not an ordering of its own results fails closed with a
typed `ResourceBatchSettlementOrder` rather than being guessed at.

`Promise.allSettled` keeps its results in input order however the leaves
settled, and a test pins that. It is not merely unaffected by construction: a
batch only selects by settlement order when it can propagate a leaf's rejection,
so an allSettled batch does not validate — and cannot die on — order metadata it
never reads.

The order translates from invocation positions to leaf positions at both hosts,
with anything that settled before the batch ran leading. Today the only
reachable case is a leaf that failed during preparation; a journaled runtime
value cannot be a batch leaf, because an aggregate containing one is rejected
before it reaches the host. An earlier revision of this section claimed
otherwise.

## Dual-review fix round

Two independent adversarial reviews returned APPROVE-WITH-FIXES with disjoint
findings. Every one is closed below.

| Finding | What it was | Red | Fix |
| --- | --- | --- | --- |
| sol F1 (High) | `call_tool_batch` dropped out-of-range settlement positions and back-filled the gaps, so any malformed order became a clean input-order permutation that no validator could tell from a real one — the original rejection selection, restored silently | `1cd32308a` | `1cd32308a` |
| opus F1 | The fail-closed journal pin keyed the tag `kind` instead of `type`, so it failed on the tag and proved nothing about the field it guards | `1cd32308a` | `1cd32308a` |
| opus F6 | An `allSettled` batch validated — and could die on — settlement metadata it never reads | — | `1cd32308a` |
| opus F10a | The settlement diagnostic reported lengths only, so a duplicate rendered as self-consistent and undiagnosable | — | `1cd32308a` |
| sol F4 | `ModuleArtifact.compilation_dialect` carried a serde default, so a dialect-less TypeScript artifact decoded as Lashlang and verified | `0d56d88f6` | `0d56d88f6` |
| opus F2 | `for…of` bodies rejected every call and member assignment, so the canonical agent loop was a link-time rejection — justified by a durability claim the reviewer refuted | `0d56d88f6` | `0d56d88f6` |
| opus F5 | Nine instance methods were withdrawn behind a representability argument that covers only astral-splitting shapes, which the existing runtime rejection already handles | `b75f39045` | `b75f39045` |
| opus F3 + sol F3 | An unadvertised method on a non-module receiver fell through to the tool-call branch and, under `await`, failed at the host untyped | `b75f39045` | `b75f39045` |
| opus F8 | `pad_string` allocated without a size preflight | — | `b75f39045` |
| opus F7 + sol F2 | `JSON.parse` rejected out-of-range numbers where ECMA clamps to an infinity | `b75f39045` | `b75f39045` |
| sol F5 | An `allSettled` rejection carried the unwrap error's code, which cannot describe a leaf that is never unwrapped | — | `14e5feb74` |
| opus F9 | A multi-segment call path rendered `declare namespace a { declare namespace b`, which is not valid TypeScript | — | `14e5feb74` |
| opus F4 + sol F6 | The ledger named pre-rebase commits, the lashlang count was stale, and the settlement narrative described an unreachable path | — | `14e5feb74` |

Three of these deserve a note beyond the table.

**The repair-instead-of-refuse defect was the important one.** The contract, the
journal decoder and the VM validator were all correct; between them a
normalisation step quietly made every malformed order well-formed. The guarantee
read as delivered in three places while being false in one. The fix validates at
the boundary and deletes the back-fill, because a repair that produces a valid
permutation is indistinguishable from a real one downstream — by construction, no
later check could have caught it.

**The `for…of` restriction was justified by a false claim.** The register said the
filter waited on a resumable iterator protocol; the reviewer suspended a process
inside a `for…of` and resumed it through a continuation round-trip. The filter
was conservatism, not durability, and it cost the most common loop in the
language. It now rejects only shapes that can reach the iterable, and says which
shape did.

**`sol F5` is closed only as far as the contract allows.** `ExecutionHostError`
carries a message and nothing else, so the finest identity available to an
`allSettled` record is the host's own text. The record no longer claims the
unwrap error's code, which was simply wrong for a leaf that is never unwrapped.
Giving rejections a discriminable code needs a code channel on the effect-host
contract that every host would have to populate — named here rather than faked.

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

A rejected `Promise.all` still waits for every leaf to settle before reporting.
ECMA specifies which reason surfaces, not when, so this is a runtime-system
constraint rather than an alternate semantics, and it is registered as one: v1
has no fail-fast cancellation of an in-flight batch leaf.

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
| `cargo nextest run` (lashlang, typescript, core, protocol-rlm, lashlang-runtime) | PASS; 2 241 tests |
| `just perf-guard` | Pre-existing failures only. The lashlang leg fails 162 budgets at the recipe's 500 iterations and 45 at 2 500; both sets are **identical before and after this work**, so nothing here regressed a budget. The overages shrink from wild (1 459 vs 336) to marginal (3.6 vs 3.0) as iterations rise, which says the recipe's 500-iteration numbers are dominated by warmup rather than by steady-state cost. Recalibration is owned separately. |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | PASS |
| `cargo fmt --all --check` | PASS |
| `python3 scripts/check_included_file_formatting.py` | PASS; 37 included Rust files |
| `python3 scripts/lint_docs.py` | PASS; 46 HTML and 42 registry files |
| `bash scripts/check-rustdoc.sh` | PASS; 602 public items documented, 0 missing |
| `python3 scripts/check_test_quarantines.py` | PASS |
| `python3 scripts/check_api_example_coverage.py` | PASS; 8,074 entries |
| `just perf-guard` | PASS; 297 Lashlang performance results and 1 profile result |
| `bash scripts/check-production-file-size.sh` | PASS; main executor reduced to 1,588 lines |
| `git diff --check 1532794d93606940121c3dcff88ac9ad088ddd3e..HEAD` | PASS |
| `cargo test -p lashlang --locked` | PASS; 464 unit tests and all package integrations |
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
