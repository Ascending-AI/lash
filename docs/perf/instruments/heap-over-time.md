# Allocation sites over time beside RSS

`scripts/profile_heap.py --heaptrack` records allocation history for the existing
`persistent-node-waves` boundary population. One real durable node, backed by a
SQLite file, stays alive across every wave of `send()` calls. Submission pauses
and authoritative drain status settles before each sample; background work can
continue. The node shuts down once, after the last wave.

This belongs in the heap profiling script because it attributes retained bytes
to allocation stacks. The wave workload already samples RSS through
`perf_support::memory::process_memory_sample`; the script extracts that series
and does not introduce another RSS collector.

From a sourced Kiln fork, with heaptrack 1.5, heaptrack_print, zstd and readelf on
PATH:

```sh
. ./env.sh
python3 scripts/profile_heap.py --heaptrack --case persistent-node-waves \
  --operations 500 --callers 1 --out-dir "$PWD/.kiln/FIG-5721/heap-history"
```

The output directory must be fresh. `--operations` is the number of waves;
`--callers` is the number of sequential completed turns per wave. The example is
a long diagnostic population, not a duration guarantee: elapsed time depends on
the host and instrumentation. Use a smaller population for instrument smoke.
Only this population is supported in heaptrack mode.

Heaptrack mode defaults to two Tokio worker threads through
`TOKIO_WORKER_THREADS`; `--worker-threads N` changes that value, which is recorded
in the receipt. This bounds profiled runtime setup on hosts with many CPUs.
It changes the diagnostic's execution geometry, not Kiln's build budgets.

The script builds `//crates/lash-perf:lash-perf__bin` using
`//tools/buck2:profiling`, optimized code with line tables and frame pointers.
To reuse that materialized executable, pass `--no-build --build-report PATH`
with the profiling build's native report. Keep the executable available for
later symbol resolution; the receipt records its path and SHA-256.

## Artifacts and quantities

| Artifact | Meaning |
| --- | --- |
| `heaptrack.zst` | Interpreted allocation/free history with stack identities for the parent process, from profiler initialization through exit. |
| `top-peak.txt` | Ten unmerged allocation stacks ranked by bytes live at the process's global heap peak. |
| `top-leaked.txt` | Ten unmerged stacks ranked by requested bytes still outstanding at process exit. Suppressions are disabled. |
| `top-allocations.txt` | Ten unmerged stacks ranked by allocation-call count over the whole profiler lifetime. |
| `top-sites.txt` | Complete heaptrack_print output; all rankings and the time series come from one analysis invocation. |
| `heap-over-time.massif` | Heap totals at successive interval peaks, with detailed retained-stack trees every tenth snapshot and at exit. Times are seconds since profiler initialization. Heaptrack's default threshold groups stacks below 1% into a remainder. |
| `boundary.waves.jsonl` | Original wave observations, including Rust allocator counters and `/proc` RSS/HWM, before waves, after each settled wave and after node shutdown. |
| `rss.jsonl` | The same observations projected to PID, wave, completion count, node lifetime, elapsed nanoseconds and RSS/HWM in KiB. |
| `boundary.json` | Completed population, allocator counters and settlement receipt. |
| `receipt.json` | Build/tool identity, allocator/interception evidence, process scope, clock alignment and RSS endpoints. |
| `linkage.txt`, `run.log`, `interpret.log`, `print.log` | ELF preflight, workload/profiler output, deferred interpretation and analyzer diagnostics. |

Capture first writes `heaptrack.raw.zst`. Interpretation and symbol resolution
run after workload exit to avoid analyzer back-pressure. After successful
analysis the script deletes this large raw scratch file and its fresh SQLite
store, retaining the complete interpreted recording and all observations. Failed
runs retain scratch for diagnosis. Allow several gigabytes of temporary disk
space for a long population; the receipt records the raw recording's byte count.

Heaptrack's human-readable byte units use decimal scaling. RSS uses KiB.
Requested heap bytes exclude allocator slack, mappings and stacks; RSS also
includes heaptrack's in-process overhead. The ranks retain full Rust symbol
names. Backtrace merging is disabled because heaptrack warns that merged peak
consumption is inaccurate.

Use the detailed history or open the recording in `heaptrack_gui` to find which
stacks grow while the node remains alive. The final leaked-byte ranking is an
exit endpoint: allocations released during shutdown no longer appear there.
The massif snapshots are interval peaks rather than exact wave endpoints.
An outstanding allocation is not by itself a product leak.

RSS and allocation history describe the same PID and run. The RSS clock starts
at population entry, after profiler initialization and command-line setup.
The script bounds the population's clock origin relative to wrapper spawn by
observing the first flushed sample, normally within a 100 ms polling interval.
The receipt records that bound; wrapper spawn itself precedes heaptrack
initialization. These clocks are approximately aligned, not interchangeable.
Use completed-turn and wave indexes to distinguish workload growth from setup
and shutdown. `rss_last_live_node_kib` and `rss_end_kib` distinguish the final
live-node sample from the shutdown sample.

## Allocator and process reach

The default binary installs `StatsAlloc<System>`; System uses dynamically
interceptable allocation functions. The script refuses missing ELF dynamic
linkage, malloc imports, line tables or symbol tables. It checks the workload's
allocator mode, positive heaptrack allocation counts, resolved Rust workload
stacks and an observed call count at least as large as the workload's Rust
allocation counter. If those checks fail, it exits with an error rather than
reporting an empty/native-only profile as success. When the run lasts long
enough to observe `/proc/PID/maps`, the receipt also identifies the actual
heaptrack preload library. A different allocator needs its own interception
proof or a System build; this command makes no automatic allocator substitution.

**Child capture is absent from the supported scope.** The receipt explicitly
sets `children_captured` to false. This population runs its node and synthetic
provider in-process and starts no VM worker. Heaptrack's interpreter and
compressor processes are tooling, not measured workload children. Do not use
this receipt to claim worker coverage; a worker workload requires separate
per-process recordings and symbol proof.

The output is diagnostic and has no growth gate or timing certification. It
does not estimate profiler overhead. Hotspots found here belong in a separate
product investigation.

Upstream references: [heaptrack's Rust support and usage](https://github.com/KDE/heaptrack#heaptrack-with-rust)
and [heaptrack 1.5's text and massif analyzer](https://github.com/KDE/heaptrack/blob/v1.5.0/src/analyze/print/heaptrack_print.cpp).
