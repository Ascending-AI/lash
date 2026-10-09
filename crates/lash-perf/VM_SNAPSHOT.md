# VM continuation snapshot spike (FIG-5166 / S1)

Measured 2026-10-06 against runtime baseline d55354b40771f4a681588e7a90f02e551ae64447. This is developer tooling; production runtime, stored formats, epochs and facade exports are unchanged.

## Recommendation

Pull chunk-aware capture and reuse of large immutable heap payloads into 1.0 if the measured large heaps remain admitted. Small cells and history handles do not justify general incremental VM machinery: their snapshot p99 is below 0.03 ms; even the 257-frame process is 1.054 ms. But 10k numbers already reach 7.474 ms, and 1M records reach 5,079.373 ms, against design-opus §6 H5's 5 ms snapshot threshold. H5 holds for the measured small cells, not as a blanket assumption for retained tool-result heaps.

Splitting an already encoded blob into storage chunks does not remove capture, GC, validation or serialization work. Use a manifest with independently encoded, reusable heap chunks and dirty tracking for large retained data; keep the complete control continuation and broker ledger atomic at every quiet point. Do not change snapshot cadence or re-execute guest code. Whole-VM incremental checkpoints are not demonstrated necessary; incremental handling of large heap data is. Chunking alone also does not make cold restore cheap: decoding and validating 1M records takes 11.185 seconds p99. Lazy decoding or a cheaper validated representation needs measurement in the implementation lane.

## Method and machine

Local x86_64 Linux 6.8.0-137-generic; AMD Ryzen 9 5950X, 16 cores / 32 threads, approximately 125 GiB RAM. Shared host, no CPU pinning. Kiln optimized profile (`opt-level=3`, debug assertions off), default stats_alloc instrumentation, continuation format 29. Wall time uses `Instant`; nearest-rank p50/p99 over 100 samples per case after one warmup. These are instrumented microbenchmark observations, not isolated-host production latency certification.

Setup compiles the program and constructs its heap outside timing. Each sample answers the pending sleep with `Park` (production `Vm::suspend`, including GC and capture), calls `VmContinuation::to_bytes`, opens bytes with `VmInstance::open_continuation` on a pristine instance, then starts that instance from the continuation. Restore includes program/continuation validation, projection refresh and reissuing the saved sleep. Earlier guest instructions and host effects do not run again. Pristine-instance construction, explicit drops, setup, host-view construction and final completion are outside timing. Capture+serialize and decode+restore percentiles come from paired sample sums, not sums of phase percentiles.

The small cell retains two order records and their reduced total. Heap cases retain either numeric lists or lists of `{id, amount}` records. The process executes 100k counter increments, then recurses 256 times; the snapshot has 257 caller frames. It uses the shared native VM AST, which the ticket explicitly permits, to measure the VM without the TypeScript async-helper authoring constraint described below. Its full AST is saved in the JSON report. Both history cases hold the binding inside a record and retain one already-read 1 KiB message. Their backing histories contain 10 or 10,000 such messages. The adapter uses the public production `RlmHistoryProjection`; the production host descriptor is private. Fresh host views are constructed independently for restore. Bounds are explicitly unbounded for these stress measurements; the default frame bound remains 1,024.

Reproduce all ten workloads from the fork:

```sh
. ./env.sh
kiln run --config=optimized //crates/lash-perf:vm-snapshot__bin -- --out .kiln/FIG-5166/snapshot.json --samples 100
```

Use repeated `--case <name>` to select rows; unknown names are refused. The seven heap rows were measured before packaging the same measurement routines as the standalone binary, through `kiln run --config=optimized //crates/lash-perf:lash-perf__bin -- vm-snapshot --out .kiln/FIG-5166/snapshot.json --samples 100`. That temporary CLI is removed. Its command completed seven cases, then stopped on an invalid deep fixture. The final three rows were measured through:

```sh
kiln run --config=optimized //crates/lash-perf:vm-snapshot__bin -- --out .kiln/FIG-5166/snapshot-tail.json --samples 100 --case deep-process --case history-10 --case history-10000
```

The seven completed heap rows were not repeated. Only their untimed frame-depth inspection and packaging changed afterward. `snapshot-combined.json` retains 1,000 valid optimized cycles, ten warmups, all raw timings, sizes, source programs and successful completion witnesses. A default-profile fixture check additionally ran 300 cycles for the final three rows; it is separate from these tables. Invalid fixture runs are preserved in logs and excluded from measurements.

## Results

| Workload | Bytes | Frames | Capture p50/p99 ms | Serialize p50/p99 ms | Deserialize p50/p99 ms | Restore p50/p99 ms |
|---|---:|---:|---:|---:|---:|---:|
| small-cell | 2,179 | 0 | 0.011 / 0.018 | 0.006 / 0.010 | 0.042 / 0.067 | 0.007 / 0.010 |
| numbers-10000 | 671,053 | 0 | 2.064 / 4.708 | 1.815 / 3.020 | 29.586 / 41.633 | 1.505 / 1.910 |
| numbers-100000 | 6,701,055 | 0 | 15.696 / 23.842 | 13.953 / 20.265 | 203.613 / 294.606 | 13.770 / 20.293 |
| numbers-1000000 | 67,001,057 | 0 | 121.007 / 221.388 | 96.179 / 184.327 | 1425.726 / 2701.009 | 101.084 / 193.836 |
| records-10000 | 2,297,053 | 0 | 13.971 / 19.010 | 6.234 / 9.148 | 53.208 / 59.945 | 8.033 / 9.562 |
| records-100000 | 23,160,860 | 0 | 274.434 / 320.062 | 101.304 / 128.222 | 723.220 / 1085.143 | 172.425 / 217.874 |
| records-1000000 | 233,598,867 | 0 | 3070.619 / 3675.220 | 1312.238 / 1568.703 | 7807.120 / 9123.946 | 1821.444 / 2263.172 |
| deep-process | 174,301 | 257 | 0.395 / 0.652 | 0.310 / 0.487 | 2.228 / 2.741 | 0.290 / 0.491 |
| history-10 | 2,466 | 0 | 0.004 / 0.006 | 0.004 / 0.007 | 0.020 / 0.034 | 0.005 / 0.008 |
| history-10000 | 2,466 | 0 | 0.004 / 0.008 | 0.004 / 0.007 | 0.020 / 0.031 | 0.005 / 0.008 |

| Workload | Capture + serialize p50/p99 ms | Deserialize + restore p50/p99 ms |
|---|---:|---:|
| small-cell | 0.018 / 0.028 | 0.050 / 0.074 |
| numbers-10000 | 3.927 / 7.474 | 31.022 / 43.405 |
| numbers-100000 | 29.303 / 41.897 | 219.151 / 309.217 |
| numbers-1000000 | 217.027 / 397.223 | 1528.316 / 2894.844 |
| records-10000 | 20.129 / 26.425 | 61.251 / 69.793 |
| records-100000 | 375.769 / 455.694 | 897.344 / 1303.016 |
| records-1000000 | 4398.144 / 5079.373 | 9642.109 / 11185.434 |
| deep-process | 0.709 / 1.054 | 2.519 / 3.099 |
| history-10 | 0.009 / 0.015 | 0.025 / 0.040 |
| history-10000 | 0.009 / 0.015 | 0.025 / 0.039 |

| Heap | Elements | Bytes/element | Capture + encode p50 ms per 10k elements | Decode + restore p50 ms per 10k elements |
|---|---:|---:|---:|---:|
| numbers | 10,000 | 67.11 | 3.927 | 31.022 |
| numbers | 100,000 | 67.01 | 2.930 | 21.915 |
| numbers | 1,000,000 | 67.00 | 2.170 | 15.283 |
| records | 10,000 | 229.71 | 20.129 | 61.251 |
| records | 100,000 | 231.61 | 37.577 | 89.734 |
| records | 1,000,000 | 233.60 | 43.981 | 96.421 |

Bytes grow approximately linearly: numeric lists approach 67 bytes per element; two-field records grow from 229.71 to 233.60 bytes per element as object identifiers widen. Across 100x more elements, numeric-list capture+encode p50 grows 55.3x and decode+restore 49.3x; records grow 218.5x and 157.4x respectively. The normalized rows show the extra cost per element for larger record heaps. These are observed growth curves, not an asymptotic complexity proof.

For 1M records, capture alone is 3,070.619 ms p50, encoding is 1,312.238 ms, decoding is 7,807.120 ms, and restore validation/installation is 1,821.444 ms. The cause is a complete heap capture and full wire conversion per quiet point (`Vm::suspend` and `continuation_serde::serialize_heap` in `crates/lash-vm/src/runtime/vm/continuation.rs`). Decode builds a JSON value tree, reconstructs the typed heap and validates it (`continuation/types.rs`); resume validates and refreshes again. This spike measures those production seams directly and changes none of them.

## Portability

Ordinary heap values, frames and counters are data. Bytecode is intentionally absent and must be supplied from the matching retained executable artifact; resume verifies its identity. Projected values are plain data (ADR 0132 §9): a resource projection encodes its `name`, `type_name` and `ResourceRef`, never a live descriptor, pointer or socket (`crates/lash-vm/src/runtime/projected_wire.rs`). Decoding yields the same projection, which reads through the provider registered for its type on the restoring side, including the history alias nested inside the heap.

Both history snapshots are exactly 2,466 bytes. Increasing message content in the backing history from 10 KiB to 10,000 KiB does not increase VM bytes. The already-read first message is in the heap; the rest of the host transcript is not. Both cases resume to the expected result with independently recreated views and fail with a typed `ProjectionRefused` (`NoProvider`) when the provider is omitted. This is an in-process fresh-instance witness, not a multi-node durability test.

Current snapshots can nevertheless carry node/run-local identity: exported custom descriptors are renamed `worker-projection/<namespace>/<registry key>/<name>` in `crates/lash-vm-client/src/projections.rs:220`. Their resolver requires the original namespace and registry (`crates/lash-vm-worker/src/projection.rs:144`); `crates/lash-vm-runtime/src/worker_execution.rs:152` refuses handover when descriptors were exported. No socket is serialized, but the token is not independently resolvable on another owner. Thus today's arbitrary VM snapshots are not universally portable. REPORT.md's provider-backed `{type, ResourceRef}` and pinned transcript revision are required for 1.0; the current history name alone supplies no revision guarantee. The report's settled provider decision supersedes older node-pinning prose in the design and verifier.

## Proof and limits

The ten optimized rows contain 100 samples each, ten correct resumed terminal results, and two typed missing-history-view refusals. Final workspace `kiln clippy` passed. `kiln check //crates/lash-perf:vm-snapshot__bin` passed, `kiln fmt` passed, and `kiln sync` regenerated membership for one binary and two feature witnesses. No unit tests were added, changed or deleted (executed unit tests: 0); this is not a product bug fix, so a red law is inapplicable. No facade or stored shape changed. No build rules or surviving execution budgets changed. No first-party compile needed a memory-scale retry.

This does not certify the complete ten-tool durable-turn H5 gate, database/network latency, mutation-heavy dirty-chunk behavior, process-to-process restore, or the future provider architecture. Those remain implementation/performance comparison work. TypeScript recursive async helper fixtures on the baseline hit TDZ or `PendingTool` on an awaited plain number, and removing helper awaits is refused as `TS_AWAIT_REQUIRED`. The native AST fixture isolates the shared VM measurement; the authoring limitation is left unchanged and its logs are retained.
