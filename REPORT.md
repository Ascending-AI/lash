# FIG-1301 round 3 report

## What changed

The heap object graph is now a forest of exclusively owned trees. A heap
reference may be duplicated only in transient VM operand flow; every durable
store — name and slot stores, global stores, container members, iterator
bindings, effect-result bindings, `State` patch APIs — holds a recursive copy
under fresh IDs. [ADR 0059](docs/adr/0059-lashlang-durable-stores-hold-exclusively-owned-copies.md)
records the decision and its cost.

The two blocking defects both re-reviews found are gone as a class rather than
case by case: an emitted snapshot always decodes because sharing between roots
is unrepresentable, and a reference can no longer hide inside an inline compound
member because the decoder refuses those members outright.

Per-instruction heapification no longer rescans iterator cursors, which is what
made iterating a heap-backed list quadratic. A single pass over a 2,000-element
list is 12.6x faster than the fix round; the existing `large_data` scenario is
4.4x faster.

## Disposition of the re-review findings

| Finding | Disposition |
| --- | --- |
| Sol P0-1 / opus N1: shallow isolation violates value semantics; emitted snapshots do not decode | Fixed. `isolate_value` reallocates the whole reachable graph under fresh IDs and deliberately ignores the boundary materialization cache, which is what re-introduced sharing on an export/import round trip. The optimized single-item concat, the general concat and the fused slot concat isolate their operands; container literals, comprehension appends, push, path assignment, iterator bindings and effect results already did. Both reviewers' probes are named tests. |
| Sol P0-2: an accepted continuation loses a live nested reference | Fixed. `Heap::from_wire` rejects a heap object member that is an inline compound, so the shape that hid a reference from tracing is not in the accepted language. One recursive enumerator now answers child discovery for allocation bookkeeping, reverse edges, mark, sweep, wire validation and root traversal. The malformed continuation is a named rejection test, and an accepted wire is driven through resume, park, re-encode and re-decode. |
| Sol P1 / opus N5: batched global patches can partially commit | Fixed. `State::patch_globals` stages a whole batch against copies of the globals, the runtime roots and the heap, and publishes all of it or none; one heap clone and one collect per batch. RLM `set_default` validates every key before applying any, and records its dirty marks from what the commit reports. Projection rehydrate and protected-binding prune route through the same batch. Two rejection regressions assert byte-identical state; both fail against the previous per-key loop. |
| Sol P1: cache invalidation can be quadratic in ancestor depth | Mitigated, not eliminated. Invalidation returns immediately when nothing is materialized, which is the common case because `export_for_mutation` drops the whole reachable cache before mutating. When materialized values do exist the walk still visits reachable ancestors. The deep-chain mutation shape is now a perf scenario with a budget; it measures 1.06x faster than the fix round, so the shape is guarded even though the traversal bound is unchanged. |
| Sol P2: the suspend equivalence test suspended both sides | Fixed. The control VM runs from the same starting point straight to completion and never calls `suspend`. Result, instruction meter and reachable heap accounting are compared against park and resume. |
| Sol P2 / ADR-0044: the canonical byte test derived its oracle from the serializer | Fixed. An authored continuation wire, written from the wire schema, is decoded and re-encoded exactly. Authoring it caught two things the round-trip tests could not: the member-shape difference between a heap object and a value, and the exact logical-byte arithmetic. |
| Opus N2: per-instruction heapification makes list iteration quadratic | Fixed. Iterator cursors and the extra-globals record are written once and read after that, so each carries a flag and is heapified once instead of rescanned after every instruction. Numbers below. |
| Opus N3: snapshot equality compares slot layout | Fixed. Two heaps are equal when they hold the same live objects under the same IDs with the same meters; storage layout is a private allocation detail a round trip legitimately compacts. Capturing a snapshot collects it, so a snapshot holds exactly what its roots reach. `decode(encode(state)) == state` is a passing property test over generated heap-shaping programs, and it fails against the previous equality. |
| Opus N4: `ExecutionBounds::new` defaulted `memory_limit` | Fixed. It takes all three bounds; every in-repository caller states the memory limit. |
| Opus N6: member overwrite left a stale parent edge | Fixed. One helper retargets a parent's outgoing edges, used by both member overwrite and object replacement, so the reverse-edge map is exact rather than an over-approximation waiting for a sweep. |
| Opus N7: `boundary_refs` was a linear scan | Fixed. The boundary cache is indexed both ways, so lookup and forget are constant time. |
| Opus N8: two import paths differed in transactionality | Fixed by removing the second one. The recursive per-node import is gone; `import_values` and `isolate_value` both stage and charge a whole batch before committing any of it. |
| Opus N9: the perf guard has no wall-clock budget and no iteration scenario | Fixed. Four heap-shaped scenarios with wall-clock budgets, described under Performance. |
| Opus judgment: the 34 KiB inline-root assert is too tight | Changed to the property plus a generous ceiling. The test asserts that no leaf is minted — the thing that would actually regress — with a 64 KiB sanity bound above a ~33 KiB measurement, and says why. |
| Sol note: heap restore errors stay stringly typed | Accepted for this round, unchanged. |

## What this round does not claim

- **The memory limit is sensitive to collection timing.** It bounds live plus
  not-yet-collected bytes, so a run that parks — which collects — can survive a
  point at which the same run without a park would have exhausted the bound. The
  relation is one-way: parking never brings exhaustion forward. The equivalence
  test asserts that one-way relation rather than an equal failure instruction,
  and ADR 0055 records it.
- **Ancestor invalidation is not proven sub-linear.** See the Sol P1 row.
- **Wall-clock budgets are not drift detectors.** They carry roughly three times
  the measured maximum because the same binary on this machine varied by up to
  75% between runs of the same scenario. They catch the order-of-magnitude class
  that hid here.
- **Isolation costs a copy.** Copying an alias-heavy graph is proportional to
  the graph. `type_system_stress`, which builds large record literals, is about
  20-25% slower than the fix round for this reason. Copy-on-write behind
  identical observable semantics is recorded as future work in ADR 0059, not
  attempted here.
- **`State::patch_globals` is atomic within a state, not across an RLM turn.**
  Callers still sequence their own persistence.

## Design

### Isolation

`isolate_value` is the one isolation operation. It reserves fresh IDs for the
whole graph reachable from the stored value, builds the copies, charges the
batch, and only then commits — so a copy that would cross the memory bound
leaves the heap byte-identical. Cycles terminate because an ID is reserved
before its object is built.

It does not consult the boundary materialization cache. That cache exists so
that exporting a heap object to a tree and importing it back is
identity-preserving, which is exactly the sharing an isolation must not
reintroduce; the fix round's shallow isolation went through it, which is why
`pair = (child,)` shared `child`'s object.

The compiler decides where isolation happens. Container literals and
comprehensions skip the store-level copy because they already isolated every
member they admitted and built a fresh container around those copies; every
other right-hand side is copied on store.

Two independent checks keep the invariant honest: the decoder refuses roots that
share an object, and the encoder asserts the same property in debug builds, so a
violation fails at the write rather than at a later cold restore in another
process.

### Iteration

An iterator cursor's values are written once, when the iterator is created or
restored, and stepping only advances an index. Each cursor carries a flag and is
heapified once. The extra-globals record is the same shape. What remains scanned
after every instruction is the operand stack and the slot table, both bounded by
the program's shape rather than by its data.

`xs = xs + [item]` appends into the accumulator's own object instead of
exporting the accumulator to a tree, appending, and importing the whole thing
back — which was O(n) per append. In-place append is safe precisely because
every other holder of the old value already owns a separate copy.

### Persistence

| Contract | Version | Policy |
| --- | ---: | --- |
| Lashlang canonical snapshot | 2 | Named MessagePack; either plain globals or runtime roots plus ID-ordered heap, never both |
| RLM snapshot root | 8 | Typed fail-closed root on this un-rebased branch |
| Lashlang bytecode | 3 | Includes explicit insertion-isolation instructions |
| Lashlang segment state | 2 | Carries the canonical continuation and cumulative meters |
| Heap logical-size schedule | 1 | Required and validated on restore |

A heap object member is a scalar or a reference. Accepted bytes are a fixed
point under decode and re-encode. Objects are emitted in ascending ID order.
NaN normalizes to one quiet representation and negative zero keeps its sign.

## Performance

Release build, `compiled_execute` mode, 200 iterations, one machine. "Fix round"
is `0f746e09a` built in a separate worktree with the same scenario definitions
copied in; both trees were measured back to back.

| Scenario | Fix round | This round | Change |
| --- | ---: | ---: | ---: |
| `heap_list_iteration` (2,000-element single pass) | 41.20 ms | 3.27 ms | 12.6x faster |
| `heap_nested_loop` (inner pass over a growing list) | 5.97 ms | 3.30 ms | 1.8x faster |
| `heap_allocation_churn` | 2.81 ms | 2.11 ms | 1.3x faster |
| `heap_deep_chain_mutation` | 2.30 ms | 2.17 ms | 1.06x faster |
| `large_data` | 5.28 ms | 1.19 ms | 4.4x faster |
| `loop_control` | 0.267 ms | 0.129 ms | 2.1x faster |
| `type_system_stress` | 0.850 ms | 0.962 ms | 1.13x slower |

At the size opus measured, a single pass over an 8,000-element list is about
1-2 ms, obtained by differencing a build-and-pass program (7.6-12.4 ms) against
a build-only one (7.1-8.0 ms); the pass is small next to the build and the
run-to-run spread is wide. The fix round measured 486 ms for that pass and the
pre-heap tree runtime measured 1.25 ms, so this is within the requested ~2x of
pre-heap behaviour, with the caveat that the difference sits close to the noise
floor of these measurements.

The scenarios committed to the guard are sized to keep the suite fast — 2,000
elements for the list pass, 60 outer iterations for the nested loop, 150 for the
deep chain. The 8,000-element figures came from temporarily raising the constant
in both trees and are not a committed measurement.

Scenarios other than those listed moved by less than 15% in either direction,
which is inside the run-to-run spread on this machine. `type_system_stress` is
the one repeatable regression and is the alias-copy cost of deep isolation: it
builds large record literals, and each literal member is now copied recursively.

`large_data` measures 2,303,071 bytes per iteration, down from the 2,340,443 the
fix round reported. Its byte override stays at 2,400,000 — 4.2% headroom on a
metric whose run-to-run spread is under 0.01% — and its allocation ceiling stays
at the unchanged 12,000 default. No other existing scenario's budget changed.

## Verification

| Gate | Result |
| --- | --- |
| `cargo check --workspace --all-targets --locked` | PASS |
| `cargo test --workspace` | PENDING |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | PENDING |
| `cargo fmt --all --check` | PENDING |
| `python3 scripts/lint_docs.py` | PENDING |
| `bash scripts/check-rustdoc.sh` | PENDING |
| `python3 scripts/check_test_quarantines.py` | PENDING |
| `python3 scripts/check_api_example_coverage.py` | PENDING |
| `just perf-guard` | PENDING |

### Probe tests

Both reviewers' programs, as named tests in
`crates/lashlang/tests/value_semantics.rs` and
`crates/lashlang/src/runtime/tests/continuation_wire_cases.rs`:

- `optimized_concat_insertion_copies_the_appended_binding` (sol probe 1, across
  three cells and a snapshot boundary)
- `optimized_concat_insertion_copies_within_one_cell`
- `general_concat_copies_the_right_operand_members`
- `slot_concat_copies_the_right_operand_members`
- `aliased_root_with_a_nested_container_round_trips` (sol probe 2)
- `self_insertion_stores_a_copy` (sol probe 3)
- `accumulated_rows_aliased_to_a_second_root_round_trip` (opus N1)
- `aliased_accumulator_does_not_observe_later_appends`
- `descendant_read_into_a_new_binding_is_isolated`
- `multi_root_program_state_always_decodes`
- `snapshot_equality_survives_a_round_trip_after_temporaries`
- `continuation_decode_rejects_inline_compound_heap_members` (sol P0-2)
- `authored_continuation_fixture_decodes_and_re_encodes_exactly`
- `accepted_continuation_wire_survives_resume_suspend_and_re_encode`
- `park_and_resume_is_invisible_to_a_straight_through_run`
- `park_and_resume_never_exhausts_memory_earlier_than_a_straight_through_run`
- `heap_backed_state_round_trips_as_an_equal_snapshot` (property test)
- `rejected_global_patch_leaves_byte_identical_state_and_no_dirty_marks`
- `rejected_protected_name_patch_leaves_byte_identical_state`

Three were checked red-side by temporarily restoring the previous behaviour: the
two rejected-patch regressions fail against the per-key loop, and the round-trip
property test fails against the layout-sensitive heap equality.

## Deferred

- Copy-on-write behind identical observable semantics (ADR 0059).
- A sub-linear bound on ancestor invalidation.
- Rebasing onto `main` and reconciling the independent RLM v8 collision; this
  branch stays on its original base.
- Closures as heap objects; a reference-semantics dialect that omits the
  isolation lowering.
- Typed heap restore errors.
