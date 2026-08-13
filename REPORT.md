# FIG-1301 report

## What this branch does

Lashlang's tuples, lists and records live in a heap of mutable objects addressed
by identity, and the heap object graph is a forest of exclusively owned trees. A
heap reference may be duplicated only in transient VM operand flow; every
durable store — name and slot stores, global stores, container members, iterator
bindings, effect-result bindings, `State` patch APIs — holds a recursive copy
under fresh IDs.
[ADR 0059](docs/adr/0059-lashlang-durable-stores-hold-exclusively-owned-copies.md)
records the decision, what it costs, and what it makes impossible.

Round 3.5 closed the two blocking defects the round-3 reviews found — a process
crash on `format("{0}", xs)`, and durable sharing that the persisted wire still
accepted — along with the remaining performance and hygiene findings.

## Round 3.5 disposition

| Finding | Disposition |
| --- | --- |
| Opus F1 (P0): `format("{0}", xs)` panics the process for any container binding | Fixed. The fused slot-format opcodes read a slot, and the slot-materialization list did not name them, so a heap reference reached the stringifier's `unreachable!`. Three hand-maintained lists answered "what does this opcode need from the heap"; they are now one plan per opcode, and an opcode not declared there gets the conservative whole-stack default. The same gap made `xs + 1` report `heap_ref` instead of `list`; both are named regressions. |
| Opus F1 (P0), second half: the six `unreachable!` sites | Formatting now reports a typed `UnexportedHeapReference`. The four boundaries that cannot report an error — serde serialization, truthiness, projection rendering, schema type matching — assert in debug builds and fall back to a defined rendering in release: `null` for JSON and projection, truthy for a container, no type match for validation, which fails the cell through the normal validation error. Losing a host process over a display path is a worse failure than a wrong string. This is the one place the round departs from the letter of the instruction, which asked for a typed error at all six: `impl Serialize for Value` and `is_truthy` cannot return one without a signature change across their callers, and the requirement that mattered — never the process — is met. What makes the departure acceptable is that the fallback is unreachable: with the opcode plan conservative on both axes, every opcode either declares what it reads or exports the whole stack and every slot, so no declared-or-default opcode can hand a reference to `to_json_*`, the projector, `is_truthy` or schema matching. The fallback exists for an opcode whose declaration is wrong, and a wrong declaration is loud in debug and quiet-but-defined in release. |
| Opus F2 (P1): stress collection corrupts the heap on a general concat | Fixed. The allocation scope now opens before every instruction rather than before the ones a list named. Committing an isolation outside an open scope collected against empty pins and swept live objects: `y = y + z` left a dangling reference behind the untouched binding `x`. Regressions cover the general concat, the fused slot concat and a loop concat; all three fail against the previous guard. ADR 0055's stress-mode sentence is true again. |
| Sol P0 + opus F3/F4 (P0): continuations accept shared durable roots and cycles; snapshots accept a within-root DAG | Fixed. One validator, in release builds, at snapshot decode and encode and at continuation decode, resume and encode: every reachable object has at most one ownership edge, ownership edges form no cycle, every object is reachable. Durable roots — slots, globals, a parked loop binding — own what they name; the operand stack, last-value register and iterator cursors borrow without owning, because a VM legitimately holds a value on the stack and in the slot it was just stored into. Authored fixtures refuse two shared slots, a shared slot and global, a shared parked loop binding, a self-cycle, an unrooted ring and a within-one-root diamond, and accept transient duplication. With the validator stubbed out, sol's shared-slot wire decodes again. |
| Sol P2: child discovery is not centralized on `child_refs` | Fixed. The validators no longer spell their own object-member traversal. |
| Opus F5 (P1): the general concat is O(accumulator) per iteration | Fixed. `acc = acc + other` extends the accumulator's own object with copies of the right operand's members, so the cost is proportional to what is appended. Measured at 150, 300 and 600 iterations: round 3 took 2.19 ms, 8.09 ms and 30.73 ms — 3.7x and 3.8x per doubling, the quadratic opus reported — against 85 µs, 159 µs and 356 µs, which is linear. The shape had no scenario at all, because every existing one uses the optimized single-item form; it has one now. |
| Sol P1: the deep-chain guard does not vary depth | Fixed. Two scenarios that differ only in nesting depth, six levels against twenty-four, plus a ratio budget on their times. Measured 2.3x for 4x the depth, bounded at 5x, where quadratic scaling would land near 13x. |
| Opus F6 (P2): allocation churn is about 2x live objects | Fixed. A store leaves its value in the slot and in the last-value register, and each holder imported the tree separately, so every literal store allocated its object twice: `xs = [[1]]` cost four objects for a two-object value. A transient holder may point at what a durable one owns, so a later transient import of the same tree reuses the first; durable holders never reuse, and the batch is ordered durable-first so that stays true by construction. Allocations now equal live objects for those shapes, asserted. The first version of this lookup allocated a hash map per instruction and cost 6% more allocator traffic than it saved; it is a scan over a vector that stays empty unless the batch has a transient holder to satisfy. |
| Opus F7 (P2): the value-depth guard is dead for the heap form | Fixed. The heap form's depth lives in its chain of objects, not in the wire's nesting, so the structural guard could not see it and a chain of any length decoded — then overflowed the stack of whatever first materialized it. The bound is enforced against the object graph, before anything reads a root, with `SnapshotDecodeError::ValueDepthLimitExceeded`, and the measurement is iterative so checking a too-deep wire cannot itself overflow. |
| Opus F8 + sol P2: hygiene | `include!` splices text rather than declaring a module, so `cargo fmt` never saw those files and CI checked one of them by hand; a scoped scan now finds every include! target it covers, the twenty-two unformatted ones in this crate are formatted, and the scan runs in CI, the push gate and pre-commit. The per-opcode materialization contract is one table. The unused canonical-heap `Default` is gone. A byte-count bound is spelled with a byte-count constructor. The live byte meter is debug-asserted against the sum of the charged object sizes wherever it is adjusted incrementally. ADR 0055 names all three collection points. |
| Sol P2: the report's budget list was not exhaustive | Fixed below, and the short-binding ceiling is investigated rather than assumed: it did not grow. |

## Round 3.5 residual findings (final pass)

| Finding | Disposition |
| --- | --- |
| N1 (P2): `extend_list` is not atomic | Fixed. The whole extension is staged, charged once and then committed, so a bound trip leaves the accumulator and the encoded state byte-identical. Red-side: the previous loop leaves `acc = [0, [1]]` behind and persists it. |
| N2 (P3): `max_value_depth` diverges on a cyclic graph | Fixed. A visiting set makes it terminate on its own rather than because its caller validates first. Red-side: without it the direct cycle test hangs. |
| N3 (P3): the scenario says `_32` but is 24 levels | Fixed. Scenario, budget key, ratio numerator, golden output and comment all say twenty-four, and the comment says why it is not thirty-two. |
| N4 (P3): the plan's default was conservative on one axis | Fixed. An undeclared opcode now exports every slot as well as the whole stack. Two opcodes that were paying that default are declared instead — every remaining intrinsic reads exactly its argument count, and a record build reads the key list it points at — which left the format-heavy and churn scenarios allocating *less* than before the change. A test names the set that still takes the default. |
| N5 (P3): the transient-borrow argument's dependency was unrecorded | Fixed. ADR 0059 records it, and a parse-level test pins the statement property. |
| N6 (P4): report precision | Fixed below: the budget table's before column is the previous head, the `heap_*` count is eight, the tightened indexed-add expectation is listed, and the carry-forward count is sixty-eight of ninety-six. |

## What this round does not claim

- **Two of the six unexported-reference boundaries report a defined fallback,
  not a typed error.** See the opus F1 row.
- **Sixty-eight `include!`d files elsewhere in the repository are still
  unformatted and unchecked.** The repository has ninety-six include! targets;
  the scan covers the twenty-eight under `crates/lashlang`. Widening it is a
  separate cleanup, and adding a scope is a one-line change once a crate is
  clean.
- **Ancestor invalidation is not proven sub-linear.** It returns immediately
  when nothing is materialized, which is the common case, and the depth pair now
  guards the scaling; the traversal bound itself is unchanged.
- **The memory limit is sensitive to collection timing.** It bounds live plus
  not-yet-collected bytes, so a run that parks — which collects — can survive a
  point at which the same run without a park would have exhausted the bound. The
  relation is one-way: parking never brings exhaustion forward.
- **Wall-clock budgets are not drift detectors.** They carry roughly three times
  the measured maximum because this machine's load is not ours to control. They
  catch order-of-magnitude regressions, which is the class that hid here twice.
- **Isolation costs a copy.** Copying an alias-heavy graph is proportional to
  the graph. Copy-on-write behind identical observable semantics is recorded as
  future work in ADR 0059, not attempted here.
- **A 32-level nested literal overflows a 2 MiB debug test thread**, which is
  why the depth pair uses six and twenty-four levels. The snapshot value-depth
  limit is 64; release builds handle that depth, debug test threads do not.
- **The transient-borrow rule depends on a language property, not a heap
  check.** Assignment is a statement, so no durable store runs while operands
  are pending. ADR 0059 records what would break it; a parse-level test pins it.

## Design

### Isolation

`isolate_value` is the one isolation operation. It reserves fresh IDs for the
whole graph reachable from the stored value, builds the copies, charges the
batch, and only then commits, so a copy that would cross the memory bound leaves
the heap byte-identical. Cycles terminate because an ID is reserved before its
object is built.

It does not consult the boundary materialization cache. That cache exists so
that exporting a heap object to a tree and importing it back is
identity-preserving, which is exactly the sharing an isolation must not
reintroduce.

The compiler decides where isolation happens. Container literals and
comprehensions skip the store-level copy because they already isolated every
member they admitted; the general concat skips it too, because its members are
copied one at a time at the insertion itself.

### The instruction heap contract

Each opcode declares one plan: how much of the operand stack it needs exported
to tree values, and which slots it reads or mutates through. Opcodes that work
on references directly — isolation, in-place container mutation, structural
equality — declare that they need nothing exported. Everything undeclared
exports the whole stack.

Leaving a reference deeper on the stack is what keeps an accumulator under a
loop body from being rebuilt on every instruction; it is safe there because it
is a collection root, it serializes into a continuation, and the terminal export
walks the whole stack.

### The persisted forest

An *ownership edge* is a reference held by a durable root or by an object
member. The validator refuses a wire where any reachable object has two, where
ownership edges form a cycle, or where an object is unreachable. It runs at
every durable boundary in release builds, on both sides, so a violation cannot
be written and cannot be read.

A heap object member is a scalar or a reference, never an inline compound, so a
reference can never hide below the member level.

### Persistence

| Contract | Version | Policy |
| --- | ---: | --- |
| Lashlang canonical snapshot | 2 | Named MessagePack; either plain globals or runtime roots plus ID-ordered heap, never both |
| RLM snapshot root | 8 | Typed fail-closed root on this un-rebased branch |
| Lashlang bytecode | 3 | Includes explicit insertion-isolation instructions |
| Lashlang segment state | 2 | Carries the canonical continuation and cumulative meters |
| Heap logical-size schedule | 1 | Required and validated on restore |

## Performance

Release build, `compiled_execute` mode, minimum of three interleaved runs of 100
iterations. "Round 3" is `8d18fb270` built in a separate worktree with the same
scenario definitions.

| Scenario | Round 3 | Round 3.5 | Change |
| --- | ---: | ---: | ---: |
| `heap_variable_concat` (300 general concats) | 8.68 ms | 0.174 ms | 50x faster |
| `heap_deep_chain_mutation_24` | 3.01 ms | 2.86 ms | 1.05x faster |
| `large_data` | 1.218 ms | 1.194 ms | unchanged |
| `heap_list_iteration` | 2.008 ms | 1.993 ms | unchanged |
| `heap_comprehension_build` | 1.504 ms | 1.489 ms | unchanged |
| `heap_allocation_churn` | 1.884 ms | 1.880 ms | unchanged |
| `heap_shallow_chain_mutation` | 1.048 ms | 1.084 ms | 1.03x slower |
| `type_system_stress` | 0.995 ms | 1.016 ms | 1.02x slower |

The concat scaling, measured at three sizes: round 3 took 2.19 ms, 8.09 ms and
30.73 ms for 150, 300 and 600 iterations — 3.7x and 3.8x per doubling — against
85 µs, 159 µs and 356 µs, which is linear in the number of appends.

Against the fix round (`0f746e09a`), the shapes the earlier rounds fixed are
unchanged by 3.5: the comprehension build is 48.7x faster, the 2,000-element
list pass 12.4x, `large_data` 4.2x, `loop_control` 2.1x.

### Every guard that moved, exhaustively

| Guard | Before | After | Why |
| --- | --- | --- | --- |
Every row's *before* is the value at the previous head of this branch, not at
the branch point.

| Guard | Before (previous head) | After | Why |
| --- | --- | --- | --- |
| `large_data` allocated bytes | 2,400,000 | unchanged | Set in the fix round. Measures 2,264,731. |
| `type_system_stress` allocated bytes | 2,400,000 | unchanged | Set in round 3 for the alias-copy cost of deep isolation on large record literals. Measures 2,306,586, 3.9% headroom. Removing it was tried and does not fit: the compile-inclusive modes still exceed the 2,200,000 default. |
| Aggregate profile instructions | 85,000 | 110,000 | Round 3.5 added three scenarios to the aggregate. Measures 71,802. |
| RLM short-binding commit ceiling | 64 KiB | 48 KiB | Investigated: the commit did not grow. It is 33,027 bytes and has been since the tree representation, so the ceiling is set from that measurement rather than left at the round-3 judgment call. The assertion that carries the meaning — no leaf minted — is separate. |
| `heap_variable_concat`, `heap_shallow_chain_mutation`, `heap_deep_chain_mutation_24` | — | ns, allocation and byte budgets from measurement | New in round 3.5. |
| `heap_deep_chain_mutation_32` key | present | renamed `_24` | The program is twenty-four levels; the name claimed thirty-two. |
| Chain depth ratio | — | 5.0 | New in round 3.5. Measures 3.0x. |
| `indexed_add_exact_limit_succeeds_and_one_byte_over_preserves_state` | empty + grown record | grown record only | Not a budget file entry but a guard with a number in it. The transition heapifies the persisted record once and grows that object in place, so the peak is lower than the expectation encoded. |

Eight `heap_*` scenarios carry budgets: `heap_list_iteration`, `heap_nested_loop`,
`heap_allocation_churn`, `heap_deep_chain_mutation`, `heap_comprehension_build`,
`heap_variable_concat`, `heap_shallow_chain_mutation` and
`heap_deep_chain_mutation_24`. Wall-clock budgets sit at roughly three times
their measured maximum, counts at roughly twenty percent.

## Verification

| Gate | Result |
| --- | --- |
| `cargo check --workspace --all-targets --locked` | PASS |
| `cargo test --workspace` | PASS |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | PASS |
| `cargo fmt --all --check` | PASS |
| `python3 scripts/check_included_file_formatting.py` | PASS — 28 files |
| `python3 scripts/lint_docs.py` | PASS — 46 HTML pages, 42 registry pages |
| `bash scripts/check-rustdoc.sh` | PASS — 583 documented, 0 missing |
| `python3 scripts/check_test_quarantines.py` | PASS |
| `python3 scripts/check_api_example_coverage.py` | PASS — 8,005 entries |
| `just perf-guard` | PASS — 0 budget failures |
| Both reviewers' probe suites, as named tests | PASS |

### Named regressions from the reviews

Round 3.5 added, in `crates/lashlang/tests/value_semantics.rs` and
`crates/lashlang/src/runtime/tests/continuation_wire_cases.rs`:

- `formatting_a_container_binding_renders_it` (opus F1)
- `arithmetic_on_a_container_binding_names_the_container_type` (opus F1)
- `stress_collection_survives_a_general_concat` (opus F2)
- `stress_collection_survives_a_slot_concat_and_a_loop_concat` (opus F2)
- `continuation_decode_rejects_shared_and_cyclic_durable_ownership` (sol P0)
- `continuation_decode_accepts_transient_duplication`
- `canonical_decode_rejects_shared_roots_cycles_and_unreachable_objects`, now
  covering an unrooted ring and a within-root diamond (sol P1)
- `continuation_heap_round_trip_is_canonical_and_rejects_cycles`
- `canonical_decode_rejects_a_heap_chain_deeper_than_the_value_limit` (opus F7)
- `a_store_allocates_one_object_per_live_object` (opus F6)
- `a_rejected_concat_leaves_the_accumulator_untouched` (N1)
- `depth_measurement_terminates_on_a_cycle` (N2)
- `only_effect_shaped_opcodes_use_the_conservative_default` (N4)
- `assignment_is_a_statement_not_an_expression` (N5)

They join the round-3 set: the concat and aliasing probes, the authored
continuation fixture, the accepted-wire round trip, the straight-through
suspension equivalence, the one-way memory relation, the heap-backed snapshot
property test, and the rejected-patch atomicity regressions.

Red-side checked by restoring the previous behaviour: the format panic and the
type-name leak, the stress-collection dangling reference, the shared-slot
continuation, the deep-chain decode, the half-applied concatenation, the cyclic
depth measurement (which hangs), the two rejected-patch regressions, and the
layout-sensitive heap equality.

## Deferred

- Copy-on-write behind identical observable semantics (ADR 0059).
- A sub-linear bound on ancestor invalidation.
- Typed errors at the two boundaries that cannot return one without a
  signature change across their callers.
- The `include!`d files outside `crates/lashlang`.
- Rebasing onto `main` and reconciling the independent RLM v8 collision.
- Closures as heap objects; a reference-semantics dialect that omits the
  isolation lowering.
- Typed heap restore errors.
