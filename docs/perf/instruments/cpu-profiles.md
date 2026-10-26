# CPU and OS blocked-time profiles

Source `env.sh` in an isolated Kiln fork. Install `perf`, LLVM `llvm-symbolizer`,
GNU `c++filt` with Rust support, and Inferno's `inferno-collapse-perf`, `inferno-diff-folded` and
`inferno-flamegraph` on PATH. These are local diagnostic tools; builds use Kiln's
existing profiling platform, optimization, line tables and frame pointers.

## One source cell

```sh
just profile-cell /absolute/path/build-sort.ts
just profile-cell /absolute/path/build-sort.py --worker
just profile-cell-diff HEAD~1 /absolute/path/build-sort.ts
```

`profile-cell` builds the symbolized optimized binary through Kiln. It selects
`--target-platforms //tools/buck2:profiling -c kiln.rust_profile=optimized` in
one build command; do not combine that platform with `--config=optimized`.
The input must be an effect-free `.ts` or `.py` cell. The default output directory is
`<file>.profile`; use `--out <fresh-directory>` for another capture.
`--no-build --build-report <report.json>` reuses a materialized Kiln artifact.
The commands need Linux x86-64, hardware perf permissions,
`llvm-addr2line` and `inferno-flamegraph`.

The text tables rank guest functions, Rust functions and their joined samples
by inclusive and self CPU samples. Guest source maps identify authored source
lines. Callable bodies without a direct name inherit the smallest enclosing
named function's source-map range and carry a `[body]` label. Helper functions
retain their library names. Each native stack and its
guest context come from one interrupt of the executing thread, not separate
runs. Function tables group by the function's mapped body location; the joined
view uses each frame's current statement line so build and sort sites within
one function remain distinct. A thread CPU timer requests 100 samples per CPU second; this is CPU
residency rather than the older wall-residency sampler. Native frame-pointer
walks retain inline frames after batch symbolization. The Rust text tables
attribute inlined standard-library/dependency work to the innermost Lash
source frame. Folded artifacts retain the complete inline chain. The interrupt handler
copies into preallocated storage; source resolution and output happen after
execution. Capacity loss and timer overruns fail the command and retain raw
capture evidence. Truncated stacks, refused context reads and unresolved
addresses remain explicit. System libraries without frame pointers can stop
the native walk; truncated chains retain the sampled leaf and known frames.
A tiny cell with no samples is refused.

`--worker` executes the same fresh-cell production embedding in the real
`lash-vm-worker` executable through its diagnostic entry. This is a standalone
worker-process capture, without the serving host's IPC, checkpoint or durable
commit costs. No deployed two-cell state is imported. Charge and memory
metering stay enabled. No VM optimization is part of this instrument.

Totals include unscaled `instructions:u`, enabled/running times and running
fraction, allocation and reallocation calls, exact charge and wall time over
run-to-terminal. Instructions are refused below 99% PMU running time. These
are observed-run totals, including instrumentation overhead, not an
uninstrumented performance claim. Prepare/start/drop are outside the window.
The command bounds the workload to 90 seconds and 8 GiB of address space.
Symbolization uses one batch of unique addresses per ELF object and disables
external debug downloads. A cache keyed by the complete raw-capture digest
retains resolved inline chains for offline report regeneration. `identity.json`, `raw.json`, `symbols.json`, `report.json`,
`tables.txt`, `result.txt`, and guest/Rust/joined folded stacks and SVGs live
beside one another.

`profile-cell-diff` extends `scripts/profile_diff.py`'s existing sequential
commit checkout/build/capture path. Run it from a clean tracked tree. Both
revisions must have the worker comparison adapter and profiling platform. The
comparison runs both cells in the worker executable with the current
measurement adapter temporarily applied to each checkout. It leaves VM,
dialect and helper implementations at their measured revisions. This supports
baselines older than the cell-profile CLI. The overlay is restored before
switching revisions, including failures. It freezes
the source bytes before checking out either commit, restores the original
branch on failure, and writes per-row self/inclusive deltas and total count
deltas to `<file>.diff/cell-delta.txt`. Raw sample counts vary with workload
CPU time and are diagnostic. `diff.svg` compares the joined folded stacks.
Never edit or build concurrently in that fork during a commit comparison.

## Standard runtime capture

Capture a small standard scenario:

```sh
. ./env.sh
python3 scripts/profile_runtime.py --cpu-profile --profile quick \
  --scenario standard --runs 1 --warmups 0 --turns 1 \
  --out .kiln/profiles/standard.json \
  --build-report .kiln/profiles/build.json
```

The workload is run under `perf record -g --call-graph fp`, sampling user CPU
cycles at 999 Hz. `.kiln/profiles/standard.profiles/` contains `perf.data`,
`perf.script`, `cpu.folded`, `cpu.top.txt`, `record.stderr.txt` and `capture.json`.
The text report retains the top 30 self-overhead rows from
`perf report --stdio --no-children`. GNU's Rust demangler handles v0 symbols on
hosts whose perf build cannot demangle them. Folded CPU weights count samples;
perf's self-overhead percentages use its estimated event counts. Missing tools,
permission failures, workload failures and empty captures exit nonzero and leave
a failed capture receipt with the reason. A workload receipt alone does not
certify that sampling succeeded. Failed captures discard previous profile output.

`--no-build --build-report .kiln/profiles/build.json` reuses the materialized
binary. `--binary` also accepts an explicit executable: the caller must supply
one built with symbols and frame pointers. The profiling flags choose the
profiling platform even without `--release`. They also work for `--boundary` and
`scripts/profile_runtime_stack.py`. Matrix members have separate profile
directories named by scenario or stack size beside the aggregate receipt;
capture receipts name the workload receipt.

## Direct `perf record`

The opt-in `//tools/buck2:profiling` target platform compiles first-party and
third-party runtime code at opt-level 3 with line tables, retained symbols and
frame pointers. It uses the existing optimized compile budgets and all existing
pool properties; normal and optimized configurations retain their behavior.

```sh
kiln build //crates/lash-perf:lash-perf__bin \
  --target-platforms //tools/buck2:profiling -c kiln.rust_profile=optimized \
  --materializations final --build-report .kiln/profiles/cpu-build.json
B="$(python3 tools/buck2/outputs.py --report .kiln/profiles/cpu-build.json \
  --label //crates/lash-perf:lash-perf__bin --single)"
perf record -g --call-graph fp -o .kiln/profiles/perf.data -- "$B" \
  --runtime-perf-scenario standard --runtime-perf-runs 1 \
  --runtime-perf-warmups 0 --runtime-perf-turns 1 --runtime-perf-smoke
perf report --stdio -i .kiln/profiles/perf.data
```

Sampling requires host perf permissions and belongs on a quiet host. Building
and running a symbolized executable is functional proof; no CPU optimization
claim follows from a smoke receipt.

## Persistent-heap sites

`scripts/profile_paged_heap.py` profiles the heap-scaling harness's park and
resume, its released workers included, and attributes CPU to page faults,
sealing, spool I/O, publication SQL and retirement. The command is in the
[durable substrate README](../../../crates/lash-perf/src/bin/durable_substrate/README.md#attributing-a-park-and-resume-at-large-h).

## Off-CPU capture

```sh
(ulimit -v 8388608; python3 scripts/profile_runtime.py --off-cpu \
  --build-report .kiln/profiles/blocked-build.json --profile quick \
  --scenario standard --runs 1 --warmups 0 --turns 1 \
  --out .kiln/profiles/blocked.json)

# chat-short has one native lookup per turn. Turn 2 is warm within the same session.
(ulimit -v 8388608; python3 scripts/profile_runtime.py --off-cpu --no-build \
  --build-report .kiln/profiles/blocked-build.json --profile quick \
  --replay chat-short --turns 2 \
  --out .kiln/profiles/blocked-tool.json)
```

This records `sched:sched_switch` with stacks and per-task context-switch
records (`--switch-events`). Every scheduler event is requested with `-c 1`; the CPU event sets its own
`freq=999` term,
including when `--cpu-profile --off-cpu` samples both populations in one workload
invocation. It does not capture other host processes system-wide.

`perf.script` retains source positions from batched LLVM symbolization. The
reader translates perf's DSO file offsets through the executable's ELF LOAD
segments before looking up addresses. This matters for the PIE executable's
nonzero file-offset/address difference. Source resolution requires the exact
materialized executable and its line tables. Unknown kernel/libc frames remain
unknown. `capture.json` counts resolved and unmapped user frames.

The collector accepts both perf scheduler field spellings, including the
human-readable `name:pid [priority] state ==> ...` form. The clock is explicitly
`mono`, matching Lash's CLOCK_MONOTONIC turn marks. Perf rejects CLOCK_REALTIME
on this host. GNU addr2line reached about 7 GB RSS on the captured executable;
LLVM's batched resolver works within the 8 GiB diagnostic cap.

Reprocess a recorded capture after a reader or source-resolution fix without
running the workload again:

```sh
python3 scripts/profile_runtime.py --reprocess-off-cpu .kiln/profiles/blocked-tool.profiles
```

`off-cpu.folded` weights and `off-cpu.top.txt` rankings are blocked nanoseconds,
from a sleeping switch-out stack to the same TID's next switch-in. This includes
wakeup-to-run delay; runnable/preempted tasks are excluded. Capture-edge intervals
without a switch-in are omitted and counted in `capture.json`; lost records or
no complete intervals fail the capture. These are OS thread waits, not async task
idle times. Several threads' blocked times can overlap. `turn-windows.jsonl` records opt-in
CLOCK_MONOTONIC turn boundaries, and `turn-N.off-cpu.top.txt` clips complete thread
intervals to each boundary. For the two-turn chat-short SQLite command above, turn 2
is the warm one-tool window. These clipped sums still overlap across threads;
intervals with no captured resume remain omitted.

Scheduler tracepoints require readable tracefs and tracepoint perf permissions.
`perf_event_paranoid=1` alone is insufficient on some hosts. If access fails, the
command exits nonzero, retaining perf's reason and a privilege note in the capture
receipt. No sudo or host reconfiguration is attempted. On the ticket's host,
the 2026-10-26 audit initially found denied tracefs access. The host now permits unprivileged scheduler tracing. The capture still checks access at runtime.

## Differential

Compare two folded files with one command:

```sh
python3 scripts/profile_diff.py \
  --folded .kiln/profiles/before.profiles/cpu.folded \
           .kiln/profiles/after.profiles/cpu.folded \
  --normalize --svg --out .kiln/profiles/diff
```

The command runs `inferno-diff-folded`, retains `diff.folded`, and prints the top
30 complete stacks by absolute signed change (`after - before`) with both
counts. `diff.top.txt` retains that table; `--svg` writes `diff.svg` with Inferno.
Normalization scales the before sample total to after's total; omit it to compare
raw work counts. Do not compare CPU sample files with blocked-time files.

Compare two commits on this host with the same scenario and geometry:

```sh
python3 scripts/profile_diff.py --commits HEAD~1 HEAD \
  --scenario standard --runs 1 --turns 1 \
  --normalize --svg --out .kiln/profiles/commits
```

Use an isolated fork with a clean tracked tree and committed tooling. The command
resolves both commits, temporarily checks out each, runs two sequential Kiln
builds with `//tools/buck2:profiling`, materializes each executable, then samples
it locally. Both revisions must support that platform and the runtime CLI. It
restores the original branch or detached HEAD even on failure. Build reports,
workload and capture receipts and stacks are retained in numbered revision
directories; `comparison.json` records both SHAs and workload geometry. Never
run another Kiln command or edit the fork during this comparison.

Two independent captures of one commit establish a noise floor. Comparing a
file to itself is only an exact-zero sanity check. Match scenario, runs, turns,
compiler/platform and host, and retain this noise comparison with any differential.
Startup and all inherited workload threads/children are included; a smoke profile
is functional instrument proof, not a statistically established performance claim.

The scheduler decoder uses perf's [per-task context-switch
records](https://man7.org/linux/man-pages/man1/perf-script.1.html), whose IN/OUT
format is defined in [Linux perf's event printer](https://github.com/torvalds/linux/blob/v6.8/tools/perf/util/event.c).
