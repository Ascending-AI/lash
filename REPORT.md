# FIG-1303 implementation report

Status: complete on `samuel-fig-1303`.

## Delivered commits

| Commit | Purpose |
|---|---|
| `c666242b3` | Add AST-only exceptions, exception bytecode, durable handler/finally state, VM unwinding, taxonomy enforcement, and conformance tests. |
| `1ecc7d41f` | Add the public AST integration example and register all new public API symbols. |

## Design

### Public AST and bytecode

The public AST now exposes `Expr::Try(Box<TryExpr>)`, `TryExpr`, `CatchClause`, and `Expr::Throw(Box<Expr>)`. Both nodes participate in traversal, folding, semantic hashing, host-requirement collection, linker validation/lowering, and type inference. The compiler lowers them to five real VM instructions:

- `PushHandler { handler, finally, catches }`
- `PopHandler`
- `EnterFinally { finally, resume }`
- `EndFinally`
- `Throw`

`heap_plan` remains the single exhaustive opcode contract table. `Throw` is heap-native because the thrown value remains in the VM heap; the four control-only instructions declare zero stack values at the host boundary. The exception instructions also have an explicit `Exception` profiler tag.

### Error value shape

Catchable `RuntimeError` values are imported into the normal heap as records. They use the following dialect-facing shape:

| Field | Value |
|---|---|
| `name` | `"EffectError"` for effect/tool failures, otherwise `"RuntimeError"` |
| `message` | Existing sanitized `RuntimeError` display text |
| `code` | Stable existing Rust variant identity, for example `LenUnsupported` or `UnwrappedModuleOperationFailed` |
| `details.kind` | `"effect"` or `"runtime"` |
| `details.instruction` | Bytecode instruction index at which the failure became a throw |
| `details.operation` | Structured sanitized operation name when the failing instruction is an effect site |

Explicit `throw value` transfers the original value unchanged. Error records use the same import, allocation metering, serialization, forest validation, and GC rules as every other record. Pending thrown values held by a finally continuation are included in VM roots.

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

A running finally records its pending completion (`Normal { resume_ip }` or `Throw { value }`), handler-stack depth, frame identity, and operand-stack depth. Both record families serialize in `VmContinuation`.

### Unwind algorithm

1. Import the thrown value into the current heap and locate the nearest catch-capable handler.
2. Run cleanup-only handlers above that catch from inner to outer. Each cleanup atomically restores the owning frame, operand stack, and iterator stack, then enters its finally with the pending throw recorded.
3. On `EndFinally`, either resume the saved normal target or continue throwing the saved value.
4. If code in a finally throws, discard that finally's prior pending completion and unwind the new value. This gives the required replacement semantics.
5. At a catch, restore the handler's frame/stack/iterator boundary, push the thrown value, and transfer to the handler target.
6. If no handler accepts an explicit throw, return typed `RuntimeError::UncaughtException`. Unwinding itself does not suspend; effects executed by ordinary finally code may suspend and serialize their pending completion.

Frame restoration uses the existing call-frame state and restores temporary iterator bindings as iterators are unwound. Occurrence counters are independent continuation state, so retry after catch allocates the next occurrence and replay key.

### Lashlang lowering decision

The textual lashlang surface remains unchanged. The parser has no `try`, `catch`, `finally`, or `throw` production, and the canonical renderer returns `NonSourceableExpression` for both new AST node families. Existing textual `?` still compiles to its existing unwrap instruction. When no AST-only handler surrounds it—which is true for every parsed lashlang program—its failure leaves the VM with the same typed error and diagnostics as before. An embedding or later dialect can place that existing effect site inside an AST-only `Try`, in which case the resumed effect failure becomes a throw at that site.

No prompt, parser, diagnostic, or benchmark snapshot changed.

## Taxonomy enforcement

| Layer | Catchable | VM route |
|---|---:|---|
| Effect/tool failure | Yes | Existing effect error becomes an `EffectError` heap record and enters normal throw unwinding. |
| Ordinary guest-reachable runtime error | Yes | Central `route_runtime_error` converts it to a `RuntimeError` heap record when an exception scope is active. |
| Instruction, deadline, memory, or frame-depth exhaustion | No | `RuntimeError::is_uncatchable_terminal` prevents handler lookup; instruction/deadline checks terminate directly from `enforce_execution_bounds`, while memory/frame terminals are rejected by the same central classification when raised at allocation/call sites. |
| Host cancellation | No | The cooperative `ExecutionHost::is_cancelled` probe terminates in `enforce_execution_bounds` as `HostCancelled`, before any handler lookup. |

`enforce_execution_bounds` documents the direct terminal boundary. `route_runtime_error` is the single defensive classification point for errors raised elsewhere in instruction execution. Tests wrap every terminal class in a catch returning a sentinel and prove the sentinel is never observed.

## Durable format changes

| Contract | Previous | New |
|---|---:|---:|
| VM continuation format | 2 | 3 |
| Compiled bytecode format | 4 | 5 |
| Lashlang VM ABI | `lashlang-vm-abi-v2` | `lashlang-vm-abi-v3` |

Continuation v3 requires `handler_stack` and `finally_stack`; absence is a decode error rather than a default. Structural validation rejects invalid frame identity, operand/iterator depth, handler/finally nesting, and pending values. Resume-time program validation additionally proves handler, finally, and normal-resume instruction pointers belong to the recorded function. Older versions fail closed through the existing exact format-version check.

## Test coverage

| Requirement | Coverage |
|---|---|
| Explicit throw and ordinary runtime catch | `explicit_throw_transfers_the_original_value_to_catch`; `runtime_errors_are_caught_as_heap_backed_error_records` |
| Tool/effect failure and structured operation metadata | `effect_failure_is_a_throw_with_structured_operation_metadata` |
| Normal, exceptional, nested, and replacement finally behavior | `finally_runs_on_normal_and_exceptional_paths_and_a_new_throw_replaces_the_old_one` |
| Cross-frame unwind | `throw_unwinds_function_frames_to_the_callers_handler` |
| Suspension inside try and finally | `continuation_round_trips_inside_try_and_finally`; `effects_suspend_inside_finally_with_pending_errors_and_gc_stress` |
| Every uncatchable terminal | `execution_terminals_bypass_a_surrounding_catch` covers instruction, deadline, memory, frame-depth, and cancellation terminals |
| N-frame/finally/iterator interaction | `exception_unwind_crosses_frames_finally_chains_and_iterators` |
| Retry occurrence and replay identity | `effect_failure_catch_retry_is_a_new_occurrence` proves occurrences advance from 1 to 2 |
| Malformed durable state | `malformed_exception_continuations_fail_closed` covers out-of-range handler, cross-function target, oversized stack base, invalid finally target, and frame-identity mismatch |
| Cross-process determinism | `independent_processes_dump_identical_exception_continuations` compares byte-identical dumps for suspension inside try, inside finally, and after a caught effect failure |
| GC stress with live handler/error state | `effects_suspend_inside_finally_with_pending_errors_and_gc_stress` compares stress and non-stress continuation bytes and resumes the pending heap error |
| Public API route | `embedding_lashlang_functions::ast_only_exceptions_compile_and_execute` compiles and executes only through public AST APIs |

### Existing test edits

All edits to pre-existing tests are representation-only:

- Authored continuation fixtures now state continuation format `3` and include empty `handler_stack` and `finally_stack` fields.
- Direct `VmContinuation` constructors initialize both new stacks as empty.
- The exception suite and public API example are new tests; they do not alter old expectations.
- No existing snapshot file changed, and no `.snap.new` file exists.

The existing lashlang library battery remains 422/422 green, including its parser, diagnostics, bytecode-contract, language, benchmark-contract, and snapshot coverage.

## Verification results

| Command | Result |
|---|---|
| `cargo check --workspace --all-targets --locked` | Pass |
| `cargo test --workspace` | Pass across every workspace test target and doctest; repository-declared ignored integration fixtures remain ignored |
| `cargo clippy --workspace --all-targets -- -D warnings` | Pass |
| `cargo fmt --all --check` | Pass |
| `python3 scripts/check_included_file_formatting.py` | Pass, 34 included files |
| `python3 scripts/lint_docs.py` | Pass, 46 HTML pages and 42 registry pages |
| `bash scripts/check-rustdoc.sh` | Pass, 599 documented public members and 0 missing |
| `python3 scripts/check_test_quarantines.py` | Pass |
| `python3 scripts/check_api_example_coverage.py` | Pass, 8,065 entries |
| `just perf-guard` | Pass |
| `bash scripts/check-production-file-size.sh` | Pass |
| `git diff --check` | Pass |

`just perf-guard` reported zero failures across 837 runtime budgets and 686 lashlang budgets. Both stack profiles stayed within the 2 MiB budget. Representative runtime medians were:

| Scenario | Total time | Total allocated bytes | Steady-turn allocated bytes |
|---|---:|---:|---:|
| `standard` | 14.907 ms | 21,527,401 | 4,563,818 |
| `rlm` | 20.901 ms | 29,562,441 | 7,007,100.5 |
| `standard_tool_calls` | 16.844 ms | 25,463,391 | 4,630,678 |

The lashlang guard produced 297 performance results plus its instruction profile; the profile measured 71,802 instructions/iteration against a 110,000 budget. No new performance scenario was added because the new exception instructions are a cold, previously nonexistent shape; the existing dispatch, deep-chain, function-frame, heap, and stack scenarios cover the changed shared machinery.

## Deferred items

No FIG-1303 requirement is deferred. Intentionally later or out of scope:

- TS syntax and ECMA-262 lowering will consume the AST/bytecode capability in its later layer.
- Textual lashlang exception syntax remains absent by design.
- Suspension during the atomic unwind operation remains unsupported as explicitly allowed; suspension inside try and finally code is implemented.
