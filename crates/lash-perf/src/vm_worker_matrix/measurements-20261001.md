# FIG-4609 worker exchange measurement status, 2026-10-01

Implementation and laws were validated against main `8db745071bf31b164cc8d0bdd1541f5108e7e651`. The optimized
remeasurement was previously **PENDING** on the remote rustc memory cap; lash-bd's sizing2 commit `22b370ec6c` gave
optimized compiles their own measured requests (lash-perf lib 4.75 GiB, test binary 5.25 GiB). The optimized matrix has now run end to end at main `d9cd65af80b7ab38523b01203cabcdc530eda914`.

The run below is one full matrix execution — `--verify`, then `--out` with the fixed 10,000 warm observations and 200
cold starts per workload — taken on a **loaded shared host** (load average 7.5/21.6/33.4 on 32 cores while it ran).
It is diagnostic evidence that the optimized build compiles, the instrumented worker round-trips, and every
population reports; it is not the quiet-host acceptance distribution. FIG-4642 Part B owns the comparable
quiet-host numbers and FIG-4172 owns the final gates; both use the same single command documented in
[README.md](README.md). No compile OOMed, so no `kiln.memory_scale` retry was needed and no sizes were raised here.

All times are nearest-rank p50 / p99 in microseconds. Judged rows use the report-only 100 / 500 us per-leaf budget.
Scalar, resumed and single-leaf parallel rows have no codec baseline (n/a); value rows carry the paired same-process
production codec/socket baseline and signed subtraction, with negative samples retained. Phase sums reconcile per
sample within the 1 us tolerance: zero reconciliation failures across all 1,270,000 exchange samples.

| Population | Budget unit | Raw batch p50 / p99 | Baseline p50 / p99 | Judged p50 / p99 | Phases / reconciliation |
| --- | --- | --- | --- | --- | --- |
| scalar-1 | per leaf, N=1 | 72.076 / 140.014 | n/a | 72.076 / 140.014 | 12 phases recorded, 0 failures, 0 overlapping |
| scalar-10 | per leaf, N=1 | 65.002 / 176.512 | n/a | 65.002 / 176.512 | 12 phases recorded, 0 failures, 13 overlapping |
| scalar-100 | per leaf, N=1 | 68.358 / 292.952 | n/a | 68.358 / 292.952 | 12 phases recorded, 0 failures, 208 overlapping |
| parallel-1 | per leaf, N=1 | 106.551 / 207.651 | n/a | 106.551 / 207.651 (over report-only p50) | 12 phases recorded, 0 failures, 3 overlapping |
| parallel-10 | per leaf, N=10 | 346.181 / 2100.284 | n/a | 34.618 / 210.028 | 12 phases recorded, 0 failures, 220 overlapping |
| parallel-100 | per leaf, N=100 | 1266.936 / 3691.289 | n/a | 12.669 / 36.912 | 12 phases recorded, 0 failures, 549 overlapping |
| value-32 | per leaf, N=1 | 64.431 / 120.708 | 36.860 / 70.492 | 28.554 / 75.783 (149 negative) | 12 phases recorded, 0 failures, 0 overlapping |
| value-8192 | per leaf, N=1 | 86.603 / 175.079 | 51.578 / 120.056 | 35.576 / 111.551 (234 negative) | 12 phases recorded, 0 failures, 0 overlapping |
| value-1044480 | per leaf, N=1 | 3088.625 / 6692.910 | 1231.238 / 3365.967 | 1854.251 / 3966.017 (13 negative, over report-only) | 12 phases recorded, 0 failures, 29 overlapping |
| resumed-segments | per leaf, N=1 | 60.614 / 143.230 | n/a | 60.614 / 143.230 | 12 phases recorded, 0 failures, 0 overlapping |

Per-phase p50 / p99 in microseconds for the same loaded-host run:

| Population | Parent decode | Parent encode | IPC write | IPC read/wait | Worker decode | Worker encode | Guest | Echo host |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| scalar-1 | 12.473 / 26.362 | 1.564 / 2.865 | 2.885 / 6.482 | 28.914 / 72.798 | 3.486 / 6.824 | 16.040 / 30.968 | 5.040 / 11.271 | 0.751 / 1.292 |
| parallel-1 | 14.837 / 33.563 | 1.994 / 4.168 | 3.146 / 8.636 | 30.837 / 87.258 | 4.288 / 9.558 | 17.994 / 36.229 | 28.573 / 55.724 | 2.134 / 3.907 |
| scalar-10 | 9.238 / 23.464 | 0.942 / 2.484 | 2.425 / 7.384 | 28.345 / 98.977 | 1.913 / 4.999 | 13.916 / 34.224 | 5.952 / 15.920 | 0.370 / 1.152 |
| parallel-10 | 57.057 / 171.093 | 5.140 / 13.775 | 4.027 / 190.238 | 71.425 / 1575.296 | 10.509 / 36.700 | 47.980 / 480.574 | 117.251 / 353.565 | 8.836 / 25.818 |
| scalar-100 | 10.080 / 26.712 | 0.992 / 2.485 | 2.404 / 8.646 | 30.647 / 194.867 | 2.053 / 5.040 | 14.808 / 44.834 | 6.893 / 19.045 | 0.390 / 1.162 |
| parallel-100 | 253.306 / 588.941 | 34.284 / 72.436 | 5.410 / 960.458 | 56.294 / 1097.303 | 34.044 / 81.303 | 167.976 / 842.516 | 634.735 / 1376.762 | 59.512 / 149.823 |
| value-32 | 10.351 / 21.078 | 1.463 / 2.795 | 2.445 / 5.470 | 27.122 / 61.723 | 3.206 / 5.750 | 14.326 / 26.349 | 4.007 / 8.225 | 0.621 / 1.162 |
| value-8192 | 12.203 / 25.136 | 2.475 / 5.050 | 4.128 / 9.117 | 34.617 / 92.714 | 4.137 / 8.706 | 16.290 / 33.242 | 9.768 / 21.951 | 0.691 / 1.252 |
| value-1044480 | 153.341 / 436.291 | 168.548 / 562.048 | 237.497 / 674.089 | 190.970 / 774.308 | 567.467 / 1188.387 | 1098.377 / 2137.053 | 623.384 / 1292.023 | 1.262 / 5.060 |
| resumed-segments | 9.749 / 30.077 | 1.263 / 3.917 | 2.525 / 8.306 | 26.941 / 76.695 | 3.176 / 9.528 | 13.154 / 36.128 | 1.803 / 5.481 | 0.621 / 2.194 |

The warm zero-effect paired overhead measured 44.664 / 138.882 us p50 / p99, inside the report-only 1 ms / 5 ms
honesty budget. Cold starts measured 200 fresh worker processes per workload (p50 1.9–23.6 ms across cases);
concurrency widths 1/2/4 and the one-slot queue probe also ran (10,000 samples each). The full summary.json has all
warm/cold/checkout/reset/RSS populations.

Two report-only populations exceeded the 100 / 500 us per-leaf threshold on this loaded host: parallel-1
(106.551 us p50) and value-1044480 (1854.251 us/leaf p50 after baseline subtraction, dominated by worker encode and
guest work on the 1 MiB payload). Thresholds remain report-only per the FIG-4433 Q3 ruling; no benchmark threshold
changed to make observed results pass.

Evidence: fork-local `.benchmarks/vm-worker/` holds the preserved raw `samples.csv` (948 MB), `summary.json`,
`budgets.json` and the generated `report.md` from which these tables were taken; `.benchmarks/` is gitignored, so
this file carries the committed record. Build proof: `kiln build --config=optimized` of
`//crates/lash-perf:vm-worker-matrix__bin` succeeded (2063 actions, 46% cached, 1113 remote, no OOM), and
`kiln test --config=optimized //crates/lash-perf:lash-perf__unit_test` passed 139 cases including
`measured_phases_cover_every_exchange_and_reconcile`, building the optimized 5.25 GiB test binary.

Earlier correctness evidence is unchanged: the three laws failed on the main baseline, then passed in 20 real
uncached executions each (60 cases, 2,540 live exchange samples and 60 paired production codec/socket baselines
across all three value sizes); the affected suites executed 86 passing cases with one ignored quiet-host start
measurement. Ordinary transport and worker calls use const-generic false specializations that compile out the
measurement clocks and telemetry.
