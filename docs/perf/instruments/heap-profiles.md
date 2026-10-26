# Cell, lifecycle and worker heap profiles

## One cell's memory

```sh
just profile-cell-mem path/to/cell.ts --stack
just profile-cell-mem path/to/cell.py --worker --stack
just profile-cell-mem path/to/build.ts --worker --cells 2 --records 200000 --out-dir "$E/cell"
```

The input is an ordinary TypeScript or Python cell. The diagnostic host offers
`perf.finish(value)` and the existing kernel benchmark's scripted operations.
A cell without `perf.finish` still runs, but its result receipt is null. External
services and arbitrary host tools are not part of this driver.

The command builds the opt-in `cell-memory` example through Kiln's optimized,
symbolized platform. It runs one observer-off/on pair with the **same executable**
and `perf stat -e instructions:u`, with an 8 GiB address-space cap. A fresh output
directory retains each command's output, raw counters, profiles, `summary.json`
and `overhead.json`. `--no-build --build-report <report>` reuses that exact build.
No number from the observed population is a production-cost estimate. The
instruction ratio includes startup, waited workers, observation, symbolization
and output. Shared-host wall time is not claimed.

`--worker` uses the real credential-free, confined worker entry, pool, IPC and
page service. `--cells 2` runs the cell twice in the same PID with a clean reset between
them and requires separate `vm-worker-<pid>-cell-<ordinal>.json` files. The default runs one cell. These are
per-cell windows, including run admission and ending before machine destruction.
They remain available after earlier cells and resets. The diagnostic host names
larger compute/serialization watchdogs, a 1 GiB guest memory bound and a 10,000
call-depth bound. Charge metering remains exact. These host settings are explicit diagnostic settings; inspect `PoolConfig`
for the current serving defaults.

Allocation sites retain unresolved instruction addresses and DWARF-resolved
Rust file/line and inline frames. Each full native stack and guest context has
its own count, cumulative requested bytes, maximum live bytes, live bytes at
the guest peak, and end live bytes. Reallocation counts as one request for the
new size; `allocations` and `reallocations` also remain separate. The count table expands beyond ten rows until it covers at least 95% of
allocations. Separate tables rank traffic, guest-peak live bytes and end live
bytes. The text table groups native source sites and reports the largest individual stack/context
peak in each group. It does not sum independent maxima. `--records` supplies an
explicit useful-record denominator, rather than guessing it from a loop bound.
The full profile also joins each site to the published guest code/statement or
native operation, with allocations divided by native call counts or code
statements plus expressions. Both the guest context and exact work observer
are enabled only in the active pass. Their projection costs belong to observer
overhead. The launcher requires equal results and exact charge in both modes.

The capacity ledger is read at statement verdict points. It splits the meter's
actual definition into resident value slots, control state, shared layouts and
the excess of a restored sealed-heap floor. A new observed ledger peak records
every allocation site's current live bytes and current process RSS. This is a
mid-cell snapshot, rather than an exit/reset proxy. Native temporary values can
peak between verdict points. Such a transient maximum is not an exact ledger
peak from this instrument.

Process snapshots distinguish requested Rust allocator bytes, glibc in-use
capacity, free arena space, anonymous RSS, stack RSS and file-backed RSS. A
window contains only its newly allocated live objects. The process total also
contains preexisting allocations and observer storage. Their difference is
shown explicitly by the two totals; it is not labelled a guest leak. Free glibc
arenas identify allocator slack; file RSS includes executable and library pages.
Allocator capacity and requested bytes can include nonresident pages, so they
are not additive components of RSS. The text reader prints a signed identity
for `RSS - window live`: preexisting/observer requested bytes, glibc in-use minus
requested bytes, free arena capacity, file and stack RSS, and an explicit
anonymous-residency-minus-allocator-capacity residual. This arithmetic balances
without claiming the residual separates nonresident pages from nonallocator
anonymous mappings. Allocations bypassing Rust's allocator are
absent from site counts. Unavailable `/proc` quantities must not be read as zero.
The collector attempts Linux's `clear_refs=5` at window start and reports whether
it succeeded. Only a successful reset gives the subsequent RSS high-water mark
a cell-window meaning; executable hashing is outside this process.

`--stack` paints unused native stack bytes and scans the paint after execution,
so its watermark measures written stack occupancy. It reports the thread ID,
initial unpainted margin, byte resolution and saturation. The measurement
includes observer frames and paint commits stack pages. It covers the original
Linux x86_64/glibc thread stack, not separately reserved parser threads or
alternate stack mappings. No sampled peak causal stack is captured. A configured
stack reservation, future size or sampled stack pointer is not substituted for
that watermark. The unpainted margin is a lower-resolution floor when execution
does not overwrite any paint.

The collector is development-only and is absent without `cell-memory`. Its
worker syscall policy permits the same file reads/writes and read-only stack
limit query as DHAT. Release-stamped packages reject the feature at compile time.

## Lifecycle and DHAT windows

Each command builds matched symbolized, optimized feature variants through
Kiln, runs each instrument mode in a fresh process and store, and writes raw
DHAT profiles, process/window receipts and `overhead.json`. A fresh output
directory is required. Subsequent boundary/latency commands can reuse the
first build with `--no-build --build-report "$E/redrive/build.json"`.

```sh
python3 scripts/profile_heap.py --case root-redrive --out-dir "$E/redrive"
python3 scripts/profile_heap.py --case parked-takeover --out-dir "$E/takeover" \
  --no-build --build-report "$E/redrive/build.json"
python3 scripts/profile_heap.py --case process-lifecycle --operations 2 \
  --out-dir "$E/lifecycle" --no-build --build-report "$E/redrive/build.json"
python3 scripts/profile_heap.py --population latency --case cross-worker \
  --out-dir "$E/latency-heap" --no-build --build-report "$E/redrive/build.json"
python3 scripts/profile_heap.py --case persistent-node-waves --operations 3 \
  --callers 2 --out-dir "$E/waves" --no-build --build-report "$E/redrive/build.json"
```

Boundary `--dhat-out` starts profiling before runtime setup and ends after
workload shutdown and runtime teardown.
Boundary `--worker-stack-bytes` still configures that runtime and can be
combined with `--future-out`; all boundary cases share the same heap options.
Latency `--dhat-out` passes a separate path to each selected child:
`<parent-stem>.<case>.child.dhat.json`.
The parent covers case setup through node teardown; the child covers runtime
and node setup through shutdown and runtime teardown. These remain diagnostic
latency selections (exit 2 because `fast` did not run). The wrapper requires
answered samples without case errors and both populated profiles.

Replay host windows use `--population replay --workload <scenario> --turns N
--profile-turn K --out-dir <fresh-directory>`. This uses the same optimized
executable for inactive/active profiler modes and records build, selected-turn
window and resource receipts. See [replay attribution](replay-workloads.md#heap-profiling)
for the complete command, range semantics and quantity limits.

The `lash-vm-worker` helper's opt-in `dhat-heap` feature uses DHAT 0.3.3.
`--heap-profile-dir` travels in argv, preserving the worker's empty environment.
It writes `vm-worker-<pid>.dhat.json`, covering bootstrap validation after the
empty-environment check through
the **first clean reset**, and flushes before `ResetDone`. The pool kills and
reaps retired workers, so waiting for process-exit destructors would lose the
profile. Abruptly failed workers do not promise a profile. The profiled worker
serves under the same confinement as any other. Its feature forwards
`lash-vm-client/dhat-heap`, which declares four configured silence windows for
startup (20 seconds at standard bounds). Heap tracing of registry assembly and
library compilation took up to 9.9 seconds on the remote pool; both stay before
Ready so the first turn runs a warm library. Default builds and serving-worker
silence keep one window (5 seconds at standard bounds), and checkout still
bounds a newly spawned checkout. Its seccomp filter also admits
`openat`, `statx`, `fstat` and `lseek`, which file reading and the flush need,
and the production build admits none of them
(`a_heap_profiled_worker_writes_its_profile_under_confinement_across_a_reset`).
The profiling filter also admits a read-only query of the worker's own
`RLIMIT_STACK`: opening `/proc/self/maps` lets stacker's `pthread_getattr_np`
probe advance to that query. It cannot change the stack limit. The feature
is compiled only for `0.0.0-dev` packages; release stamping rejects it while
optimized developer profiling remains available. Replay's re-executed
Python workers use this same window and write separate PID-named profiles
beside their receipt, as described in [replay workloads](replay-workloads.md).

DHAT's `tb`, `gb` and `eb` are cumulative requested bytes, site live bytes at
the simultaneous process heap peak, and site live bytes at profile end.
Independent site maxima (`mb`) must not be summed into a simultaneous peak.
Profiles observe this process's Rust allocator; they exclude database-server
memory and allocations bypassing that allocator. Sidecar receipts identify
role, PID, window, units and statistics.

The overhead wrapper reports one ordered on/off wall-duration difference per
instrument, including parent startup, waited children and profile flush.
Its `configuration` object labels workload settings and limits separately
from measured durations. Both builds use the same profiling platform; the
off population uses stats_alloc. Boundary
commands additionally measure future-size collection alone in the same
stats_alloc binary. This measures the observed instrumented population cost,
including bookkeeping and output. Shared-host noise, order and allocator
differences prevent interpreting it as a calibrated profiler overhead or
regression baseline. Allocation counts from the two allocator modes are not
numerically comparable.

`persistent-node-waves` keeps one real SQLite node and session alive across
`--operations` waves of `--callers` sequential sends. Each wave pauses
submission and requires authoritative drain status to be drained. Its
`<receipt-stem>.waves.jsonl` records node-open, every settled wave and node-close
endpoints. Rows identify the combined host/node process role, completed turns,
process-lifetime outstanding requested bytes and allocation totals, RSS in
KiB and the process-lifetime RSS high-water mark. Profiled runs also record
DHAT-window live bytes and its simultaneous heap high-water mark; those fields
are absent values when profiling is off. Quiescence means settled work, not
stopped background maintenance. The sampler streams rows and clears per-turn
phases between waves so its retained observation buffers do not grow with N.
Harness and runtime allocations still share the process allocator. No smoke
run sets a growth gate.

Boundary `--future-out <path> --future-top N` opens an exclusive process window
before setup and reports the largest N concrete future kinds/types observed
before tracing wraps `task::spawn` or `effect.run_in_place` boxes its body.
Sizes are inline state in bytes, not pointed-to allocations, Tokio headers,
retained heap or native stack use. Constructions are counted in the explicit
window; they are not live-instance counts. Ties are deterministic and omitted
kinds are counted. Recording is compiled out without `perf-witness`, and is
inactive without an open window. The wrapper's separate `future-sizes` mode
writes this receipt without DHAT so its overhead is distinguishable.

## Prior-turn and compaction cost

One maintained command builds the existing optimized, symbolized replay host
and worker, then runs fresh SQLite stores with stats_alloc and inactive/active
DHAT at each selected prior depth:

```sh
. ./env.sh
python3 scripts/profile_heap.py --population replay \
  --workload crates/lash-perf/workloads/replay/prior-turn-probe.json \
  --prior-depths 2 5 8 --out-dir .kiln/FIG-6623/prior-turn
```

The supplied workload repeats exactly the same lookup and answer. Custom
scripts must select one session's non-setup turns with identical literal user
and reply data; turn/session substitutions refuse comparison. Replay adds its
ordinal routing marker to the input; that metadata is disclosed separately.
Each depth profiles only its next turn after building history with profiling
inactive. Both modes use the same DHAT-capable executable; `off` retains the
existing stats allocator. Native workers use the separately fingerprinted
unprofiled executable. Each run has an 8 GiB virtual-memory cap.

`prior-turn-cost.json` contains the raw selected ordinary rounds from the
inactive-profiler pass and their per-session metrics, plus DHAT source stacks
from the active pass. Full stacks are keys, with ASLR addresses removed;
`source_sites[*].stack` indexes the shared `frame_table`. The text reader prints
the ten largest absolute robust traffic slopes with their populations and
uncertainty (long symbols are abbreviated only in text). `tb`
is source-site allocated traffic; `eb` is the bytes allocated **in this window**
still live at its end; `gb` is those allocations live at the simultaneous heap
peak. Neither live quantity includes preexisting history. Retained history,
worker and native heaps are unavailable (`null`), never inferred from traffic.
A site absent from a complete DHAT capture allocated zero bytes in that window.

`compaction-cost.json` reads the raw `compactions` records excluded by replay's
ordinary summary and duration-history reader. It uses every compaction from the
deepest inactive-profiler capture once. Shorter fresh histories repeat its early
depths; selecting one complete history avoids weighting those depths by extra
prefix runs.
Compaction after the final turn is not executed by replay. The sample command
therefore observes compactions at history depths 3 and 6. These are per-session
round averages, with separate history and frame coordinates; neither series is
an individual-session latency-tail estimate. FIG-6570's committed call-ledger
join remains unavailable until that dependency lands; scripted model-call
counts are retained in their existing units.

Both files preserve producer experiment identities, exact executable hashes,
backend/settings, useful-work digest, observer modes, selected window and
successful terminal population. An experiment ID includes observed run identities
and the heap window's process identity; repeated executions at the same depth
remain distinct. The offline reader refuses duplicate captures and missing or
mixed identities. The standard probe starts no native worker; its worker
fingerprint identifies the configured artifact. Re-read archived captures without executing the runtime:

```sh
python3 scripts/prior_turn_cost.py .kiln/FIG-6623/prior-turn/depth-* \
  --out-dir .kiln/FIG-6623/prior-turn
```

The text table and JSON retain `n` and distinct-depth count for every metric.
OLS and Theil–Sen slopes are in metric units per prior turn (and separately per
frame turn). Fewer than two distinct depths yield unknown slopes. OLS residual
standard error requires more than two observations. The min/max pairwise slope
range describes dispersion, **not** a confidence interval; this small population
cannot establish a zero-growth promise. Raw samples remain available for a
larger experiment. There is no threshold, automatic budget or regression gate.

Every depth also retains one ordered local `perf stat -e instructions:u`
inactive/active pair, counter runtime/running fraction in `perf_counter_fields`,
load1, `/usr/bin/time -v` peak RSS and raw counter/resource files. Its instruction
difference covers whole process startup, prior-history creation, selected turn,
shutdown and profile flush, with identical useful work. DHAT overhead is
**diagnostic**, not calibrated or subtracted from turn slopes. Shared-host wall
time remains unqualified. The offline reader adds no runtime observer work.

The [FIG-7432 measurement report](../evidence/FIG-7432-cell-memory.md) records
the 200k allocation sites, the 100 MB reconciliation, stack watermarks and
same-executable instruction pairs.
