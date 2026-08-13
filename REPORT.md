# FIG-1302 — VM user functions and closures

## Result

Implemented VM-level user functions, heap-backed closures, stackless calls,
recursion, an internal callback-capable `map`, and durable mid-call
continuations. The public AST/compiler route is covered by an integration
example. The lashlang parser, grammar, prompt, model-visible builtin registry,
and surface-language battery were not changed.

## Design

### Public AST and compiled functions

The public AST now has `FunctionExpr`, `Expr::Function`, `Expr::Call`, and
`Expr::Map`. These are deliberately AST-only. Canonical source rendering
returns `NonSourceableExpression` for them and no parser production or prompt
inventory recognizes them. `Expr::Function` boxes its payload so the recursive
AST remains 72 bytes; this preserves the existing 2 MiB-stack deep-expression
benchmark contract.

Compilation appends each function body to one module code vector and records a
stable entry in the module function table. Each entry contains its entry PC,
parameter/capture counts, optional self slot, parameter and capture slot maps,
and local slot names. The root code length remains explicit, so falling off the
root does not execute appended function bodies. Named functions receive their
own closure in the self slot, which enables recursion without a host/global
lookup.

### Function values and captures

A function value is a heap `Closure { function: u32, captures: Vec<Value> }`.
The function index selects code from the supplied content-addressed compiled
program; code is never serialized inside a closure. Closure members participate
in tracing, isolation, equality, logical-byte accounting, canonical snapshots,
and continuation wires like other heap objects.

The VM stores the capture values it receives without imposing a copy policy.
Lashlang lowering emits `DeepCopy` for every declared capture at closure
creation, and for function arguments, giving this dialect deep by-value
semantics. A future dialect can intentionally pass `Value::Ref` captures to the
same VM mechanism.

Function values are VM-private. They remain in runtime globals and the heap so
they can survive a state snapshot, but are omitted from the host-materialized
globals view. An attempted direct export is the typed
`FunctionValueAtHostBoundary` error.

### Flat call frames

Guest calls never recurse through Rust. `Call` swaps in a new local `SlotState`,
pushes a VM `CallFrame`, jumps the interpreter PC, and returns to the same flat
run loop. A frame records:

- return PC and caller function identity;
- operand-stack base;
- caller slots, projected-slot bits, and extra globals;
- caller iterator stack and heapification state;
- direct-return or builtin-map return target.

`Return` restores that state and either pushes the result to the caller or
continues the builtin callback state machine. `ExecutionBounds` has an
independent `max_frame_depth`, defaulting to 1,024. Exceeding it produces the
typed terminal execution-bound error `FrameDepthExceeded`; it cannot overflow
the Rust stack.

### Builtin callback reentry

`Expr::Map` is the minimal internal callback proof. It is not registered as a
model-visible lashlang builtin. The `Map` opcode retains the closure, input
items, next cursor, and completed results in a frame return target. Each pure
callback re-enters the ordinary interpreter through `begin_function_call`, so
frame/heap/meter/occurrence state stays coherent and can be suspended and
serialized mid-map.

Effects inside a map callback are rejected with the typed
`EffectInBuiltinCallback` runtime error. Normal user functions may perform an
effect and suspend at any call depth; only the builtin-owned callback case is
restricted until a resumable effectful-builtin protocol exists.

### Continuation and replay discipline

`VmContinuation` now carries its format version, active function identity, and
a serializable `VmFrameContinuation` stack. Each frame preserves its slots,
projected bits, extra globals, iterators, operand base, return PC, and direct or
map return state. Suspend collects the entire reachable heap; resume validates
format, function indices, slot shapes, PCs, operand bases, iterator/map cursors,
heap ownership, and the active frame-depth bound before running.

Call instructions are execution sites. Their occurrence counters are retained
through continuation serialization, increase under recursion and repeated
builtin callbacks, and resume without changing replay identity. Independent OS
processes produce byte-identical normalized mid-recursion continuation dumps.

### Metering and GC

Function body instructions consume the ordinary instruction budget.
`MakeClosure`, explicit `Call`, `Map`, and `Return` each cost one instruction;
each subsequent builtin-initiated callback frame push adds one instruction.
Instruction profiles expose separate `make_closure`, `call`, `callback`, and
`return` tags. Instruction, elapsed-time, allocation-counter, and logical-byte
meters continue monotonically across resume.

Closure size is the existing object header plus four bytes for the function
index plus the existing logical cost of each capture value. This is an additive
new object kind; no existing cost changed, so heap size schedule v1 remains
valid. Closure capture references are traced, and collection on every
allocation produces the same recursive result as normal collection.

## Format cutovers

| Contract | Before | After | Reason |
| --- | ---: | ---: | --- |
| Bytecode | 3 | 4 | New closure/call/map/return opcodes and function table |
| Lashlang VM ABI | v1 | v2 | Compiled artifact semantics now include functions |
| VM continuation | implicit v1 | explicit v2 | Active function and serializable frame stack |
| Canonical lashlang snapshot | 2 | 3 | Closure heap object wire |
| RLM execution snapshot | 9 | 10 | Embeds the v3 lashlang snapshot/frame-capable runtime |
| Heap size schedule | 1 | 1 | Additive closure kind; existing costs unchanged |

All changed durable formats fail closed. No compatibility decoder or shim was
added.

## New test suites

The following named tests pass:

- `public_ast_constructs_and_calls_a_capturing_function`
- `user_function_closure_capture_is_deep_by_value_and_recursion_is_stackless`
- `builtin_map_reenters_the_flat_vm_and_rejects_effectful_callbacks`
- `effect_suspension_inside_a_user_function_round_trips_the_frame_stack`
- `frame_depth_is_a_typed_execution_bound_and_gc_stress_preserves_closures`
- `closure_heap_objects_round_trip_through_state_snapshot_v3`
- `continuation_round_trip_mid_recursion_preserves_frames_heap_and_meter`
- `recursive_calls_keep_occurrence_counters_stable_across_resume`
- `builtin_callback_continuation_preserves_reentry_and_occurrence_counters`
- `independent_os_processes_dump_identical_mid_recursion_continuations`

The closure snapshot test resumes the same compiled program after canonical
snapshot decode and calls the restored closure. The process determinism test
spawns two independent copies of the test executable and compares exact
normalized continuation bytes.

## Existing test edits

| Existing test file | Edit | Justification |
| --- | --- | --- |
| `crates/lashlang/src/runtime/error.rs` | Extended the exact-display matrix | Exhaustive representation oracle for the new typed runtime errors |
| `crates/lashlang/src/runtime/tests.rs` | Added formatter arms and included the new suite | Exhaustive representation support for new opcodes; no old expectation changed |
| `crates/lashlang/src/runtime/tests/continuation_cases.rs` | Added v2 fields to one authored struct | Required representation-only continuation shape update |
| `crates/lashlang/src/runtime/tests/continuation_wire_cases.rs` | Added version, active-function, and empty-frame fields | Required representation-only v2 wire update; ownership assertions unchanged |
| `crates/lashlang/src/runtime/state/tests.rs` | Updated canonical version byte/hash | Required representation-only v3 snapshot golden update |
| `crates/lash-protocol-rlm/src/executor/state/tests.rs` | Updated root golden and embedded leaf identity | Required representation-only v10/v3 checkpoint update |
| `crates/lash/src/tests/core_session_builder/session_lifecycle.rs` | Expected version 10 in cold-open rejection | Required representation-only operator-message update |

No parser or model-visible lashlang test was changed.

## Verification

| Command | Result |
| --- | --- |
| `cargo check --workspace --all-targets --locked` | PASS |
| `cargo test --workspace` | PASS after intentional format-golden updates |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | PASS |
| `cargo fmt --all --check` | PASS |
| `python3 scripts/lint_docs.py` | PASS — 46 HTML pages, 42 registry pages |
| `bash scripts/check-rustdoc.sh` | PASS — 599 documented public members, 0 missing |
| `python3 scripts/check_test_quarantines.py` | PASS |
| `python3 scripts/check_api_example_coverage.py` | PASS — 8,065 entries; public AST use also has an integration example |
| `just perf-guard` | PASS — no budget changes |

The complete lashlang unit battery passes with 396 tests, plus the new public
AST integration test. The existing surface-language/parser goldens remain
green without edits.

## Performance

`just perf-guard` produced 837 runtime budget checks and 662 lashlang budget
checks, with zero failures.

| Measurement | Median | Budget |
| --- | ---: | ---: |
| Standard runtime total | 14.682 ms | 10,000 ms |
| RLM runtime total | 19.889 ms | 10,000 ms |
| RLM lashlang execute phase | 0.245 ms | 5 ms |
| Lashlang baseline one-shot | 319,998 ns/iter | guarded by checked-in profile |
| Lashlang baseline compiled execute | 51,388 ns/iter | guarded by checked-in profile |
| Standard total allocations | 21,527,401 bytes | 1,000,000,000 bytes |
| RLM total allocations | 29,562,222 bytes | 1,000,000,000 bytes |

No performance budget or baseline was modified.

## Deferred items

- Surface syntax, parser grammar, prompt documentation, and model-visible
  builtins remain deferred by scope ruling.
- Effectful builtin callbacks remain a typed limitation. Ordinary effectful
  functions are durable at arbitrary depth.
- Additional callback combinators such as filter/reduce/sort are deferred; the
  internal map proves the shared reentry mechanism.
- Reference/cell capture semantics are deferred to a future dialect. The VM can
  already store reference-valued captures; lashlang lowering intentionally
  deep-copies them.
- Catchability of frame-depth exhaustion remains for the exceptions layer; it
  is terminal today.
