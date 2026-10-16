# Shared pool measurements, 2026-10-16

FIG-6468 measured Lash's pool use before changing admission requests. The
24-hour window was 2026-10-15 10:24:30 through 2026-10-16 10:24:30 UTC.
Four remote workers supplied 322,963 first-party compile, Clippy and test
records. They reserved 4,352,817 CPU-seconds and used 3,693,658 CPU-seconds;
requested memory was 5,341,764 GiB-seconds against 2,803,535 GiB-seconds of
duration-weighted cgroup peaks. Peaks are not mean memory consumption, and
the difference includes necessary headroom. It is not a reclaimable budget.
Third-party actions and anonymous helpers cannot be attributed to Lash from
worker usage alone; the retained Buck2 events account for them separately.

## Changes supported by the measurements

| Request | Before | After | Evidence | Reservation saved in the observed executions |
| --- | --- | --- | --- | ---: |
| 18 previously unmeasured test identities | 512 MiB | 256 MiB | At least 20 successful samples each; maximum anonymous peak below 80% of the new request, including unsuccessful runs | 38,126 GiB-seconds |
| Core-worker library metadata | 2 GiB | 768 MiB | 20 joined metadata/check/diagnostic actions, anonymous p99 168.4 MiB | 211 GiB-seconds |
| OpenAI-provider library metadata | 1.75 GiB | 1 GiB | 20 joined metadata/check/diagnostic actions, anonymous p99 251.4 MiB | 60 GiB-seconds |
| Conformance Clippy | 3.25 GiB | 1.5 GiB | 360 actions, anonymous p99 725.4 MiB, maximum total peak 848.9 MiB | 12,254 GiB-seconds |

The replay estimate is 50,650 GiB-seconds, about 0.95% of the measured
first-party memory reservation. It assumes the same executions and durations;
it is not a measured throughput improvement. Resizing also changes action
keys and incurs an initial cache miss. CPU requests, optimized compilation,
large-suite exceptions, contention and service floors, unmeasured defaults,
batch policy and all five required execution properties retain their values.
The worker's independent minimum cgroup memory limit still applies.

Requests use the existing sizing formulas: compile anonymous p99 times 1.25
plus 512 MiB, rounded to 256 MiB; tests use anonymous p99 times 1.25, rounded
to 256 MiB. Only the measured identities above are refreshed. Updating all
rows from a shorter observation window would discard useful evidence about
large suites and filtered selections.

## The larger opportunities are scheduling and attribution

A watcher run selected 811 targets and produced 801 test results. Two archived
runs used 52,629 and 59,026 estimated reserved CPU-seconds for uncached remote
tests. The test262 and TypeScript unit shards consumed 29.8% and 27.7% of
those reservations. Their timeouts alone consumed 5.7% and 9.2%.
The timeouts were recorded by the watchdog inside the worker after about
300 seconds. Admission queue time is outside that clock. Across the full
24-hour worker sample, 219 test262 timeouts used 65,846 action-seconds and
67,252 CPU-seconds, with a median of 1.001 cores against a two-core request.
No OOM kill was recorded. These observations do not establish whether the
execution tail comes from host contention or a particular test input.

Retained events contained 60 extra responses for already running actions,
saving about 5,413 action-seconds through in-flight coalescing. They are not
re-executions. Only one identical key had two distinct execution starts: a
failing Clippy action, costing 5.2 additional seconds. No successful identical
key re-execution or proven fork-path cache drift was found in that sample.
The last 12 logs per daemon do not support a complete daily consumer census.

The orchestrator's process changes are drafted in the FIG-6468 report:
coalesce watcher heads on a three-hour cadence; retain per-invocation action
and test summaries; capture per-case timing and cgroup throttle counters for
the TS tail; sweep only idle daemons; and avoid a second Clippy invocation
only when the final checked tree and configuration are identical. A rebased
or combined landing tree still needs its own Clippy. None of those process
changes is implemented by this repository patch.

## Reproduce the measurements

The full inventory, top 20 reservations, per-consumer cache rates, proposal
drafts and proof receipts are in the lane report and its archived
`.kiln/FIG-6468/` evidence. The evidence retains compressed usage from each
worker, 198 original event logs, compact extracted observations and the
measurement scripts. The scripts ran with an 8 GiB virtual-memory cap;
`/usr/bin/time -v` records peak RSS.

Use `tools/buck2/action_sizes_from_log.py` for the sizing formulas and
`tools/buck2/action_categories_from_events.py` for the conservative event/usage
join. Source `env.sh` before running generation checks. Regenerate
`exec_sizes.bzl` with `kiln sync`; do not edit its requests directly.
