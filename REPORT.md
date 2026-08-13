# FIG-1303 implementation report

Status: complete on `samuel-fig-1303`, after two adversarial review rounds and
their fixes.

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
| `83dbda356` | Red-side: the AST cap refusing parsed programs. |
| `3cfec80f9` | Make one nesting budget govern the parser and the AST. |
| `662a51120` | Red-side: handler scope substitution. |
| `d2f0c9161` | Anchor each durable handler scope to a live code position. |
| `b7ee34103` | Red-side: uncatchable-state and loop-control holes. |
| `d29ec2c36` | Make internal exception state terminal; refuse stray loop control. |
| `86a880c18` | Pin deferred process-terminal behaviour and every error code. |
| `ab832df7a` | Exercise the layer's remaining public API. |

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

### One nesting budget for the parser and the AST

`Try`, `Throw`, `Function` and the other AST-only shapes have no source grammar,
so nothing bounded them: a dialect that lowered a deeply nested `try` aborted
the host process rather than returning a typed error. `LinkedModule::link`,
`compile_ast` and `compile_process` now run `validate_ast` first.

`link` is the shared entry for parsed *and* AST-built programs, so one budget
governs both — and a syntactic level is not an AST level. Block-bodied
constructs (`if`, `while`, `for`) build an `Expr::Block` inside them and cost
two, so the parser's old limit of 40 admitted an 81-level tree. Measured on a
2 MiB thread, the full link/compile/execute pipeline aborts at an AST depth of
about 74 for the most expensive per-level variant (nested `try`/`catch`/
`finally`) and about 79 for the cheapest block-bodied one. The deepest `if`
chain the parser accepted therefore already aborted in `link` before this layer
existed, which the parser's own doc comment claimed was impossible.

So the budget is derived from measurement and enforced at the AST level:
`MAX_AST_NESTING_DEPTH` is 64, ten levels under the tighter cliff, and the
parser's `MAX_NESTING_DEPTH` drops from 40 to 30 so the worst parsed shape (two
AST levels per syntactic level plus a statement's constant) lands at 63. That
narrows the accepted source language, and it turns a host-process abort into the
typed error the limit already promised.

No constant relation carries this — a comparison between two numbers cannot see
the per-level cost difference. `tests/nesting_cap.rs` walks a family of parsed
shapes (`if`/`while`/`for`/record/list/paren/unary/binary/comprehension/call) to
the parser's refusal point and requires every accepted program to pass the depth
check *and* link, and reports the deepest tree the family can build.
`tests/stack_budget.rs` re-pins the 2 MiB budget at the cap using the most
expensive per-level variant rather than the cheapest.

`validate_ast` also covers loop-control placement: `break` and `continue` have
no parser to reject them out of place, and a host-built function body carrying a
stray `break` previously panicked the public `compile_ast`. Both checks are
iterative walks over `Expr::children`, exhaustive over the variants, and neither
can overflow on the inputs it exists to refuse. `compile_ast` and
`LinkedModule::link` now return typed errors (`InvalidAst` / `LinkError`), which
is the only signature change.

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

`InvalidExceptionState` is an uncatchable terminal too. It is raised *by* the
exception machinery when the bytecode has violated the handler/finally
discipline, and routing it back through a handler stack that has just been shown
inconsistent is the one place a catchable classification cannot be defended.

The classification is a single exhaustive `RuntimeError::taxonomy` match rather
than two hand-maintained `matches!` lists, so a new variant fails to compile
until it declares its class; `is_uncatchable_terminal` and `is_effect_failure`
read it and keep their signatures. `RuntimeError::code` is likewise an explicit
static table, not a string derived from `Debug`, so renaming a Rust variant
cannot silently change a value guest code branches on. The pinning test covers
instances rather than only match arms: the exhaustive match forces a new variant
to declare a code, and a count of distinct observed codes against the declared
list forces it to be constructed and asserted. That check found eight variants
that had been classified but never instantiated, three of whose expected display
strings had drifted from the real ones unnoticed.

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

Nesting alone still left every handler unanchored: it only had to name *some*
scope in the right function, so a single-entry stack could be pointed at a
sibling scope — substituting one cleanup-only scope for its sibling ran the
wrong cleanup and skipped the mandatory one, the same harm the nesting rule was
written to prevent, reached by a different one-field edit. The innermost handler
of each frame is therefore anchored to the code position that frame is sitting
at: the active frame's instruction pointer, or a parked frame's return site.
Outer handlers of the same frame stay anchored to the inner one through the
nesting chain, which makes them a consequence rather than a separate rule. A
scope is live over `(push_ip, end_ip]` — a handler is never on the stack while
its own `finally` body runs, and suspension inside a `catch` body is covered by
the catch-cleanup scope's own extent.

Decode-time validation covers what needs no compiled program — handler frame
depths never shrink, per-frame operand and iterator depths never shrink, and the
finally stack's handler and frame depths never shrink. Resume-time validation
adds the scope-extent chain, the anchor, and the per-record range checks, so
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
| Malformed durable state | `malformed_exception_continuations_fail_closed` covers out-of-range handler, cross-function target, oversized stack base, invalid finally target, and frame-identity mismatch; `exception_wire_cases.rs` covers reordered handler stacks (cross-frame and same-frame), handler and finally targets that are not scope entries, a non-monotonic finally stack, and handlers substituted for a sibling cleanup scope or an unrelated catch scope |
| Internal invariant violations stay terminal | `an_invalid_exception_state_bypasses_a_surrounding_catch` |
| Format-version fail-closed | `a_v2_shaped_continuation_fails_closed` |
| Cross-process determinism and exactly-once cleanup | `independent_processes_dump_identical_exception_continuations`; `a_cleanup_chain_is_exactly_once_across_a_process_boundary` |
| GC stress with live handler/error state | `effects_suspend_inside_finally_with_pending_errors_and_gc_stress` compares stress and non-stress continuation bytes and resumes the pending heap error |
| Renderer refusal | `the_renderer_declines_try_and_throw_at_every_nesting` |
| AST-only nesting bound | `stack_budget_most_expensive_ast_variant_at_the_nesting_cap` pins the 2 MiB budget at the cap with the most expensive per-level variant; `stack_budget_ast_try_finally_at_parser_max_depth` pins the new variants at the parser's cap; `ast_only_nesting_beyond_the_cap_is_a_typed_error_not_an_abort` drives real depths in child processes, because an abort cannot be caught in-process |
| The parser's cap stays inside the AST cap | `every_parsed_shape_the_parser_accepts_stays_inside_the_ast_cap` walks ten parsed shape families to the parser's refusal point and requires each accepted program to pass the depth check and to link; `the_worst_parsed_shape_stays_inside_the_ast_cap` reports the deepest tree the family can build |
| Loop control placement | `loop_control_outside_a_loop_is_a_typed_error_not_a_panic`; `a_bare_continue_at_the_program_root_is_a_typed_error` |
| Deferred process terminals | `finish_inside_a_try_does_not_run_the_finally`; `fail_inside_a_try_does_not_run_the_finally` pin the deferral in process mode |
| Taxonomy and code stability | `every_runtime_error_display_is_exact` also pins every variant's guest-facing code through an exhaustive match |
| Public API route | `embedding_lashlang_functions::ast_only_exceptions_compile_and_execute` compiles and executes only through public AST APIs; `ast_construction_is_validated_before_compilation`, `runtime_errors_carry_a_code_and_a_taxonomy` and `a_suspended_cleanup_chain_exposes_its_exception_state` exercise the layer's remaining public surface, and all of it is now inventoried in `docs/api-example-coverage.toml` |

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

A second, decisive fresh-eyes verification of that fix round returned BLOCK.
Those findings are closed here, again red-side first.

| Finding | Severity | Status |
|---|---|---|
| The AST cap refuses parsed programs the parser accepts | BLOCKER | Fixed (one measured budget; parser cap 40 to 30) |
| A durable handler may name any scope in its function | BLOCKER | Fixed (scope anchored to a live code position) |
| `Finish`/`Fail` inside a `try` skip the `finally` | P2 | Deliberate deferral, now documented and pinned |
| `break`/`continue` in an AST-only function body panics `compile_ast` | P2 | Fixed (`validate_ast`) |
| New public API absent from the coverage registry | P3 | Fixed (12 symbols registered and exercised) |
| Breaking embedder changes carry no release note | P3 | Flagged below for the PR body |
| `InvalidExceptionState` classified catchable | P3 | Fixed (uncatchable terminal) |
| The code pin pins arms, not instances | P4 | Fixed (variant-count assertion) |

`break`/`continue` inside a `finally` body is implemented to ECMA-262
completion-replacement semantics rather than rejected at link time: the pending
completion is discarded by `AbandonFinally` and the jump continues, and the
enclosing scopes it still has to leave are emitted by the same handler-scope
walk.

## Verification results

| Command | Result |
|---|---|
| `cargo check --workspace --all-targets --locked` | Pass |
| `cargo test --workspace` | Pass, 136 test targets, 0 failures, run to the final doc-test target |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | Pass |
| `cargo fmt --all --check` | Pass |
| `python3 scripts/check_included_file_formatting.py` | Pass, 37 included files |
| `python3 scripts/lint_docs.py` | Pass, 46 HTML pages and 42 registry pages |
| `bash scripts/check-rustdoc.sh` | Pass, 599 documented public members, 0 missing |
| `python3 scripts/check_test_quarantines.py` | Pass |
| `python3 scripts/check_api_example_coverage.py` | Pass, 8,065 entries |
| `just perf-guard` | Pass, both legs with `--enforce-budgets` on this worktree's own target dir: 297 lashlang perf results plus 1 instruction profile, every scenario `within_stack_budget` against 2 MiB, no budget breached |
| `bash scripts/check-production-file-size.sh` | Pass |
| `git diff --check` | Pass |

## Release notes owed by the PR body

Two commits make breaking embedder changes without a `Release-Notes:` trailer of
their own, so the PR body must carry them:

- `0606e076e` makes `compile_ast` and `LinkedModule::link` return a `Result`.
  Every embedder constructing an AST has to handle the new typed error.
- `0b872cf1d` changes `RuntimeError`'s borrowed `&'static str` payloads to
  `Cow<'static, str>`, which breaks anyone matching on them against a literal,
  and pins the guest-visible `code()` strings as an explicit contract.

The nesting-limit change carries its own trailer on `3cfec80f9`.

## Deferred items

- **`finish` and `fail` inside a `try` do not run the `finally`.** This is a
  deliberate deferral, not an oversight. They are *process terminals*, not
  function returns; ECMA-262's analogue (`process.exit`) skips `finally` too,
  and deciding process-terminal-versus-completion semantics belongs in the layer
  that has a real `return` to test against, not in one that has none.
  **Constraint on FIG-1304/1305: a TypeScript `return` must lower to a real
  function return and never to `Expr::Finish`.** If it does, cleanups are
  dropped silently. `finish_inside_a_try_does_not_run_the_finally` and
  `fail_inside_a_try_does_not_run_the_finally` pin the current behaviour in
  process mode and name that constraint in their failure messages, so a lowering
  that violates it trips red. (In *foreground* mode `fail` instead raises the
  catchable `SessionProcessAdminOutsideProcess`, which does route through the
  handler stack and run the finally — an artifact of the mode check, and one
  more reason to pin rather than leave implicit.)
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
