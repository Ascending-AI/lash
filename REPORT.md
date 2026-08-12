# FIG-1301 fix-round report

## Result

Both adversarial reviews are fully addressed on `samuel-fig-1301`, without rebasing. Lashlang's heap representation now preserves the pre-heap language's value semantics, persists one representation per value, validates restored heaps through one fail-closed path, and keeps logical metering deterministic across execution, suspension, and restore.

The cutover remains whole: there is no compatibility decoder, migration shim, or fallback to a superseded wire version.

## Review finding disposition

| Finding | Disposition |
| --- | --- |
| Opus F1 / Sol Critical: container insertion and iterator aliases | Fixed at the Lashlang lowering seam. `DeepCopy` is emitted for name/effect/path stores, iterator bindings, comprehension append, generic and specialized `push` items, and tuple/list/record literal members. `DeepCopy` isolates the entering root; the inductive insertion rule keeps descendants isolated. Heap/VM primitives remain reference-preserving for the future TypeScript lowering. Ancestor-aware materialization-cache invalidation removes the prior two-bugs-cancelling behavior. |
| Opus F2 / Sol Medium: duplicate persisted tree and heap | Fixed. A snapshot uses the plain tree form when there is no runtime heap and the heap-root form otherwise; it never writes both. A scalar canonical snapshot is 48 bytes. The short-binding RLM root is 33,027 bytes and uses a measured 34 KiB ceiling instead of 96 KiB. |
| Sol Critical: public globals could diverge from private roots | Fixed. `State::{set_default,insert_global,remove_global}` patch the visible tree and runtime heap roots together, re-meter, and collect. RLM defaulting, projected rehydration, and protected-binding pruning use those APIs. Snapshot fields are private, preventing the stale `Snapshot::globals` mutation path. |
| Opus F5 / Sol Critical: indexed add mutated before charging and recomputed O(n) size | Fixed. The operation calculates its exact member delta, checks the prospective bound, then commits the value and counters together. Existing-field and new-field updates are O(1) amortized. |
| Sol High: fallible heapification could leave `null` values | Fixed. Heapification stages all required inline compounds and their accounting, validates the complete batch, then commits and replaces VM values. A failed import leaves stack, slots, iterators, heap, and persisted state structurally intact. The implementation avoids the rejected per-instruction whole-heap clone. |
| Opus F3 / Sol High: malformed snapshot and continuation heaps were accepted | Fixed. Snapshot and continuation decode share `Heap::from_wire`, which enforces strictly increasing nonzero IDs, exact `next_id == allocation_counter + 1`, schedule version, recomputed byte accounting, resolvable root and nested references, and canonical root ordering. Snapshot restore additionally rejects shared roots, cycles, and unreachable objects. |
| Sol High: continuation NaN rejection | Fixed. Continuations use a versioned numeric-bits wire shared in policy with snapshots: NaNs normalize to one quiet-NaN representation and negative zero retains its sign bit. Durable segment and cross-process tests cover both. |
| Opus F4: suspension collected a clone | Fixed. `suspend(&mut self)` collects the live heap at the stop-the-world boundary before encoding the continuation, so park/resume and straight-through execution retain the same heap accounting and hit limits at the same point. |
| Opus F6: allocation-ID bookkeeping grew forever | Fixed. `id_to_slot` is a sparse `BTreeMap` keyed only by live IDs; materialized entries are a sparse `FxHashMap`. Sweeping removes both, while recycled slots receive fresh monotonic IDs. Reverse parent edges support targeted cache invalidation and are removed with their objects. |
| Opus F7: `memory_limit` silently defaulted | Fixed. `memory_limit` is required in RLM configuration and every in-repository constructor/config/example states it explicitly. Missing input fails typed construction. ADR-0055 now names instruction, deadline, and logical-memory bounds. |
| Opus F8 / Sol Medium: test rigor gaps | Fixed. `Snapshot::PartialEq` covers public globals, runtime roots, heap objects, and meters. The two-process probe executes beyond the 1,024-allocation collection threshold, retains objects across collection, creates vacant/reused slots, and compares both snapshot and continuation bytes. Sweep fixed points and every review alias repro are explicit tests. |
| Opus F9: report overclaims | Fixed by this report. It describes the actual shallow insertion-isolation mechanism, the shared decoder invariants, the single-source persisted size, and current performance measurements. |
| Opus F10: boundary cache invariant implicit | Fixed. Cache removal is centralized in `forget`, the cache relationship has a debug assertion, and descendant mutation invalidates every materialized ancestor through reverse edges. |

## Design after the fixes

### Value semantics and lowering

`Value::Ref(HeapId)` remains an internal VM representation for tuple, list, and record objects. Heap operations do not implicitly copy references. The Lashlang compiler owns the value-semantics decision by emitting `DeepCopy` immediately before each value crosses an assignment or container-insertion boundary.

Isolation is root-level rather than recursively cloning an already-isolated graph. Every container construction/insertion isolates each entering member, so this rule is inductive: later in-place mutation of one root cannot mutate a root held by another binding or container. A dialect with reference semantics can omit this lowering without changing heap primitives.

### Heap, collection, and metering

- Heap IDs start at 1 and advance monotonically. Storage slots may be reused after sweep; IDs never are.
- Mark-sweep traces stack, last value, slots/extras, iterator cursor and restoration state, and persisted runtime roots. Normal collection occurs every 1,024 allocations under size-schedule version 1.
- Logical size charges a fixed object header, value slots, record fields and keys, and scalar payloads. Allocation and incremental mutation charge before committing. Sweep subtracts the stored per-object charge.
- Boundary materialization is non-semantic and non-persisted. Sparse caches and reverse parent edges allow a child mutation to invalidate only values that can reach it.
- The default logical heap limit remains 64 MiB, but hosts must choose and serialize a bound explicitly.

### Persistence

| Contract | Version | Policy |
| --- | ---: | --- |
| Lashlang canonical snapshot | 2 | Named MessagePack; either plain globals or runtime roots plus ID-ordered heap, never both |
| RLM snapshot root | 8 | Typed fail-closed root on this unre-based branch |
| Lashlang bytecode | 3 | Includes explicit insertion-isolation instructions |
| Lashlang segment state | 2 | Carries the canonical continuation and cumulative meters |
| Heap logical-size schedule | 1 | Required and validated on restore |

Accepted snapshot bytes are a fixed point under decode and re-encode. Heap objects are emitted in ascending ID order. Floating-point values use canonical bits: normalized quiet NaN and sign-preserving negative zero.

## New and strengthened tests

### Exact review repros

- `path_assignment_rhs_matches_pre_heap_value_semantics`
- `iterator_binding_matches_pre_heap_value_semantics`
- `push_insertion_matches_pre_heap_value_semantics`
- `iterator_value_pushed_into_container_matches_pre_heap_semantics`
- `nested_field_index_and_comprehension_inserts_keep_value_semantics`
- `effect_result_to_path_isolated_from_later_field_reads`
- `every_container_insertion_lowering_emits_value_isolation`

These assert the literal pre-heap results from both reviews, including nested path/index reads, loop bindings, comprehension accumulation, generic push, and optimized push.

### State, accounting, and persistence

- `plain_scalar_snapshot_has_no_heap_duplicate`
- `heap_aware_global_patches_survive_next_cell_and_cold_restore`
- `heap_backed_default_patch_survives_next_cell_and_cold_restore`
- `heap_backed_projection_rehydrate_and_prune_survive_execution_and_restore`
- `indexed_add_charges_before_record_growth_and_updates_incrementally`
- `indexed_add_exact_limit_succeeds_and_one_byte_over_preserves_state`
- `failed_heapification_preserves_compound_state_transactionally`
- `canonical_decode_rejects_descending_heap_ids`
- `canonical_decode_rejects_dangling_root_and_nested_references`
- `canonical_decode_rejects_counter_accounting_schedule_and_root_order`
- `canonical_decode_rejects_shared_roots_cycles_and_unreachable_objects`
- `continuation_decode_rejects_descending_counters_and_dangling_refs`
- `continuation_numbers_canonicalize_nan_and_preserve_negative_zero`
- `durable_segment_round_trip_preserves_nan_and_negative_zero`
- `sparse_object_bookkeeping_stays_bounded_by_live_objects`
- `child_mutation_invalidates_materialized_ancestor_cache`
- `suspend_collects_live_heap_before_park_or_keep_running_diverge`
- `independent_os_processes_emit_byte_identical_snapshot_and_continuation_dumps` now crosses collection, vacancy, and reuse and compares both encodings.

## Verification

| Gate | Result |
| --- | --- |
| Review repro tests named above | PASS |
| `cargo check --workspace --all-targets --locked` | PASS |
| `cargo test --workspace` | PASS |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | PASS |
| `cargo fmt --all --check` | PASS |
| `python3 scripts/lint_docs.py` | PASS: 46 HTML pages, 42 registry pages |
| `bash scripts/check-rustdoc.sh` | PASS |
| `python3 scripts/check_test_quarantines.py` | PASS |
| `python3 scripts/check_api_example_coverage.py` | PASS: 8,005 entries |
| `just perf-guard` | PASS: complete quick runtime profile and 210 Lashlang results |

## Performance

The final `just perf-guard` run passes every runtime and Lashlang budget. The earlier transactional implementation cloned the complete heap after every instruction; the final staged importer commits atomically without that clone and imports only inline compound values.

The heap-sensitive `large_data` scenario (500 iterations) measured:

| Mode | Time/iteration | Allocations/iteration | Bytes/iteration |
| --- | ---: | ---: | ---: |
| one shot | 6.931 ms | 11,763.644 | 2,329,301.2 |
| prewarmed one shot | 9.087 ms | 11,763.514 | 2,329,267.6 |
| compiled execute | 6.048 ms | 9,110.124 | 2,039,797.8 |
| snapshot | 10.451 ms | 9,697.124 | 2,055,027.8 |
| phase breakdown | 7.202 ms | 11,910.750 | 2,340,442.7 |

The existing `large_data` byte override is still required: its measured maximum is 2,340,442.7 bytes/iteration, 6.38% above the 2,200,000 default. It is calibrated to 2,400,000 bytes (2.54% measured headroom). The allocation ceiling remains the unchanged 12,000 default, and every other scenario uses the default budgets.

The final quick runtime profile also measured:

| Scenario | Total median | Total allocated | Lashlang execute median | Lashlang execute allocated |
| --- | ---: | ---: | ---: | ---: |
| `rlm_globals` | 43.768 ms | 44,764,326 B | 3.507 ms | 4,549,135 B |
| `rlm_large_print` | 52.036 ms | 92,914,466 B | 6.819 ms | 23,592,059 B |
| `rlm_oblique_stack_mix` | 215.115 ms | 277,174,170 B | 140.708 ms | 158,076,833.5 B |

No measured hot path is a greater-than-2x cliff, and no unrelated performance ceiling changed.

## Deferred work

- Rebase and reconcile the independent origin/main RLM v8 collision only in the orchestrator's follow-up round; this branch intentionally remains on its original base.
- Closures become heap objects in their later campaign layer.
- The TypeScript dialect can omit Lashlang's insertion-copy lowering to expose reference semantics.
- General uniqueness analysis, incremental/moving GC, weak references, finalizers, and allocator/RSS metering remain out of scope.
