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
kiln gate lash <fork> -- python3 scripts/test_latency_load_record.py LoadRecordTests.test_a_quiet_fast_case_qualifies LoadRecordTests.test_load_at_the_core_count_during_the_fast_case_is_unqualified LoadRecordTests.test_a_fast_case_that_dies_is_unqualified_and_keeps_its_status
kiln gate lash <fork> -- python3 scripts/test_latency_slope.py SlopeTests.test_a_known_slope_is_recovered_exactly_without_noise SlopeTests.test_bin_medians_are_the_ordinal_ranges_medians
```

Expect one Rust test, three load laws and two slope laws: six passed tests.
Keep test logs and the Rust `kiln: test passed executed=1 passed=1` receipt.
An empty selection is Abort, and a red law remains a product finding.

## Phase 2 — Qualify the window before measuring

The qualifying release run belongs to the planned quiet window immediately
before FIG-5193's certification dress rehearsal, owned under FIG-3843:
stop dispatching new lanes, let active lanes finish, then wait for load1
below 8. Do not stop someone else's job or reconfigure the shared pool.
Record `/proc/loadavg`, `/proc/pressure/cpu`, core count, source SHA, dirty
status, command/configuration, store and artifact directory. Do not begin a
certifying measurement with load1 >=8. During the fast case load1 must remain
below the host's core count; the wrapper samples the case edges and intervals.
CPU PSI is recorded as diagnostic evidence, not an extra pass threshold.
The automated core-count predicate can qualify a run started above load1 8;
record both predicates and keep that run unqualified for the planned release
window.

A busy development lane can rehearse the report-reading procedure once with
`--cases fast --fast-samples 1050 --lanes 1`; it is an **unqualified rehearsal**,
not release certification. The audit's 1050-sample example is below the current
10,000-sample minimum and intentionally exits 2. Never reduce the release
population, lanes or budget to turn that rehearsal green. Do not wait for a
quiet window inside an implementation lane.

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

## Phase 4 — Read qualification before the verdict

1. Read `latency-load.json`: `verdict.qualified`, `reason`, `max_load1`,
   `cores`, CPU PSI and samples/interval coverage for the fast window.
   An unqualified host yields **UNQUALIFIED**, even if child output says PASS.
   A successful child on an unqualified host makes the wrapper exit 3;
   a failed child retains its status. Record both `exit_status` and
   `certification_exit_status`. Missing or incomplete fast coverage is Abort.
2. Read `latency-report.json`'s `verdict` and fast case: at least 10,000
   samples, actual 16 lanes, overhead **p50 <50 ms and p99 <250 ms**, with
   failures retained. A qualified budget violation is FAIL. Qualification
   alone never means PASS, and exit 0 alone is insufficient evidence.
3. Pin environment/binary identity and `baseline.kind`, `baseline.sha`,
   dirty status, fixture and payload. The current baseline subtracts the
   same-run provider duration; it is not a historical inline A/B baseline.
4. Compare request→acceptance, acceptance→admission, first actual delta,
   admission→terminal and terminal→host completion. Null delta remains
   absent. Inspect physical SQLite writes/send and writer busy ms/send;
   summed reader-thread work is not another elapsed latency phase.
5. Read the centred overall/per-lane slope and medians for ordinals 0–99
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
| Initial window | load1 <8; source/config/core count recorded | | |
| Fast qualification | completed sampled window, load1 below cores | | |
| Population/config | >=10,000 fast samples, actual 16 lanes | | |
| Budget | overhead p50 <50 ms, p99 <250 ms | | |
| Report integrity | ledgers, failures, causal clocks and baseline identity retained | | |
| Slope/bins | numerical estimates/CI and populated ordinal bins reviewed | | |
| Outcome | PASS / FAIL / UNQUALIFIED / Abort, matching receipts | | |
| Resources/cost | peak RSS within 8 GiB; no provider spend ($0) | | |

If two qualified quiet runs are borderline, FIG-3843's next step is a dedicated
gate machine; it is not permission to retry within this runbook invocation or
relax thresholds. [Recorded operator rehearsal](recorded-run.md).
