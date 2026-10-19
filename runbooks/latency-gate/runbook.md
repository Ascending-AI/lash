# Run and read the latency release gate

Read [the shared rules](../RULES.md) and the
[instrument reference](../../docs/perf/instruments/send-to-completion-latency.md).
This `deterministic_only` operator procedure emits no browser row, calls no
provider model and costs $0. Its fixtures submit through `send()`; Lash's
engine drives the turns. The gate protects FIG-3843's release budget.

## Phase 1 — Companion laws

Run each command under `(ulimit -v 8388608; ...)`, once through Kiln by
full selector, and require nonzero executed counts:

```sh
kiln test //crates/lash-perf:latency__test --test_arg=--exact --test_arg=latency_receipts_use_engine_clocks_and_include_remote_work
kiln gate lash <fork> -- python3 scripts/test_latency_load_record.py LoadRecordTests.test_load_never_changes_the_exit_status LoadRecordTests.test_the_command_writes_the_record_and_prints_each_cases_load LoadRecordTests.test_subsecond_cases_sample_both_edges_without_inventing_interval_coverage
kiln gate lash <fork> -- python3 scripts/test_latency_slope.py SlopeTests.test_a_known_slope_is_recovered_exactly_without_noise SlopeTests.test_bin_medians_are_the_ordinal_ranges_medians
```

Expect one Rust test, three load laws and two slope laws: six passed tests.
Keep test logs and the Rust `kiln: test passed executed=1 passed=1` receipt.
An empty selection is Abort, and a red law remains a product finding.

## Phase 2 — Record the host before measuring

Load never qualifies or disqualifies a run (Sam, 2026-10-19: "remove that
gate on performance checks - we should only think about doing on quiet if it
shows that the machine is the issue"). Measure when the run is due; do not
wait for a quiet host, and do not rerun because of load. Do not stop someone
else's job or reconfigure the shared pool.

Record `/proc/loadavg`, `/proc/pressure/cpu`, core count, `kiln cgroup
status`, source SHA, dirty status, command/configuration, store and artifact
directory before and after. The wrapper samples load and CPU pressure through
the run, at both edges of every case, and summarizes each case's window.

A development lane can rehearse the report-reading procedure once with
`--cases fast --fast-samples 1050 --lanes 1`. The 1050-sample example is below
the 10,000-sample minimum and intentionally exits 2. Never reduce the release
population, lanes or budget to turn that rehearsal green.

## Phase 3 — Run once with the full release configuration

Source `env.sh`. Use a fresh owned artifact root and stores outside the
evidence archive. The recipe builds the optimized binary through Kiln and
writes its build report; never choose a stale executable by hand.

```sh
export LASH_LATENCY_ARTIFACT_DIR="$PWD/target/latency-operator-release"
(ulimit -v 8388608; /usr/bin/time -v \
  kiln gate lash <fork> -- just latency-gate)
```

Save the actual exit code and maximum RSS. The default gate runs all cases,
including the same-process, deterministic, no-tool `fast` case with 10,000
samples at 16 lanes. Other topologies/cases are reported, not independently
gated by the 50/250 ms budget. Preserve failures in the sample ledger.

Copy only logs, receipts and reports into
`.kiln/<ticket>/run-root/latency-gate/`; keep binaries and stores in build or
target directories. Retain `build-report.json`, `latency-gate.log`,
`latency-report.json`, `latency-samples.json`, `latency-load.json` and
`latency-slope.txt` plus the command, source identity and RSS record.

## Phase 4 — Read the verdict, then the host evidence

1. Read `latency-load.json`: `exit_status` (the wrapper's own) and, for each
   case, `cases.<case>.load` (samples, interval samples, max and mean load1,
   max CPU `some` avg10). A case with no finished window did not complete.
2. Read `latency-report.json`'s `verdict` and fast case: at least 10,000
   samples, actual 16 lanes, overhead **p50 <50 ms and p99 <250 ms**, with
   failures retained. A budget violation is FAIL whatever the load; exit 0
   alone is insufficient evidence.
3. Judge from the evidence whether the host explains the result: phases
   dominated by waiting rather than work, CPU pressure, wall time far above
   the write and busy counts, or variance tracking load. Recommend a quiet
   rerun only when it does, with those numbers.
4. Pin environment/binary identity and `baseline.kind`, `baseline.sha`,
   dirty status, fixture and payload. The current baseline subtracts the
   same-run provider duration; it is not a historical inline A/B baseline.
5. Compare request→acceptance, acceptance→admission, first actual delta,
   admission→terminal and terminal→host completion. Null delta remains
   absent. Inspect physical SQLite writes/send and writer busy ms/send;
   summed reader-thread work is not another elapsed latency phase.
6. Read the centred overall/per-lane slope and medians for ordinals 0–99
   and 500–599. An interval wholly above zero signals growth with settled
   history (FIG-4033). Empty bins and undetermined slopes are missing evidence.
   `latency_slope.py` reports numbers and exits 0 when samples exist: it
   encodes **no numerical pass threshold** for the medians and does not
   certify the release. Record regression findings without inventing a gate.

The current follower has no historical five-second live-report grace. The
`grace` CLI case name survives as a remote-terminal-wait diagnostic; do not
interpret it as proof of a removed engine's grace behaviour.

## Scorecard

| Gate | Expected | Observed | Verdict/evidence |
| --- | --- | --- | --- |
| Companions | six executed, six passed | | |
| Host record | load, PSI, cgroup status, source/config/core count recorded before, during and after | | |
| Fast window | completed sampled window | | |
| Population/config | >=10,000 fast samples, actual 16 lanes | | |
| Budget | overhead p50 <50 ms, p99 <250 ms | | |
| Report integrity | ledgers, failures, causal clocks and baseline identity retained | | |
| Slope/bins | numerical estimates/CI and populated ordinal bins reviewed | | |
| Outcome | PASS / FAIL / Abort, matching receipts; whether the host explains it | | |
| Resources/cost | peak RSS within 8 GiB; no provider spend ($0) | | |

If two runs are borderline and the host evidence does not explain them, FIG-3843's next step is a dedicated
gate machine; it is not permission to retry within this runbook invocation or
relax thresholds. [Recorded operator rehearsal](recorded-run.md).
