# FIG-1302 — VM user functions and closures, fix round

## Result

The two blocking reviews and the decisive verification's five final residuals
are reconciled, and every confirmed finding is fixed. The functions/closures
layer now has one frame-aware root model for collection and serialization,
program-aware continuation validation, a non-panicking host boundary, explicit
performance coverage, and checked public-AST examples. The parser, grammar,
prompt, model-visible builtin registry, and surface language remain unchanged.

## Red-first reconciliation

Opus's exact repro was promoted to
`effect_suspension_inside_a_user_function_with_heap_argument_round_trips` and
committed alone in `c8af81b83` before the fix. The equivalent AST is:

```text
f = function(n) { print(1); n }; finish f([7, 8])
```

It reproduced Opus's result. Execution reached the effect, but `Vm::suspend()`
failed with:

```text
UnserializableValue {
    location: "continuation heap: heap wire must not contain unreachable objects",
    variant: "invalid heap object graph",
}
```

The callee's list argument was live in the active function slots, while the wire
root collector omitted active slots/globals whenever `active_function` was set.
The test passed after the shared root-enumerator change in `ad6e5f275`.

## Fixes

### One frame-aware root enumerator

`runtime/vm/roots.rs` is now the sole enumerator for both GC roots and
continuation-wire roots. It walks active slots/globals, the shared operand
stack, `last_value`, active iterators, and every saved frame's slots/globals,
iterators, and Map return state.

Roots are classified as durable or transient. The saved root frame is durable;
the active callee and saved function frames are transient borrowers. ADR-0059
now records this deliberate qualification of the forest invariant: a named
function's `self_slot` may alias its caller-owned closure, and operands may
temporarily retain the same heap value. This classification is shared by GC and
wire validation, so they cannot diverge again.

Coverage includes list/record/tuple arguments and locals at suspension, plus a
suspension where caller and callee simultaneously retain heap-backed state.

### Program-aware restore validation

Compiled code ranges are root `0..root_code_len` plus an explicit
`entry_ip..end_ip` stored on every compiled function. Validation therefore has
no dependency on function-table ordering. A suspended active instruction
pointer may equal its owning range end: root fallthrough completes the program,
while function fallthrough performs the function's natural return. Saved return
pointers remain legal only inside the caller range and immediately after a
`Call` or `Map`. Resume rejects:

- an active PC outside the active function's range plus its accepted end
  boundary;
- a saved return PC outside the recorded caller's exact range;
- cross-root, non-boundary root-to-function, function-to-root, and sibling
  function substitutions;
- return PCs that are not legal post-`Call`/post-`Map` sites.

Closure validation walks the entire reachable graph before execution and checks
both the function index and exact capture count against the compiled function
table. It covers active state, saved frames, globals, and nested containers, and
runs both on continuation resume and when a decoded snapshot is paired with a
program. Invalid indices now produce the live, accurately named
`UnknownFunction` error; capture mismatches are typed errors rather than a
use-site assertion.

Iterator validation is centralized and applied to the active state and every
frame at decode and resume. Binding slots are checked against that frame's slot
count and range steps must be nonzero. The duplicate top-level
iterator-to-continuation conversion was removed.

### Host boundaries and formatting

`Vm::into_globals` is now fallible and uses the same materialization policy as
state installation. A whole global binding is omitted if a closure occurs at
any depth; ordinary neighboring bindings remain. This cannot panic.

The omission is intentional and documented in ADR-0059: snapshots and
continuations preserve closures, but a host that round-trips only materialized
globals silently drops closure-bearing bindings. Direct boundary uses—effect
arguments, finish, wake, yield, JSON conversion, formatting, projection, and
schema validation—return `FunctionValueAtHostBoundary`. A checkpoint remains a
valid durable boundary and preserves the closure.

`FormatCompiled(index)` heap planning now reads the selected template's actual
argument count instead of treating the template index as arity. Tests cover
indices below, equal to, and above argc with scalar, heap-backed, and closure
arguments; closures normalize to `FunctionValueAtHostBoundary`.

### Heap-plan and Map decisions

The old conservative heap-plan default and its runtime guard stay deleted. The
instruction match is exhaustive, and its source comment makes the contract
explicit: adding an opcode must make a compile error until its heap behavior is
classified. Compile-enforced exhaustiveness is the stronger replacement for a
runtime default that could silently assign the wrong plan.

`BuildTuple`, `BuildList`, and `BuildRecord` deliberately use `heap_native`
rather than `Top(len)`. Lowering emits `DeepCopy` for every literal member, so
the builders receive already-isolated values and must assemble those values
without exporting heap references back into trees. Exporting the whole operand
window would add avoidable materialization and weaken the direct heap contract.
`shared_binding_list_and_record_literals_remain_independent_after_snapshot_round_trip`
commits the PB6 shape: list and record literals repeat one heap-backed binding,
the original is mutated, and canonical encode/decode proves both literal copies
remain independent.

The two generic `fixed_argc()` fallthroughs no longer assert compiler
invariants with `expect`. Heap planning and intrinsic dispatch now return
`ContextDependentIntrinsicMisdispatch`, a typed runtime error, if a future
context-dependent intrinsic lacks its required explicit arm.

Map no longer isolates an item before `begin_function_call`, because ordinary
argument lowering already performs the required isolation. The redundant deep
copy and its metering cost were removed.

### API, workflow graph, and performance surface

Malformed-state constructors remain test-local. The public surface was narrowed
where possible; intentional exports are dispositioned in the API example
coverage registry. `compile_ast(&Program)`, function AST construction, and
`VmContinuation::frame_depth()` are exercised by the checked
`embedding_lashlang_functions` docs snippet. Low-level wire/version items are
explicitly recorded as checked rather than pretending to be ordinary user APIs.

The workflow graph remains a source-only projection by conscious acceptance.
Its entry point now documents why AST-only `Function`, `Call`, and `Map` cannot
reach its generic expression arms: both paths parse source and canonicalization
rejects these nodes as `NonSourceableExpression`.

An AST benchmark entry point now measures noncapturing and captured calls,
768-deep flat recursion, Map at 64/256/1024 elements, and a 512-frame
allocation/logical-byte scenario. Checked budgets and a 16x Map-size linearity
ratio were added to the normal performance guard.

## Durable format cutovers

These are unchanged by the fix round:

| Contract | Before | After | Reason |
| --- | ---: | ---: | --- |
| Bytecode | 3 | 4 | Closure/call/map/return opcodes and function table |
| Lashlang VM ABI | v1 | v2 | Compiled artifact semantics include functions |
| VM continuation | implicit v1 | explicit v2 | Active function and serializable frame stack |
| Canonical Lashlang snapshot | 2 | 3 | Closure heap-object wire |
| RLM execution snapshot | 9 | 10 | Embeds the frame-capable v3 Lashlang snapshot |
| Heap size schedule | 1 | 1 | Additive closure kind; existing costs unchanged |

Changed durable formats fail closed; no compatibility decoder or shim exists.

## Committed regression battery

The fix round adds or extends these named probes:

- `effect_suspension_inside_a_user_function_with_heap_argument_round_trips`
- `suspended_caller_and_callee_preserve_heap_arguments_and_locals`
- `resume_rejects_cross_function_active_and_return_instruction_pointers`
- `continuation_decode_rejects_an_active_function_without_a_root_frame`
- `shared_binding_list_and_record_literals_remain_independent_after_snapshot_round_trip`
- `caller_and_callee_iterators_round_trip_and_corrupt_frames_fail_closed`
- `resume_validates_closures_in_active_frames_globals_and_nested_containers`
- `resume_reports_unknown_closure_function_indices_by_name`
- `decoded_snapshots_validate_closure_metadata_when_paired_with_a_program`
- `vm_into_globals_omits_top_level_and_nested_closures_without_panicking`
- `compiled_format_uses_template_arity_for_scalar_heap_and_closure_arguments`
- `default_frame_depth_rejects_fifteen_hundred_recursive_calls`
- `closures_obey_the_complete_host_boundary_matrix`
- the checked `embedding_lashlang_functions` public-AST example

The original function suite also remains green: closure capture and stackless
recursion, Map callback re-entry and effect rejection, suspension at arbitrary
call depth, frame limits and GC stress, snapshot/continuation round trips,
occurrence-counter stability, and independent-process continuation determinism.

## Existing-test and contract edits

| Area | Edit and justification |
| --- | --- |
| Runtime error display matrix | Added exact displays for typed program/closure/frame validation errors. |
| Continuation and wire fixtures | Updated authored v2 frame fields and added adversarial active/frame/snapshot mutations. |
| Canonical and RLM snapshot goldens | Updated only for the already-declared v3/v10 format cutover. |
| Heap-plan test | Replaced the conservative-default runtime expectation with an exhaustive compiler match and source contract. This is an intentional behavior-test change. |
| Function/host tests | Added the review repros, complete closure-boundary matrix, and default-depth probe. |
| API coverage | Added checked AST documentation and explicit low-level dispositions; narrowed nonessential visibility. |
| Performance guard | Added seven function scenarios, per-metric ceilings, heap/live-byte ceilings, and Map scaling ratio. Existing budgets were not loosened. |

No parser, grammar, prompt, or model-visible Lashlang test was changed.

## Final polish dispositions

| Residual | Disposition |
| --- | --- |
| `ip == range.end` acceptance | Fixed. Active root and function PCs accept the owning end boundary; the run loop treats function end as an implicit `Return`, and the updated cross-range regression proves root and function completion. |
| Function-table ordering assumption | Fixed. `CompiledFunction` owns an explicit `end_ip`; range validation no longer consults the next table entry. |
| `Build*` to `heap_native` rationale and PB6 | Fixed. The plan-change rationale is recorded above, and the shared-binding list/record mutation survives canonical encode/decode in a committed regression. |
| `fixed_argc()` `expect` calls | Fixed. Both sites propagate the typed `ContextDependentIntrinsicMisdispatch` runtime error. |
| Bottom-frame owner check | Fixed. Decode explicitly requires an active function's bottom frame to be root-owned, with a scalar-only malformed wire proving rejection independent of heap reachability. |

## Performance

Release measurements from the final 500-iteration `just perf-guard` run:

| Scenario | ns/iter | allocs/iter | bytes/iter | Time budget |
| --- | ---: | ---: | ---: | ---: |
| Noncapturing call | 2,308 | 17 | 4,641 | 100,000 ns |
| Captured call | 4,285 | 51 | 8,863 | 150,000 ns |
| Recursion, depth 768 | 520,931 | 2,329 | 847,490 | 3,000,000 ns |
| Map, 64 items | 31,694 | 246 | 47,332 | 400,000 ns |
| Map, 256 items | 119,490 | 830 | 170,404 | 1,200,000 ns |
| Map, 1,024 items | 505,702 | 3,142 | 662,692 | 4,000,000 ns |
| Frame-heavy, depth 512 | 519,766 | 7,315 | 1,671,778 | 4,000,000 ns |

Map's measured 1,024/64 ratio is 15.96 for a 16x input increase, below the 20x
guard. The frame-heavy case reached depth 513 with 3 heap allocations and 228
live logical bytes, below budgets of 8 and 1,024 respectively.

The same run measured standard runtime total at 13.997 ms, RLM total at 20.468
ms, standard allocations at 21,527,401 bytes, and RLM allocations at 29,562,195
bytes. All 837 runtime and 686 Lashlang budget checks passed.

## Verification

| Command | Result |
| --- | --- |
| `cargo check --workspace --all-targets --locked` | PASS |
| `cargo test -p lashlang` | PASS; 409 unit tests plus all integration, property, prompt, stack-budget, value-semantics, and benchmark-contract tests |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | PASS |
| `cargo fmt --all --check` | PASS |
| `python3 scripts/check_included_file_formatting.py` | PASS; 32 included files |
| `python3 scripts/lint_docs.py` | PASS; 46 HTML pages, 42 registry pages |
| `bash scripts/check-rustdoc.sh` | PASS; 599 public members, 0 missing |
| `python3 scripts/check_test_quarantines.py` | PASS |
| `python3 scripts/check_api_example_coverage.py` | PASS; 8,065 host entries plus checked low-level dispositions |
| `just perf-guard` | PASS; 837 runtime + 686 Lashlang checks, zero failures |

The updated range-end regression, malformed bottom-frame wire, and PB6
shared-binding snapshot regression were also rerun directly and pass.

## Reviewable commits

1. `c8af81b83` — commit the exact suspension repro red.
2. `ad6e5f275` — introduce the shared active/parked root model and turn it green.
3. `0e81ed1b4` — validate restored PCs, closures, iterators, and state/program pairing.
4. `5ed908d4c` — close host materialization, formatting, heap-plan, Map, workflow, and ADR gaps.
5. `ade99bac3` — add the boundary battery, public example/registry coverage, and performance guards.

## Deliberately deferred scope

- Surface syntax, parser grammar, prompt documentation, and model-visible
  builtins remain deferred by the original scope ruling.
- Effectful builtin callbacks remain a typed limitation; ordinary effectful
  functions are durable at arbitrary depth.
- Additional callback combinators and reference/cell capture semantics remain
  future dialect work.
- Frame-depth exhaustion remains terminal pending the exceptions layer.
