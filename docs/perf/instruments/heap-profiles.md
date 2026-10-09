# Lifecycle, child and worker heap profiles

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
python3 scripts/profile_heap.py --population vm-worker --out-dir "$E/worker-heap"
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

The `lash-vm-worker` helper's opt-in `dhat-heap` feature uses DHAT 0.3.3.
`--heap-profile-dir` travels in argv, preserving the worker's empty environment.
It writes `vm-worker-<pid>.dhat.json`, covering bootstrap validation after the
empty-environment check through
the **first clean reset**, and flushes before `ResetDone`. The pool kills and
reaps retired workers, so waiting for process-exit destructors would lose the
profile. Abruptly failed workers do not promise a profile. The matrix's
`--heap-smoke --worker <helper> --out <directory>` runs one zero-effect case;
adding `--heap-profile-dir <directory>` selects the worker profile. This smoke
mode carries no worker performance gate.

DHAT's `tb`, `gb` and `eb` are cumulative requested bytes, site live bytes at
the simultaneous process heap peak, and site live bytes at profile end.
Independent site maxima (`mb`) must not be summed into a simultaneous peak.
Profiles observe this process's Rust allocator; they exclude database-server
memory and allocations bypassing that allocator. Sidecar receipts identify
role, PID, window, units and statistics.

The overhead wrapper reports one ordered on/off wall-duration difference per
instrument, including parent startup, waited children and profile flush.
Its `configuration` object labels workload settings and limits separately
from measured durations; worker receipts identify the heap-smoke population.
Both builds use the same profiling platform; the off population uses
stats_alloc for the harness or System for the dedicated worker. Boundary
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
