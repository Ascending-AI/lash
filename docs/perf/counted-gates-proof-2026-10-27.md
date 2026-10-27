# Counted regression gate proof, 2026-10-27

FIG-7434. All patches were applied and reverted only in the r33-gates Kiln fork.
No shared-host wall duration decides a verdict. The CPU event is qualified
hardware instructions:u; missing or less than 99% running time refuses capture.

| Probe | Injected failure | Reverted full gate |
|---|---|---|
| (a) one SQL UPDATE/turn | chat-short writes 92 > 91; compacting 102 > 101 | PASS, five independent processes |
| (b) 1KiB allocation/guest read | TS worker calls 29,203 > 29,189; Python 12,924 > 12,905; bytes also fail | PASS, five independent processes |
| (c) multiply/register dispatch | 21 of 40 CPU rows exceed 1%; Python float arithmetic about +5.96% | PASS, all 40 rows |
| (d) 50ms worker-entry sleep | median requested sleep 50,000,000 ns > 0; calls 1 > 0 over 20 Ready handshakes | PASS, both counted rows |
| (f) retained 2 MiB/facade handle | chat-short live 4,482,579 B > 2,391,524 B; all four workloads fail | PASS, five independent processes |

Probe (e) is unproven and stale: its patch targets SurfaceTools::execute, which
ordinary facade replay dispatch no longer runs. The owner explicitly waived
that proof; no workload or moved injection was added.

The completed counted pairs are pinned to source `56825be218`. The CPU pair uses
the final optimized artifact captured at `c5e4cdfda8`; its source patch is the
original `c/injection.patch`. Startup reuses that unchanged harness and separately
builds only the actual helper main executable with the original `d/injection.patch`.
A superseded combined startup build was cancelled because it unnecessarily
rebuilt the harness; it is not a proof result.

The final committed CPU reference contains a fresh complete 40-row population
from source `39550dd238a6`, with 20 replay counted metrics and the new main
build identity. `ship-optimized-build.json` pins the actual optimized artifacts;
`ship-cpu-calibration/` retains the raw qualified captures. The mutation pairs
above remain attributed to their original source and are not relabeled as
measurements of the landing revision. The final startup calibration repeats
five populations of 20 Ready handshakes on those same optimized binaries;
all requested sleep counts and nanoseconds remain exactly zero.

The landing count baseline uses five complete captures from the actual hermetic
executor with eight runtime workers and periodic node maintenance outside the
foreground window. Each ceiling is maximum plus max-minus-min range. All 60
original main median ceilings tighten; original hard caps and depth-slope
references still bind. After rebasing main through FIG-7446, foreground write maxima are 87, 97, 132
and 132, each with zero observed range. All 96 median ceilings use the five
complete receipts in `turnsql-main-raw/`. Source checkpoint `c871a0019457` and
the actual binary hash are retained in `ship-counts-source.json`. Original
hermetic identities remain unchanged, including their unavailable Git commit.
`ship-baseline.diff` contains the final baseline diff.

The earlier local calibration failed the hermetic replay target, exposing
resource-geometry differences and maintenance writes proportional to executor
scheduling. Those failures and a 300-second watchdog timeout remain retained.
The gate now fixes eight runtime workers, retains raw executor receipts, defers
periodic private-node maintenance, and has a declared 900-second watchdog.
That watchdog is a liveness bound, never a performance verdict. SQL reads still
lack complete foreground attribution and do not certify one additional read.

Validation: all 15 edited laws executed and passed; the existing perfreport
refusal law passed; schema checks passed; the full replay target passed 9/9.
`scripts/land` runs the required workspace Clippy before pushing.

Evidence directory:
`/workspace/notes/lash/tasks/lanes/r33/gates.evidence/`

- `paired-count-probe-{a,b,f}/proof.json` and sibling raw receipts/controls.
- `final-probe-c/capture/decision.json`, `final-probe-c-reverted/capture/decision.json`.
- `paired-startup-probe-d/capture/decision.json` and reverted sibling.
- `turnsql-main-raw/replay-receipt.0.json` through `.4.json`, `ship-counts-baseline.json`.
- `ship-replay-tests.json`: final rebased full target, 9/9 passed.
- Earlier failed controls, partial captures and superseded calibrations remain
  retained and are not passing proof pairs.

Scheduled main commands, from its sourced Kiln fork:

```sh
just perf-gate-instructions
just perf-gate-startup
just perf-history-record
```

The history recipe requires the clean current main head, appends counted values
with full producer identity to the existing runtime JSONL store, and runs its
existing change detector. It records no wall/latency fields. Override
LASH_PERF_HISTORY_FILE to retain that same store outside a disposable fork.
