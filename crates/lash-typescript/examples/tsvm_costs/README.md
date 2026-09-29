# Worker cost measurements

FIG-4157 is measurement only. These examples do not implement the production
worker or its authority, reset, recovery, admission or watchdog contracts.

Source the fork's `env.sh`, then build the two examples with the shared executor:

```sh
kiln build --compilation_mode=opt --remote_download_outputs=toplevel //crates/lash-typescript:tsvm_costs__example //crates/lash-typescript:tsvm_allocations__example
```

Run the downloaded binaries on the same machine, sequentially. Pass the fork
root as `--repo` so the fixtures come from that exact checkout:

```sh
bazel-bin/crates/lash-typescript/tsvm_costs__example --verify --repo "$PWD"
bazel-bin/crates/lash-typescript/tsvm_costs__example --repo "$PWD" --output "$PWD/measurements"
bazel-bin/crates/lash-typescript/tsvm_costs__example --rss --output "$PWD/measurements"
bazel-bin/crates/lash-typescript/tsvm_allocations__example --repo "$PWD" --output "$PWD/measurements"
```

The defaults are 10,000 samples per warm boundary and 200 new processes. The
fresh baseline also takes 200 samples after one second idle. Sample floors are
mandatory. The `--verify` mode has no sample floor and checks truncated and
oversized frames, worker results, snapshot fixed points, continuation fixed
points and resume outcomes. No service credentials or live effects are used.

## Measurement contract

The primary boundaries are fresh parse/compile/execute, exec to the first
Ready frame, pristine pool checkout, complete JSON pipe round trip, current
snapshot/continuation codecs, and drop/replace of an owned VM state prototype.
Use p50, nearest-rank p99 and maximum nanoseconds. Keep every observed sample.
Raw durations are in `samples.csv`; descriptive percentiles are in
`summary.csv`. These closed-loop measurements describe service time on the
observed host. They do not estimate open-loop overload or production capacity.

Cold means a new process and fresh VM state. The executable and OS page cache
are allowed to remain hot. The first worker announces Ready before parsing any
source. Prewarmed checkout measures a local pool pop/return lower bound. The
warm simple case includes the request, fresh parse/compile/execute and response.
Batch widths 1, 2 and 4 issue one simple script per worker concurrently and
measure time until all responses arrive. They include parent framing and
scheduling, but no production pool locks, authority checks or journaling.

The baseline calls the functions in `monty_comparison.rs` directly. Compile
and execute semantics are unchanged. The ten-feed session verifies its exact
report. Six fixed accepted differential rows cover coercion, nested arrays,
loops and callbacks. Three complete session fixtures cover mutable arrays and
records, exotic objects and spread. Each session cell starts with the actual
previous cell state. A console effect is appended to pure cells to make a
continuation observable through today's public checkpoint API. VM snapshots
contain guest state only. They exclude the RLM parent authority envelope.

Two seeded state string cases carry 8 KiB and 1 MiB, one source reaches the
current 64 KiB parser cap, and a dense array carries 10,000
elements. These are cap probes, not claims about typical sessions. Current
snapshot encoding is canonical MessagePack; continuation encoding is its
current serde JSON representation. Snapshot decode includes structural and
semantic validation and canonical re-encoding. Decode timings include dropping
the decoded result. Encode timings include dropping the output buffer. They
are supporting estimates for the future worker codec, whose shape may differ.

The reset prototype owns current `State`, `ExecutionScratch`, optional
`VmContinuation` and compiled-program cache entries. Each dirty state is decoded
outside the timer; drop plus fresh/template replacement is timed. Scratch is
fresh in both strategies because its current public API does not implement
Clone. The pristine template has no guest inputs. Interleave strategies on
every iteration. This cannot prove ownership of future compiler/regex caches
or the production reset law. Choose fresh unless a repeatable material
advantage above 10% appears in both representative and large state tails.

Decode cumulative allocation is measured by `tsvm_allocations`, a separate
System allocator wrapper, for 10,000 samples per codec and fixture. It counts
all allocation requests, including the entire reallocated buffer. It does not
measure RSS, peak live allocation or timing. The timing executable uses the
normal allocator. Node/depth observations refer to the JSON continuation tree,
not a future bounded decoder's exact charge schedule.

RSS comes from Linux `/proc/<pid>/status`, once for each pristine cold worker,
and separately after workloads. The `--rss` supplement appends 10,000 readings
of one idle worker after 100 short scripts. Run it after the default benchmark
and use the same output directory. It is total RSS, including shared code pages;
it is not incremental PSS and cannot be multiplied into a precise host budget.
Workers are killed and reaped when released by this measurement executable.

Record checkout SHA, binary SHA/size, toolchain, target, optimization flags,
features, CPU/OS, power governor and shared-host load alongside the raw data.
Run verification before timings; run allocation probes separately. Reject
semantic differences. Treat reset choices inside the 10% margin as inconclusive.
Pool size and bounds remain explicit host presets; corpus maxima and observed
tails justify initial headroom, not universal workload safety or watchdog
ceilings for arbitrary valid compute. No samples or slow attempts are excluded.
