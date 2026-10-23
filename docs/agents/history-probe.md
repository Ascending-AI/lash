# History component probe

`lash-perf history --turns 1000 --check --out history.json --store-dir target/history`
checks the FIG-7211 held-frame rule. Add `--compact-every 100` for ten frame
cycles through the real administrative compaction command. The store directory
is emptied first. Use a fresh directory per geometry.

Build with `kiln build //crates/lash-perf:lash-perf__bin --config=optimized
--materializations final --build-report <report>`, then locate the executable
with `python3 tools/buck2/outputs.py --report <report> --label
//crates/lash-perf:lash-perf__bin --single`. Run the executable through
`kiln gate lash <fork> -- <measurement-script>`. The script must apply
`ulimit -v 8388608`, record `/usr/bin/time -v`, load1, and
`perf stat -e instructions:u` for each geometry. Stores belong under `target/`;
logs, JSON, profiles and receipts belong under `.kiln/<ticket>/`.

This is a developer check, outside the merge gate. Runtime instrumentation is
behind the existing non-default `perf-witness` feature. Its disabled macro
expands to the original function body. The allocation wrapper is installed
only by the developer executable, and delegates to its established allocator.

CPU attribution is exclusive thread CPU, charged only while a future is being
polled or a synchronous scope runs. Pending, completion, cancellation and unwind
return the worker's attribution. A nested component temporarily replaces its
parent. The components are request assembly (including the pure machine's
request projection), provider lowering/encode/send, durable commit/SQLite
connection service, acknowledged owner state, plugin hooks/tool surface, trace
record preparation/delivery, and all remaining process CPU. Waiting is excluded.
The report conserves process CPU as the sum of these components plus the
explicit CPU spent measuring the request's held size. It does not assign
process-wide instructions to overlapping spans; instructions remain a whole-turn
cross-check. Allocations and allocated bytes count Rust allocation and positive
reallocation growth, excluding the SQLite C allocator. They are work counters,
not retained-heap sizes.

Held bytes are the UTF-8 JSON representation of the actual provider request's
message array, including JSON structure. Held messages are the length of that
array. These measure what the current frame sends, not old durable frames or
spare heap capacity. The extra serialization used to measure this size is
separately metered; its CPU and allocations are excluded from component work.
The fixture installs a discard trace sink with omitted content: real records
are constructed and delivered, then released without retaining a session ledger.

Each turn contains all component counters and parameterized SQL families with
completed statements, VM steps and returned rows. Cached statement VM counters
are differenced at STMT/PROFILE rather than reusing their cumulative totals.
The gate checks CPU, allocation count/bytes and SQL counts/VM steps/returned rows
separately; a growing family cannot hide behind a flat aggregate.

Without compaction the windows are turns 10–30 and 980–1000. Request build/send
work is divided by each turn's held bytes before taking medians; all other work
is compared directly. With K=100 compaction the equal-age windows are 10–30 and
910–930, compared directly, and all nine frame transitions retain their exact
before/after costs and held sizes. Every ratio must be at most 1.05. Failed
comparisons name the component/family, metric, values and ratio, the JSON is
written, and the command exits nonzero. Missing populations, failed turns and
missing frame resets are refusals, not passing measurements. The test
`latency::history_gate::tests::linear_non_history_work_fails_the_held_state_gate`
injects actual turn-proportional CPU work and proves rejection; its positive
control removes that work and checks the held-byte normalization formula.
