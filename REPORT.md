# FIG-1303 implementation report

Status: complete on `samuel-fig-1303`, after one adversarial review round and
its fixes.

## Delivered commits

| Commit | Purpose |
|---|---|
| `738b20dce` | Add AST-only exceptions, exception bytecode, durable handler/finally state, VM unwinding, taxonomy enforcement, and conformance tests. |
| `f858fdd0c` | Add the public AST integration example and register all new public API symbols. |
| `557243c5c` | Red-side: `break`/`continue` leaking exception scopes. |
| `8e15036c1` | Unwind exception scopes on `break` and `continue`. |
| `c064d4b5d` | Red-side: a cleanup-only scope rewriting the error identity. |
| `19d3c5f6c` | Preserve the original error through a cleanup-only unwind. |
| `c551107ca` | Red-side: handler and finally stacks validated as bags. |
| `8d60296d1` | Validate exception stacks as nesting structures. |
| `61127ec6c` | Red-side: unbounded AST-only nesting. |
| `0606e076e` | Cap AST nesting depth at the construction entry points. |
| `0b872cf1d` | Classify runtime errors exhaustively and pin their codes. |
| `276abc2db` | Fold the review round's covering cases into the suite; wire host cancellation. |

## Design

### Public AST and bytecode

The public AST exposes `Expr::Try(Box<TryExpr>)`, `TryExpr`, `CatchClause`, and
`Expr::Throw(Box<Expr>)`. Both nodes participate in traversal, folding, semantic
hashing, host-requirement collection, linker validation/lowering, and type
inference. The compiler lowers them to six real VM instructions:

- `PushHandler { handler, finally, catches }`
- `PopHandler`
- `EnterFinally { finally, resume }`
- `EndFinally`
- `AbandonFinally`
- `Throw`

`heap_plan` remains the single exhaustive opcode contract table. `Throw` is
heap-native because the thrown value remains in the VM heap; the five
control-only instructions declare zero stack values at the host boundary. The
exception instructions also have an explicit `Exception` profiler tag.

### Loop control flow across exception scopes

`break` and `continue` are abrupt completions, and ECMA-262 requires them to run
the pending `finally` blocks between the jump and its target loop, innermost
first, while every handler they cross comes off the handler stack. The compiler
carries a handler-scope stack alongside its loop contexts; each loop context
records the scope depth it was entered at, so a jump edge emits exactly the
instructions the fall-through edge would have executed, and nested loops unwind
only as far as their own loop.

Leaving a running `finally` body by a jump replaces that body's pending
completion. `AbandonFinally` performs the replacement: it drops the pending
resume or rethrow and restores the operand stack. The `finally` that a `break`
itself enters uses the ordinary `Normal { resume_ip }` completion with a resume
site inside the jump edge, so the pending jump lives in bytecode and no new
pending-completion kind reaches the wire.

Before this, `break` and `continue` compiled to a raw patched jump: the pending
`finally` never ran and the handler stayed installed, where a structurally
unrelated later throw could be captured by it and transferred into abandoned
catch code — and the stale handler was written into the durable continuation.

### Error value shape and error identity

Catchable `RuntimeError` values are imported into the normal heap as records.
They use the following dialect-facing shape:

| Field | Value |
|---|---|
| `name` | `"EffectError"` for effect/tool failures, otherwise `"RuntimeError"` |
| `message` | Existing sanitized `RuntimeError` display text |
| `code` | Stable guest-visible identity from an explicit table, for example `LenUnsupported` or `UnwrappedModuleOperationFailed` |
| `details.kind` | `"effect"` or `"runtime"` |
| `details.instruction` | Bytecode instruction index at which the failure became a throw |
| `details.operation` | Structured sanitized operation name when the failing instruction is an effect site |

Explicit `throw value` transfers the original value unchanged. Error records use
the same import, allocation metering, serialization, forest validation, and GC
rules as every other record. Pending thrown values held by a finally
continuation are included in VM roots.

A routed runtime failure now travels with the typed `RuntimeError` it came from
alongside that heap record. When the unwind ends with no catch, `EndFinally`
re-raises the original error with its original instruction and span
attribution; `UncaughtException` is reserved for values thrown by an explicit
`throw`. Wrapping an expression in `try { … } finally { … }` therefore no longer
changes which error the host sees, which matters because host-side
classification (effect-vs-runtime, retryability) is variant-based.

That origin is durable rather than VM-local, so a cleanup chain that suspends
mid-`finally` resumes to the same identity. Carrying a typed error on the wire
requires `RuntimeError` to round-trip, so its borrowed `&'static str` payloads
are now `Cow<'static, str>` and `RuntimeError`, `FormatError` and
`ExecutionHostError` derive serde. Validation refuses an origin holding an
`UncaughtException`, the one variant carrying a `Value` and therefore the one
that could smuggle an unrooted heap reference past the forest rule.

### Handler and finally records

An active handler records:

| Field | Purpose |
|---|---|
| handler instruction pointer | Catch transfer target |
| optional finally instruction pointer | Cleanup target |
| catches flag | Distinguishes catch handlers from cleanup-only handlers |
| frame depth and function identity | Exact owning call frame |
| operand-stack depth | Stack restoration boundary |
| iterator-stack depth | Iterator restoration boundary |

A running finally records its pending completion (`Normal { resume_ip }` or
`Throw { value, origin }`), handler-stack depth, frame identity, and
operand-stack depth. Both record families serialize in `VmContinuation`.

### Unwind algorithm

1. Import the thrown value into the current heap and locate the nearest
   catch-capable handler.
2. Run cleanup-only handlers above that catch from inner to outer. Each cleanup
   atomically restores the owning frame, operand stack, and iterator stack, then
   enters its finally with the pending throw recorded.
3. On `EndFinally`, either resume the saved normal target or continue throwing
   the saved value. If nothing catches it and the throw came from a routed
   runtime failure, the original typed error is raised instead of an
   `UncaughtException` record.
4. If code in a finally throws, discard that finally's prior pending completion
   and unwind the new value. This gives the required replacement semantics; a
   `break` or `continue` out of the finally body replaces it the same way.
5. At a catch, restore the handler's frame/stack/iterator boundary, push the
   thrown value, and transfer to the handler target.
6. If no handler accepts an explicit throw, return typed
   `RuntimeError::UncaughtException`. Unwinding itself does not suspend; effects
   executed by ordinary finally code may suspend and serialize their pending
   completion.

Frame restoration uses the existing call-frame state and restores temporary
iterator bindings as iterators are unwound. Occurrence counters are independent
continuation state, so retry after catch allocates the next occurrence and
replay key.

### Nesting depth of AST-built programs

The parser's nesting cap bounds parsed programs, and the linker's 2 MiB stack
contract is anchored on it. `Try`, `Throw`, `Function` and the other AST-only
shapes have no source grammar, so nothing bounded them: a dialect that lowered a
deeply nested `try` aborted the host process on a stack overflow rather than
returning a typed error. `LinkedModule::link`, `compile_ast` and
`compile_process` now apply `check_ast_nesting_depth` first.

The check is a generic iterative walk over `Expr::children`, which is exhaustive
over the variants, so it covers every AST node rather than only the new ones,
and it cannot itself overflow on the inputs it exists to refuse.
`MAX_AST_NESTING_DEPTH` sits above the deepest tree the parser can produce and
below the depth at which the cheapest AST chains exhaust the budget; per-level
stack cost varies by variant, so it bounds the tree rather than promising a
per-variant margin. `compile_ast` now returns a `Result`, which is the only
signature change.

### Lashlang lowering decision

The textual lashlang surface remains unchanged. The parser has no `try`,
`catch`, `finally`, or `throw` production, and the canonical renderer returns
`NonSourceableExpression` for both new AST node families —
`the_renderer_declines_try_and_throw_at_every_nesting` pins that refusal.
Because the workflow-graph projector only ever reads parsed, canonicalized
source, those nodes cannot reach it; its source-only note now names them
explicitly rather than leaving the wildcard arms accidental.

Existing textual `?` still compiles to its existing unwrap instruction. When no
AST-only handler surrounds it — which is true for every parsed lashlang program
— its failure leaves the VM with the same typed error and diagnostics as before.
An embedding or later dialect can place that existing effect site inside an
AST-only `Try`, in which case the resumed effect failure becomes a throw at that
site.

No prompt, parser, diagnostic, or benchmark snapshot changed.

## Taxonomy enforcement

| Layer | Catchable | VM route |
|---|---:|---|
| Effect/tool failure | Yes | Existing effect error becomes an `EffectError` heap record and enters normal throw unwinding. Its typed variant survives an unwind that nothing catches. |
| Ordinary guest-reachable runtime error | Yes | Central `route_runtime_error` converts it to a `RuntimeError` heap record when an exception scope is active, carrying the original error alongside it. |
| Instruction, deadline, memory, or frame-depth exhaustion | No | The taxonomy prevents handler lookup; instruction/deadline checks terminate directly from `enforce_execution_bounds`, while memory/frame terminals are rejected by the same central classification when raised at allocation/call sites. |
| Host cancellation | No | The cooperative `ExecutionHost::is_cancelled` probe terminates in `enforce_execution_bounds` as `HostCancelled`, before any handler lookup. `LashlangProcessHost` forwards the engine's cancellation token to it, so the row is live in production. |

The classification is a single exhaustive `RuntimeError::taxonomy` match rather
than two hand-maintained `matches!` lists, so a new variant fails to compile
until it declares its class; `is_uncatchable_terminal` and `is_effect_failure`
read it and keep their signatures. `RuntimeError::code` is likewise an explicit
static table pinned by an exhaustive test, not a string derived from `Debug`, so
renaming a Rust variant cannot silently change a value guest code branches on.

`enforce_execution_bounds` documents the direct terminal boundary.
`route_runtime_error` is the single defensive classification point for errors
raised elsewhere in instruction execution. Tests wrap every terminal class in a
catch returning a sentinel and prove the sentinel is never observed.

## Durable format changes

| Contract | Previous | New |
|---|---:|---:|
| VM continuation format | 2 | 4 |
| Compiled bytecode format | 4 | 6 |
| Lashlang VM ABI | `lashlang-vm-abi-v2` | `lashlang-vm-abi-v3` |

Both numeric contracts moved twice inside this unmerged branch: to 3 and 5 for
the exception layer itself, and again to 4 and 6 for the fix round's new
`AbandonFinally` opcode, scope-extent table and pending-error origin. No
released artifact carries the intermediate stamps.

Continuation v4 requires `handler_stack` and `finally_stack`, and a throw
completion requires its `origin` field; absence is a decode error rather than a
default, and there is no migration decoder. Older versions fail closed through
the existing exact format-version check.

Both exception stacks are validated as nesting structures, not bags of records.
The compiler emits a scope-extent table into the chunk — one entry per
`PushHandler`, carrying its install site, handler and finally targets, and the
end of the region it protects. A durable handler record must name one of those
scopes exactly, and consecutive records in one frame must form a strictly nested
chain of them, which makes reordered and forged handler stacks unrepresentable
rather than merely defended against inside the VM. Requiring an exact scope
match also closes the weaker target check: a handler target one instruction past
its catch entry, or a finally target borrowed from another scope, no longer
passes as "somewhere inside the right function".

Decode-time validation covers what needs no compiled program — handler frame
depths never shrink, per-frame operand and iterator depths never shrink, and the
finally stack's handler and frame depths never shrink. Resume-time validation
adds the scope-extent chain and the per-record range checks, so
`Vm::resume_from` refuses these shapes as well as serde does; that matters
because `VmContinuation`'s stacks are public API a host embedding can build in
Rust without touching the wire.

## Test coverage

| Requirement | Coverage |
|---|---|
| Explicit throw and ordinary runtime catch | `explicit_throw_transfers_the_original_value_to_catch`; `runtime_errors_are_caught_as_heap_backed_error_records` |
| Tool/effect failure and structured operation metadata | `effect_failure_is_a_throw_with_structured_operation_metadata` |
| Normal, exceptional, nested, and replacement finally behavior | `finally_runs_on_normal_and_exceptional_paths_and_a_new_throw_replaces_the_old_one` |
| Cross-frame unwind | `throw_unwinds_function_frames_to_the_callers_handler` |
| `break`/`continue` across try, catch and finally bodies | `exception_control_flow_cases.rs`: pending finallys run inner-to-outer, nested loops unwind only to their own loop, no handler or finally record outlives its region in the durable continuation, and a finally entered by a `break` resumes to the break after a suspension |
| Error identity through a cleanup-only unwind | `a_cleanup_only_scope_preserves_the_runtime_error_identity`; `a_cleanup_only_scope_preserves_an_effect_failure_identity`; `a_cleanup_only_scope_keeps_the_failing_expression_span`; `a_suspended_cleanup_chain_resumes_with_the_original_error` |
| Suspension inside try, catch and finally | `continuation_round_trips_inside_try_and_finally`; `effects_suspend_inside_finally_with_pending_errors_and_gc_stress`; `suspension_inside_a_catch_body_is_byte_identical_under_gc_stress` |
| Every uncatchable terminal | `execution_terminals_bypass_a_surrounding_catch` covers instruction, deadline, memory, frame-depth, and cancellation terminals; `memory_exhaustion_while_importing_the_error_record_stays_terminal` and `frame_depth_exhaustion_inside_a_catch_body_is_terminal` cover terminals raised from inside the exception machinery, and `cancellation_observed_mid_unwind_is_terminal` covers a host that cancels once the cleanup chain is running |
| N-frame/finally/iterator interaction | `exception_unwind_crosses_frames_finally_chains_and_iterators` |
| Unwinding across a builtin callback frame | `a_throw_escapes_a_builtin_map_callback` |
| Thrown-slot aliasing and catch-binding copy semantics | `every_boundary_of_a_caught_throw_stays_capturable`; `mutating_the_catch_binding_leaves_the_thrown_slot_alone` |
| Retry occurrence and replay identity | `effect_failure_catch_retry_is_a_new_occurrence` proves occurrences advance from 1 to 2 |
| Malformed durable state | `malformed_exception_continuations_fail_closed` covers out-of-range handler, cross-function target, oversized stack base, invalid finally target, and frame-identity mismatch; `exception_wire_cases.rs` covers reordered handler stacks (cross-frame and same-frame), handler and finally targets that are not scope entries, and a non-monotonic finally stack |
| Format-version fail-closed | `a_v2_shaped_continuation_fails_closed` |
| Cross-process determinism and exactly-once cleanup | `independent_processes_dump_identical_exception_continuations`; `a_cleanup_chain_is_exactly_once_across_a_process_boundary` |
| GC stress with live handler/error state | `effects_suspend_inside_finally_with_pending_errors_and_gc_stress` compares stress and non-stress continuation bytes and resumes the pending heap error |
| Renderer refusal | `the_renderer_declines_try_and_throw_at_every_nesting` |
| AST-only nesting bound | `stack_budget_ast_try_finally_at_parser_max_depth` pins the per-level cost of the new variants through the full pipeline on 2 MiB; `ast_only_nesting_beyond_the_cap_is_a_typed_error_not_an_abort` drives real depths in child processes, because an abort cannot be caught in-process |
| Taxonomy and code stability | `every_runtime_error_display_is_exact` also pins every variant's guest-facing code through an exhaustive match |
| Public API route | `embedding_lashlang_functions::ast_only_exceptions_compile_and_execute` compiles and executes only through public AST APIs |

### Existing test edits

- Authored continuation fixtures state continuation format `4` and include
  empty `handler_stack` and `finally_stack` fields.
- Direct `VmContinuation` constructors initialize both new stacks as empty.
- `rlm_prompt_claims` destructures `ShapingEmptyList` instead of matching a
  borrowed literal, because the payload is now `Cow<'static, str>`.
- Two `compile_ast` call sites in examples add `.expect(…)` for its new
  `Result`.
- No snapshot file changed, and no `.snap.new` file exists.

## Review round

Two independent adversarial reviews were run against `752f28ac3` (opus lane:
BLOCK; sol-sub lane: APPROVE-WITH-FIXES). Every finding is closed above, with a
red-side regression committed before each fix.

| Finding | Severity | Status |
|---|---|---|
| `break`/`continue` crossing a `Try` leaks the handler and skips the finally | P0 | Fixed (compiler handler-scope stack, `AbandonFinally`) |
| A catch-less `finally` rewrites the host-visible error identity | P1 | Fixed (durable pending-error origin) |
| Handler/finally stacks validated as bags, not structures | P1 | Fixed (scope-extent table + monotonicity) |
| Handler/finally targets only range-checked | P3 | Fixed by the same scope-extent match |
| AST-only nesting depth uncapped | P2 | Fixed (`check_ast_nesting_depth`) |
| Taxonomy is two non-exhaustive `matches!` lists | P2 | Fixed (`RuntimeError::taxonomy`) |
| `RuntimeError::code` derived from `Debug` | P2 | Fixed (explicit table + pinning test) |
| `ExecutionHost::is_cancelled` unwired in production | P3 | Wired; see the deferred item below for the end-to-end test |
| `workflow_graph.rs` wildcard-swallows `Try`/`Throw` | P3 | Documented explicitly, with the renderer refusal pinned |

`break`/`continue` inside a `finally` body is implemented to ECMA-262
completion-replacement semantics rather than rejected at link time: the pending
completion is discarded by `AbandonFinally` and the jump continues, and the
enclosing scopes it still has to leave are emitted by the same handler-scope
walk.

## Verification results

| Command | Result |
|---|---|
| `cargo check --workspace --all-targets --locked` | Pass |
| `cargo test --workspace` | Pass |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | Pass |
| `cargo fmt --all --check` | Pass |
| `python3 scripts/check_included_file_formatting.py` | Pass, 37 included files |
| `python3 scripts/lint_docs.py` | Pass |
| `bash scripts/check-rustdoc.sh` | Pass |
| `python3 scripts/check_test_quarantines.py` | Pass |
| `python3 scripts/check_api_example_coverage.py` | Pass |
| `just perf-guard` | Pass |
| `bash scripts/check-production-file-size.sh` | Pass |
| `git diff --check` | Pass |

## Deferred items

- TS syntax and ECMA-262 lowering consume the AST/bytecode capability in a later
  layer.
- Textual lashlang exception syntax remains absent by design.
- Suspension during the atomic unwind operation remains unsupported as
  explicitly allowed; suspension inside try, catch and finally code is
  implemented.
- An end-to-end cancellation test at the `lash-lashlang-runtime` level is not
  added. The wiring itself is a one-line delegation to the engine's cancellation
  token, but driving a real process through that crate needs a
  `ProcessEngineRunContext`, which only `lash-core`'s private process-worker test
  harness can build; that crate has no process-execution test of its own to
  extend. The taxonomy row is covered at the VM level by
  `execution_terminals_bypass_a_surrounding_catch` and
  `cancellation_observed_mid_unwind_is_terminal`.
