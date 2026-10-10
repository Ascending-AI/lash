# CPU and OS blocked-time profiles

Source `env.sh` in an isolated Kiln fork. Install `perf`, GNU `c++filt` with Rust
support, and Inferno's `inferno-collapse-perf`, `inferno-diff-folded` and
`inferno-flamegraph` on PATH. These are local diagnostic tools; builds use Kiln's
existing profiling platform, optimization, line tables and frame pointers.

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

## Off-CPU capture

```sh
python3 scripts/profile_runtime.py --off-cpu --no-build \
  --build-report .kiln/profiles/build.json --profile quick \
  --scenario standard --runs 1 --warmups 0 --turns 1 \
  --out .kiln/profiles/blocked.json
```

This records `sched:sched_switch` with stacks and per-task context-switch
records (`--switch-events`). Every scheduler event is requested with `-c 1`; the CPU event sets its own
`freq=999` term,
including when `--cpu-profile --off-cpu` samples both populations in one workload
invocation. It does not capture other host processes system-wide.

`off-cpu.folded` weights and `off-cpu.top.txt` rankings are blocked nanoseconds,
from a sleeping switch-out stack to the same TID's next switch-in. This includes
wakeup-to-run delay; runnable/preempted tasks are excluded. Capture-edge intervals
without a switch-in are omitted and counted in `capture.json`; lost records or
no complete intervals fail the capture. These are OS thread waits, not async task
idle times. Several threads' blocked times can overlap.

Scheduler tracepoints require readable tracefs and tracepoint perf permissions.
`perf_event_paranoid=1` alone is insufficient on some hosts. If access fails, the
command exits nonzero, retaining perf's reason and a privilege note in the capture
receipt. No sudo or host reconfiguration is attempted. On the ticket's host,
tracefs access was denied; CPU sampling succeeded independently.

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
