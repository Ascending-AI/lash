# FIG-4609 worker exchange measurement status, 2026-10-01

Implementation and laws were validated against main `8db745071bf31b164cc8d0bdd1541f5108e7e651`. The optimized
remeasurement is **PENDING**: both optimized build attempts stopped in
`//crates/lash-restate-test:lash-restate-test`, with remote compiler exit 9 and
empty stdout/stderr. The action digest was
`6d245080718aa0d81f49cc499c395b23782c9246cd92f37081ddc7d5f8962f44:142`.
The matrix executable was never produced, so no latency numbers are reported.
The pool owner confirmed that optimized rustc exceeded its 2.75 GiB memory
cap: compile sizes were derived from development-profile peaks. A separate
`buck2-sizing2` lane owns the sizing fix and the orchestrator will rerun this
matrix afterward. No execution properties, budgets or compiler flags were
changed here to bypass the failure. The dependency is used by the existing
runtime performance harness and was retained.

| Population | Budget unit | Raw batch p50 / p99 | Baseline p50 / p99 | Judged p50 / p99 | Phases / reconciliation |
| --- | --- | --- | --- | --- | --- |
| scalar-1 | per leaf, N=1 | PENDING | n/a | PENDING | PENDING |
| scalar-10 | per leaf, N=1 | PENDING | n/a | PENDING | PENDING |
| scalar-100 | per leaf, N=1 | PENDING | n/a | PENDING | PENDING |
| parallel-1 | per leaf, N=1 | PENDING | n/a | PENDING | PENDING |
| parallel-10 | per leaf, N=10 | PENDING | n/a | PENDING | PENDING |
| parallel-100 | per leaf, N=100 | PENDING | n/a | PENDING | PENDING |
| value-32 | per leaf, N=1 | PENDING | PENDING | PENDING | PENDING |
| value-8192 | per leaf, N=1 | PENDING | PENDING | PENDING | PENDING |
| value-1044480 | per leaf, N=1 | PENDING | PENDING | PENDING | PENDING |
| resumed-segments | per leaf, N=1 | PENDING | n/a | PENDING | PENDING |

The three laws failed on the main baseline, then passed in 20 real uncached
executions each: 60 cases, including 2,540 live exchange samples and 60 paired
production codec/socket baselines across all three value sizes. Every run's XML,
log and remote execution report was inspected. The affected suites executed 86
passing cases, with one ignored quiet-host start measurement. Workspace Clippy
and formatting passed. These correctness results are not optimized performance
measurements.

The implemented budget remains report-only at 100 / 500 us per leaf. Value
rows judge signed, paired exchange-minus-baseline samples and count negatives.
Phase rows include the seven required phases plus fixed echo host work, raw
blocking read/wait and explicit overlap; each sample's phase sum minus overlap
reconciles within 1 us. Ordinary transport and worker calls use const-generic
false specializations that compile out the measurement clocks and telemetry.

To complete this table after the build issue is resolved, run the optimized
build and full matrix commands in [README.md](README.md), preserve the raw CSV,
and replace the pending rows with the generated `report.md`, `budgets.json`
and `summary.json`. The full run requires 10,000 warm observations and 200 cold
starts per workload. FIG-4172 owns quiet-host final measurements. No benchmark
threshold may change to make observed results pass.
