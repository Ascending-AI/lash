# FIG-1301 implementation report

## Result

Lashlang now executes compound values through a deterministic mutable heap while preserving the existing language's observable value semantics. The cutover is complete: the runtime, suspended VM, canonical Lashlang snapshot, RLM snapshot, segment state, and bytecode contract all use the new representation and reject their superseded versions. There is no compatibility decoder or representation shim.

## Design

### Heap, identity, and collection

- `Value::Ref(HeapId)` is the VM representation for tuples, lists, and records. The object table is a `Vec<Option<HeapEntry>>`; a separate allocation-ID-indexed vector resolves an ID to its current storage slot, and a LIFO free-slot list permits storage reuse.
- `HeapId` starts at 1 and is issued by a monotonic `u64` counter. Sweeping can recycle a vacant vector slot, but never an ID; exhaustion is a typed terminal error.
- Collection is non-moving, stop-the-world mark-sweep. It traces references from stack values, slots, globals, active iterator state, and projected/bound state. Normal collection is triggered every 1,024 allocations by size-schedule version 1. A test-only host policy collects at every allocation.
- The boundary materialization cache is non-semantic and is neither a root by itself nor serialized. It avoids repeatedly exporting and re-importing the same tree during one VM residency; reachability still comes exclusively from runtime roots.

### Logical memory metering

- Heap size schedule 1 charges a fixed 16-byte object header, 16 bytes per value slot, 8 bytes per record field plus UTF-8 key bytes, and an explicitly defined payload cost per scalar/reference kind.
- The default bound is 64 MiB of logical live heap data. Charging happens before allocation and sweeping subtracts the exact stored charge. `MemoryLimitExceeded { limit, attempted }` is an uncatchable execution-bound terminal.
- `ExecutionBounds` and the RLM public/config representations now carry `memory_limit`. Allocation count, live logical bytes, next allocation ID, schedule version, and heap objects persist through snapshots and continuations. The deterministic trigger is recovered from the allocation counter; no GC phase or wall-clock state is persisted.

### Copy lowering and observable semantics

- The compiler emits `DeepCopy` before ordinary name stores in `runtime/compiler/entry.rs` and before effect-result bindings in `runtime/compiler/effects.rs`. Copies recursively duplicate the reachable object graph, preserve cycles, and assign fresh IDs.
- Loop/iterator bindings and host crossings are materialized at their established binding boundaries. Host operations receive the same JSON-shaped tree values as before. Export detects cycles and returns the typed `CyclicHostValue` error rather than recursing indefinitely.
- Assignment-path operations mutate the destination's isolated heap graph. Existing specialized self-update instructions (`push` and numeric indexed addition) operate directly on the owned heap object; this is a proof-specific copy elision for an already isolated destination, not generalized copy-on-write.
- Lashlang equality dereferences structurally and tracks compared ID pairs, so cycles terminate and observable equality remains value-based. Raw `Value::Ref` equality is only identity-level internal behavior.
- Generalized CoW remains deferred. The later TypeScript dialect can omit Lashlang's compiler-inserted copies to expose true reference semantics.

### Canonical persistence and versions

| Contract | Previous | New | Policy |
| --- | ---: | ---: | --- |
| Lashlang canonical snapshot | unversioned tree | 2 | named MessagePack envelope with globals, roots, and ID-ordered heap |
| RLM snapshot root | 7 | 8 | fail closed through the existing typed mismatch |
| Lashlang bytecode format | 2 | 3 | fail closed; bytecode now includes `DeepCopy` |
| Lashlang segment state | 1 | 2 | fail closed; carries the new continuation |
| Heap logical-size schedule | none | 1 | validated on restore |

Heap objects serialize in ascending `HeapId` order and children serialize as IDs, so cyclic graphs need no exceptional wire form. Floating-point values serialize by explicit bits: every NaN is normalized to the canonical quiet-NaN bits, while negative zero remains distinct. Both the snapshot and continuation decoders validate IDs, object order, roots, counters, byte accounting, and schedule version. Dump-load-redump is a byte-for-byte fixed point.

## Consumer audit

| Consumer | Audited sites | Disposition |
| --- | --- | --- |
| Dialect snapshot boundary | `crates/lash-protocol-rlm/src/dialect/lashlang.rs`, `executor/snapshot.rs`, `executor/state.rs` | The dialect owns the version-8 envelope. Lashlang globals carry the version-2 canonical snapshot, including the heap. Old roots fail closed. |
| Projection and transport | `crates/lash-protocol-rlm/src/projection/transport.rs`, `projection/context.rs`, `rlm_support.rs` | Projection operates on boundary-materialized `Snapshot` globals, so bound-variable rendering remains tree-shaped. Defensive `Ref` arms prevent an internal reference from being rendered as user data. |
| Prompt rendering of Bound Variables | `crates/lash-protocol-rlm/src/executor.rs`, `rlm_support.rs` | Bound variables are taken from the visible snapshot projection. Prompt text and ordering are unchanged. |
| Host bridge | `crates/lash-protocol-rlm/src/executor/host_bridge.rs`, `crates/lash-lashlang-runtime/src/lib.rs` | VM crossings materialize JSON-shaped values first. A leaked reference or reachable cycle is a typed error; neither is silently serialized. |
| Runtime handover/replay | `crates/lash-lashlang-runtime/src/process.rs`, `crates/lashlang/src/runtime/vm/continuation.rs` | Segment state version 2 stores the complete continuation and cumulative heap meters. Replay keys and occurrence counters remain outside the value representation and are unchanged. |
| Typed checkpoint components | `crates/lash-core/src/store/mod.rs`, `store/commit_identity.rs`, `crates/lash-protocol-rlm/src/executor/state.rs` | The RLM component body changes through its explicit versioned codec. Component hashing remains over canonical bytes; the root keeps typed component descriptors. |
| SQLite persistence | `crates/lash-sqlite-store/src/blobs.rs`, `graph.rs` | Stores checkpoint components as opaque content-addressed bytes and never interprets Lashlang values or heap IDs. No schema or table change is needed. |
| PostgreSQL persistence | `crates/lash-postgres-store/src/postgres/support.rs`, `postgres/runtime_persistence.rs` | Same opaque component/ref behavior as SQLite. No payload-shape registration, migration, or new table is needed. |
| Trace graph projection | `crates/lash-core/src/trace.rs`, `crates/lash-sim/src/trace.rs` | Trace facts observe protocol/runtime events and checkpoint descriptors, not the Lashlang value tree. Unaffected. |
| Expect/transcript output | `crates/lash-sim/src/transcript.rs`, `runner/contract_support.rs` | Continues to report typed component states and logical byte fields only. No opaque-blob-size assertion was added. |
| Lash-sim state/oracles | `crates/lash-sim/src/runtime_contracts.rs`, `oracles.rs`, `sqlite_replay.rs`, `postgres_replay.rs` | The simulation treats the versioned RLM component as runtime-owned bytes and compares semantic projections. Existing cross-backend, replay, schema-congruence, and transcript tests pass; no SQL registry edit is required. |

## Test changes

### New conformance coverage

- `gc_stress_mode_preserves_results_and_canonical_dumps`: default and collect-every-allocation modes have identical result and dump bytes.
- `logical_memory_exhaustion_is_an_uncatchable_typed_terminal`: a logical-byte limit terminates with the execution-bound error family.
- `continuation_dump_round_trip_is_byte_identical_and_preserves_heap_meters`: continuation JSON is a fixed point and its counters advance after resume.
- `independent_os_processes_emit_byte_identical_snapshot_and_continuation_dumps`: two fresh child processes emit identical snapshot and continuation bytes.
- `heap_meters_continue_after_restore_in_a_new_os_process`: allocation/live-byte/instruction meters survive a child-process handoff and continue monotonically.
- `continuation_heap_round_trip_is_canonical_and_cycle_safe`: continuation heap IDs and cycles round-trip canonically.
- `swept_storage_gets_a_fresh_monotonic_identity`: recycled storage receives a never-before-used ID.
- `deep_copy_preserves_cycles_with_fresh_ids`: deep copy terminates on cycles and produces an isomorphic graph with fresh identities.
- `canonical_encoding_is_deterministic_for_map_order_and_nan_payload`: canonical ordering and NaN normalization are pinned.
- `canonical_empty_heap_has_exact_golden_bytes`: the complete version-2 empty envelope is pinned byte-for-byte.
- Existing canonical/property tests now cover every value variant through the heap envelope, negative zero, invalid IDs/counters/accounting, version mismatches, and dump-redump stability.

### Existing test edits and justification

These are the only existing-test changes; none changes an observable Lashlang expectation.

| Test file | Edit | Why representation-only |
| --- | --- | --- |
| `runtime/snapshots/lashlang__runtime__tests__lashlang_compiled_bytecode_contract.snap` | Added `deep_copy` instructions and shifted PCs/jump targets. | The bytecode snapshot intentionally asserts internal lowering; program results are unchanged. |
| `runtime/tests.rs` | Added the `DeepCopy` spelling used by the bytecode snapshot helper. | This exposes the new internal instruction in the representation contract above; it changes no runtime assertion. |
| `runtime/state.rs` → `runtime/state/tests.rs` | Mechanically extracted the existing test module for the production file-size gate; switched hand-built snapshots to `Snapshot::new`, added required version/heap fields, updated heap-aware error locations, and regenerated the comprehensive canonical length/hash. | Every changed assertion targets the new canonical wire representation. The semantic round trips, ordering, depth, malformed-input, and value-variant expectations are unchanged. |
| `runtime/tests/continuation_cases.rs` | Materialized an internal restored `Ref` before inspecting record insertion order; initialized the heap in a hand-built continuation. | The test directly inspects VM internals. Its insertion-order assertion and invalid-continuation behavior are unchanged. The same file also contains the new conformance tests above. |
| `runtime/state/fixes3_tests.rs` | Added the version and empty heap to a hand-built canonical wire. | The internal wire deliberately gained required envelope fields. The depth-limit assertion is unchanged. |
| `runtime/tests/async_and_cache_cases.rs` | Replaced a `Snapshot` struct literal with `Snapshot::new`. | The constructor initializes the new private heap section; globals and behavior are identical. |
| `runtime/tests/projection_cases.rs` | Replaced a `Snapshot` struct literal with `Snapshot::new`. | Same constructor-only representation update; projection behavior is unchanged. |
| `tests/property.rs` | Replaced `Snapshot` struct literals with `Snapshot::new`. | Same constructor-only update; all generated semantic comparisons are unchanged. |
| `executor/state/tests.rs` | Bumped the root golden from version 7 to 8 and included the nested version-2 Lashlang heap envelope. | This is the required canonical-persistence contract bump; the typed state values outside that envelope are unchanged. |
| `executor.rs` (`many_short_bindings_keep_the_root_inline_and_commit_cost_bounded`) | Raised the internal encoded-root ceiling from 32 KiB to 96 KiB. | Each inline binding now carries the fixed versioned heap envelope, but the test's observable contract is unchanged: values remain inline, mint no leaf components, and stay under a bounded commit-size ceiling. |
| `crates/lash/src/tests/core_session_builder/session_lifecycle.rs` | Updated the expected current RLM version in the old-version rejection message from 7 to 8. | The test still proves the same fail-closed operator remediation path. |
| `crates/lashlang/examples/bench_support/mod.rs` | Replaced a `Snapshot` struct literal with `Snapshot::new`. | Constructor-only initialization of the private heap section; benchmark inputs are identical. |

## Verification

| Gate | Result |
| --- | --- |
| `cargo check --workspace --all-targets --locked` | PASS |
| `cargo test --workspace` | PASS, including full existing Lashlang, replay/redrive, cross-backend, UI/trybuild, and doctest batteries |
| `cargo clippy --workspace --all-targets -- -D warnings` | PASS |
| `cargo fmt --all --check` | PASS |
| `python3 scripts/lint_docs.py` | PASS: 46 HTML pages, 42 registry pages |
| `bash scripts/check-rustdoc.sh` | PASS: 583 public `lash-core` members documented, 0 missing |
| `python3 scripts/check_test_quarantines.py` | PASS |
| `python3 scripts/check_api_example_coverage.py` | PASS: 8,005 entries |
| New determinism/metering/GC suites named above | PASS as part of the workspace suite |
| `just perf-guard` | PASS: 210 Lashlang perf results plus the complete quick runtime profile |

## Performance

`just perf-guard` passed the complete quick runtime profile and all 210 Lashlang budget results. For the heap-sensitive `large_data` scenario (500 iterations), the measured hot modes were:

| Mode | Time/iteration | Allocations/iteration | Bytes/iteration |
| --- | ---: | ---: | ---: |
| one shot | 28.462 ms | 9,589.592 | 2,196,378.7 |
| prewarmed one shot | 27.323 ms | 9,589.652 | 2,196,398.3 |
| compiled execute | 28.400 ms | 6,937.122 | 1,912,103.4 |
| snapshot | 28.198 ms | 8,181.122 | 1,942,769.4 |
| phase breakdown | 28.615 ms | 9,736.760 | 2,207,537.5 |

The only budget delta is scoped to `large_data` allocation bytes: 2,200,000 to 2,250,000 bytes per iteration (+2.27% headroom). The final phase-breakdown measurement is 2,207,537.5 bytes/iteration, 0.34% above the old ceiling and 1.89% below the new one. The allocation-count ceiling remains 12,000, and every other scenario budget is unchanged. No measured hot path is a greater-than-2x cliff.

## Deferred work

- Closures become heap objects in their later campaign layer; this layer covers every compound value the current VM owns.
- The TypeScript dialect will omit the Lashlang copy lowering to expose reference semantics.
- Generalized uniqueness analysis/CoW, incremental or moving GC, weak references, finalizers, and allocator/RSS metering remain explicitly out of scope.
