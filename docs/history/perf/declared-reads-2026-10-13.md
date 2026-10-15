# Declared reads through the facade, 2026-10-13–14

> Historical measurement. FIG-6299 removed the `round.traced` commit and its
> decision facts, so a traced run now commits what an untraced one does; the
> numbers below are as measured before that change.

FIG-6154. Fifty dependent reads in TypeScript and Python were measured on SQLite file FULL
and a private PostgreSQL 18 with fsync, synchronous commit and full-page writes
on. Both stores confirm the cost model: **99 fewer owner commits untraced,
149 fewer traced**, after paying one enclosing Read-plan commit. The 150
admission/start/outcome facts disappear; an additional cell-report fact also
disappears, and tracing removes 50 decision facts. Timing has no gate or
predeclared target.

Both dialects are complete. The first mixed-dialect law exposed the production
helper-release pin refusal fixed by FIG-6186: a Python cell could not save in a
store where TypeScript had pinned its 1.0 helper manifest. The required single
final rebase brought in that fix, so Python was enabled and measured; no skip
remains. The projection fixture also uses each dialect's own number policy and
saved-function calling convention. The production fix belongs to FIG-6186.

## Method and scope

The [maintained harness](../../../crates/lash-perf/src/bin/durable_substrate/README.md#fifty-dependent-declared-reads-fig-6154)
uses the production `LashSession::send()` path, registry, RLM protocol, engine,
stores and worker transport. The model answers locally with one code cell and
then prose. Every local provider answer has an ordinal prefix and a fixed body
of 32 or 16,384 ASCII bytes; the next request carries the full preceding answer.
The recorded `Repeatable` and `Read` variants use identical source, schemas and
provider code. A projection handle control uses the same answer function via a
host-seeded saved function. The mixed case makes **two reads and one Once write**;
the write carries the first answer. The fixtures refuse extra/reordered calls
and the cell checks its final value; this is a cost instrument, not a retry or
crash-certification fixture.

Twenty fresh sessions per population, 32 populations per store:
**640 completed samples per store, 1,280 total** (320 per dialect/store leg). Workers are prewarmed and shared;
creation, projection seeding and completed deletion are outside the window.
Samples share the case's database, so immutable source/helper/value nodes may
deduplicate. Populations run sequentially in the command-manifest order; they
are not interleaved or randomized. End-to-end time starts before `send()` and
ends when its outcome returns, including facade observation. Percentiles use
nearest rank; with 20 samples p95 is rank 19 and p99 is the maximum.

Owner counts include successful transactions of the measured session up to its
`turn.commit`. Mailbox writes, node maintenance and later session release are
excluded. Facts count successful appends before terminal pruning. Persisted
bytes mean **actual newly inserted canonical graph payload bytes**, not staged
bytes or an estimate of physical writes; SQL metadata, indexes and WAL are not
included. Full transaction receipts and staged bytes remain in the raw ledger.
IPC frames and bytes come from the worker pool's counters around the same
window. Traced populations install the production OpenTelemetry adapter with
omitted content and a local in-memory span exporter.

The Read-plan column times its owning `model.done+cell.snapshot+admit`
transaction separately. That receipt includes the fused model response and
initial snapshot; it is not an isolated plan-building CPU measurement. The
recorded variant fuses its first admission/start into that boundary instead.
No helper/library definition or production read implementation was changed.
The projection worker adds only a pure fixture handle factory. The top-level
`vm_worker` run metadata describes the substrate cases; declared-read workers
re-execute the benchmark itself, identified by its SHA below. Store options
use the typed `PostgresHostConfig::default()` for this single-node matrix.

## Exact structure

Each dialect/store leg has **11,680 accepted owner commits and 14,440 appended
facts** across 320 samples. Each store therefore has **23,360 commits and
28,880 facts** across both dialects; all four legs total **46,720 commits and
57,760 facts**. Maps are identical across dialects, stores and samples in each population. The maintained `READ-COST-MODEL` assertion pins the entire label and
fact maps, including the enclosing commits; it does not learn a baseline from
the first sample. Every scalar in the raw ledger is an exact observation.

| Variant | Owner commits untraced / traced | Admission / start / outcome facts | Decision facts untraced / traced | Cell-report facts |
|---|---:|---:|---:|---:|
| 50 recorded Repeatable | 104 / 154 | 50 / 50 / 50 | 0 / 50 | 1 |
| 50 Read | 5 / 5 | 0 / 0 / 0 | 0 / 0 | 0 |
| 50 projection, 32-byte body | 4 / 4 | 0 / 0 / 0 | 0 / 0 | 0 |
| 50 projection, 16,384-byte body | 5 / 5 | 0 / 0 / 0 | 0 / 0 | 0 |
| Read → Once write → Read | 7 / 8 | 1 / 1 / 1 | 0 / 1 | 1 |

The model's **100 per-call transactions and 150 per-call phase facts** are
removed. Recorded's first admission shared an enclosing boundary, whereas
Read uses that boundary for its plan: this leaves **99 net commits saved**.
Tracing removes another 50 commits and 50 `decide` facts. The extra removed
`cell_report` is a per-cell reference fact, not a per-read phase. A cell whose
ledger has no recorded calls needs none.

The large projection control outlasts its first compute slice and pays one
standalone `model.done`, as [ADR 0142](../../adr/0142-a-cells-first-save-carries-its-turns-model-response.md)
requires. Read already committed that response with its initial plan. This
explains the control's extra enclosing transaction; it changes no per-read
count. Read's initial save adds fixed IPC traffic, and the large Read/control
cases require additional compute-slice and progress exchanges. Their fixed
whole-window frame totals differ by dialect, as shown below. The **five frames per read
remain** on every route; no elimination of per-read IPC is claimed.

## Measured timing, bytes and frames

Host: `turbo2-sam`, AMD Ryzen 9 5950X, 32 logical CPUs, Linux
6.8.0-137-generic. Kiln default developer profile, `debug_assertions=true`,
`stats_alloc` instrumentation. TypeScript was measured on source base
`4b1b3a0d684e448108de3ac8f13d0a153ffe0ef1` (FIG-6153 landed); Python on
`195ed4db937e52f9e24fcd3672d0d311e53df16a` after FIG-6186. That fix separates
helper-release pin keys by dialect; the Read implementation and manifests did
not change. Compare routes within a dialect/store leg on its same build;
these sequential runs do not certify a cross-dialect latency comparison.

| Dialect | Measured benchmark SHA-256 |
|---|---|
| TypeScript | `4bd035a6acf225909f82b14d4ad26b93ea1aee315ed6b2edfce26d940a249784` |
| Python | `a7c51fda9d6d29082ada5e9b4f2ddc5ba4406475e5c3b8940e57fb9abe36cceb` |

The final source tightens the TypeScript count oracle and uses the corrected
Python projection fixture; the corresponding measured runtime paths are retained.

**Shared-host timing is unqualified diagnostic data.** No pool-box measurement,
isolation, fast-gate certificate, traced-versus-untraced comparison or general
speedup claim is made. Exact structural counts are the primary endpoint. Every
foreground run had an 8 GiB address-space cap and exited 0; `/usr/bin/time -v`
retains peak RSS.

| Leg | Host load1 min–max | Peak RSS KiB | Matrix wall time |
|---|---:|---:|---:|
| TypeScript / SQLite | 10.63–58.95 | 304,104 | 9:06.08 |
| TypeScript / PostgreSQL | 10.30–76.72 | 307,968 | 11:02.94 |
| Python / SQLite | 28.86–79.00 | 296,864 | 11:08.31 |
| Python / PostgreSQL | 41.42–140.91 | 307,712 | 14:57.59 |

The tables below are emitted by the maintained `reads_report.py` from all raw
samples, including separate fused-plan receipt distributions.

| Store | Dialect | Body bytes | Traced | Variant | Samples | p50 / p95 / p99 ms | Plan commit p50 / p95 / p99 ms |
|---|---|---:|---|---|---:|---:|---:|
| postgres | python | 32 | False | projection | 20 | 796.023 / 1345.343 / 1355.941 | — |
| postgres | python | 32 | False | read | 20 | 713.970 / 1448.754 / 1750.262 | 42.753 / 101.508 / 121.849 |
| postgres | python | 32 | False | read-write-read | 20 | 780.372 / 1194.414 / 1535.606 | 43.454 / 99.979 / 113.977 |
| postgres | python | 32 | False | repeatable | 20 | 6131.542 / 8475.580 / 8949.016 | — |
| postgres | python | 32 | True | projection | 20 | 348.128 / 472.452 / 535.478 | — |
| postgres | python | 32 | True | read | 20 | 344.817 / 559.633 / 643.675 | 22.670 / 46.536 / 76.554 |
| postgres | python | 32 | True | read-write-read | 20 | 432.367 / 677.450 / 711.040 | 28.671 / 54.687 / 65.248 |
| postgres | python | 32 | True | repeatable | 20 | 6088.978 / 7719.233 / 7938.642 | — |
| postgres | python | 16384 | False | projection | 20 | 818.825 / 1031.862 / 1247.014 | — |
| postgres | python | 16384 | False | read | 20 | 861.380 / 1040.869 / 1085.106 | 36.957 / 68.388 / 83.825 |
| postgres | python | 16384 | False | read-write-read | 20 | 737.763 / 1080.101 / 1081.450 | 33.402 / 96.428 / 115.812 |
| postgres | python | 16384 | False | repeatable | 20 | 7529.426 / 10728.444 / 10868.039 | — |
| postgres | python | 16384 | True | projection | 20 | 428.185 / 552.474 / 571.797 | — |
| postgres | python | 16384 | True | read | 20 | 512.374 / 786.376 / 917.816 | 29.990 / 45.656 / 55.056 |
| postgres | python | 16384 | True | read-write-read | 20 | 417.480 / 533.021 / 686.280 | 20.505 / 34.072 / 45.297 |
| postgres | python | 16384 | True | repeatable | 20 | 6508.336 / 7002.409 / 7159.699 | — |
| postgres | typescript | 32 | False | projection | 20 | 275.674 / 334.532 / 475.105 | — |
| postgres | typescript | 32 | False | read | 20 | 203.726 / 249.055 / 304.948 | 14.664 / 19.694 / 25.519 |
| postgres | typescript | 32 | False | read-write-read | 20 | 221.253 / 297.936 / 334.193 | 11.254 / 23.140 / 37.548 |
| postgres | typescript | 32 | False | repeatable | 20 | 2296.357 / 2963.883 / 3194.305 | — |
| postgres | typescript | 32 | True | projection | 20 | 680.620 / 857.307 / 873.340 | — |
| postgres | typescript | 32 | True | read | 20 | 501.120 / 657.169 / 772.402 | 29.256 / 41.232 / 53.560 |
| postgres | typescript | 32 | True | read-write-read | 20 | 637.592 / 942.457 / 1010.312 | 28.416 / 63.841 / 73.952 |
| postgres | typescript | 32 | True | repeatable | 20 | 4727.291 / 5744.081 / 5762.408 | — |
| postgres | typescript | 16384 | False | projection | 20 | 722.787 / 938.105 / 1029.295 | — |
| postgres | typescript | 16384 | False | read | 20 | 563.593 / 704.048 / 758.955 | 23.996 / 35.416 / 36.402 |
| postgres | typescript | 16384 | False | read-write-read | 20 | 571.102 / 701.432 / 812.159 | 24.868 / 41.290 / 53.372 |
| postgres | typescript | 16384 | False | repeatable | 20 | 2714.093 / 4659.359 / 4796.920 | — |
| postgres | typescript | 16384 | True | projection | 20 | 804.850 / 1107.530 / 1162.237 | — |
| postgres | typescript | 16384 | True | read | 20 | 677.798 / 987.871 / 1014.984 | 29.316 / 73.852 / 77.420 |
| postgres | typescript | 16384 | True | read-write-read | 20 | 770.416 / 1062.062 / 1100.330 | 31.654 / 61.017 / 66.408 |
| postgres | typescript | 16384 | True | repeatable | 20 | 7351.959 / 8764.333 / 9515.828 | — |
| sqlite-file | python | 32 | False | projection | 20 | 304.714 / 352.065 / 355.671 | — |
| sqlite-file | python | 32 | False | read | 20 | 351.303 / 440.922 / 599.496 | 26.868 / 39.495 / 46.009 |
| sqlite-file | python | 32 | False | read-write-read | 20 | 440.486 / 518.706 / 526.963 | 26.750 / 42.609 / 44.615 |
| sqlite-file | python | 32 | False | repeatable | 20 | 4947.820 / 5648.663 / 5976.522 | — |
| sqlite-file | python | 32 | True | projection | 20 | 270.930 / 336.014 / 436.596 | — |
| sqlite-file | python | 32 | True | read | 20 | 227.012 / 295.275 / 297.784 | 18.842 / 24.880 / 27.774 |
| sqlite-file | python | 32 | True | read-write-read | 20 | 375.008 / 435.635 / 450.183 | 24.189 / 31.575 / 37.122 |
| sqlite-file | python | 32 | True | repeatable | 20 | 3932.225 / 5091.870 / 5426.243 | — |
| sqlite-file | python | 16384 | False | projection | 20 | 359.219 / 455.566 / 459.546 | — |
| sqlite-file | python | 16384 | False | read | 20 | 381.513 / 490.753 / 623.808 | 22.065 / 32.241 / 35.929 |
| sqlite-file | python | 16384 | False | read-write-read | 20 | 292.744 / 359.496 / 363.971 | 21.885 / 25.669 / 27.095 |
| sqlite-file | python | 16384 | False | repeatable | 20 | 5993.387 / 6397.453 / 6502.753 | — |
| sqlite-file | python | 16384 | True | projection | 20 | 419.267 / 527.616 / 544.632 | — |
| sqlite-file | python | 16384 | True | read | 20 | 498.557 / 604.728 / 628.813 | 27.199 / 41.209 / 55.600 |
| sqlite-file | python | 16384 | True | read-write-read | 20 | 475.514 / 594.129 / 646.464 | 29.517 / 56.219 / 61.503 |
| sqlite-file | python | 16384 | True | repeatable | 20 | 7344.718 / 8387.170 / 8451.841 | — |
| sqlite-file | typescript | 32 | False | projection | 20 | 279.802 / 673.865 / 690.914 | — |
| sqlite-file | typescript | 32 | False | read | 20 | 293.265 / 492.363 / 520.430 | 19.345 / 23.738 / 66.275 |
| sqlite-file | typescript | 32 | False | read-write-read | 20 | 312.997 / 681.754 / 1019.501 | 18.899 / 48.075 / 106.697 |
| sqlite-file | typescript | 32 | False | repeatable | 20 | 4069.011 / 6447.814 / 7432.778 | — |
| sqlite-file | typescript | 32 | True | projection | 20 | 209.787 / 311.334 / 532.407 | — |
| sqlite-file | typescript | 32 | True | read | 20 | 285.472 / 387.388 / 626.365 | 17.891 / 30.147 / 30.429 |
| sqlite-file | typescript | 32 | True | read-write-read | 20 | 289.938 / 322.094 / 326.599 | 17.057 / 22.131 / 25.376 |
| sqlite-file | typescript | 32 | True | repeatable | 20 | 3408.054 / 5255.806 / 5628.208 | — |
| sqlite-file | typescript | 16384 | False | projection | 20 | 344.352 / 538.763 / 540.048 | — |
| sqlite-file | typescript | 16384 | False | read | 20 | 425.541 / 513.217 / 556.795 | 22.084 / 33.114 / 37.350 |
| sqlite-file | typescript | 16384 | False | read-write-read | 20 | 296.598 / 358.488 / 395.219 | 19.833 / 22.105 / 22.118 |
| sqlite-file | typescript | 16384 | False | repeatable | 20 | 3654.721 / 4737.415 / 5828.676 | — |
| sqlite-file | typescript | 16384 | True | projection | 20 | 359.209 / 404.391 / 434.163 | — |
| sqlite-file | typescript | 16384 | True | read | 20 | 418.359 / 519.407 / 551.302 | 21.918 / 28.197 / 30.753 |
| sqlite-file | typescript | 16384 | True | read-write-read | 20 | 273.260 / 321.651 / 323.223 | 15.348 / 21.564 / 23.905 |
| sqlite-file | typescript | 16384 | True | repeatable | 20 | 4349.243 / 5702.203 / 5745.727 | — |

Persisted bytes are newly inserted canonical graph-node payload bytes in accepted owner commits. They exclude SQL metadata, indexes, WAL and mailbox/maintenance writes. Frames count both directions in the enclosing send-to-outcome window.

| Store | Dialect | Body bytes | Traced | Variant | Installed bytes median (min–max) | IPC frames sent / received |
|---|---|---:|---|---|---:|---|
| postgres | python | 32 | False | projection | 18245 (18239–19915) | 53 / 210 |
| postgres | python | 32 | False | read | 28095 (28086–36347) | 55 / 218 |
| postgres | python | 32 | False | read-write-read | 30419 (30405–32117) | 8 / 30 |
| postgres | python | 32 | False | repeatable | 366076 (365817–663471) | 53 / 210 |
| postgres | python | 32 | True | projection | 18462 (18459–18465) | 53 / 210 |
| postgres | python | 32 | True | read | 28420 (28416–28425) | 55 / 218 |
| postgres | python | 32 | True | read-write-read | 30989 (30982–30996) | 8 / 30 |
| postgres | python | 32 | True | repeatable | 378626 (378497–378756) | 53 / 210 |
| postgres | python | 16384 | False | projection | 71052 (71045–89421) | 54 / 213 |
| postgres | python | 16384 | False | read | 110132 (110123–111279) | 56 / 221 |
| postgres | python | 16384 | False | read-write-read | 177870 (177856–195971) | 8 / 29 |
| postgres | python | 16384 | False | repeatable | 3686912 (3686653–3706025) | 53 / 209 |
| postgres | python | 16384 | True | projection | 71380 (71377–71384) | 54 / 213 |
| postgres | python | 16384 | True | read | 110458 (110453–110462) | 56 / 221 |
| postgres | python | 16384 | True | read-write-read | 178440 (178433–178447) | 8 / 29 |
| postgres | python | 16384 | True | repeatable | 3699462 (3699333–3699592) | 53 / 209 |
| postgres | typescript | 32 | False | projection | 21271 (21265–23018) | 53 / 210 |
| postgres | typescript | 32 | False | read | 32597 (32588–40890) | 55 / 218 |
| postgres | typescript | 32 | False | read-write-read | 37176 (37162–38948) | 8 / 30 |
| postgres | typescript | 32 | False | repeatable | 380381 (380122–2054733) | 53 / 210 |
| postgres | typescript | 32 | True | projection | 21488 (21485–21491) | 53 / 210 |
| postgres | typescript | 32 | True | read | 32922 (32918–32927) | 55 / 218 |
| postgres | typescript | 32 | True | read-write-read | 37746 (37739–37753) | 8 / 30 |
| postgres | typescript | 32 | True | repeatable | 392932 (392802–393061) | 53 / 210 |
| postgres | typescript | 16384 | False | projection | 90578 (90571–109972) | 54 / 214 |
| postgres | typescript | 16384 | False | read | 114610 (114601–117029) | 57 / 226 |
| postgres | typescript | 16384 | False | read-write-read | 184589 (184575–204010) | 8 / 30 |
| postgres | typescript | 16384 | False | repeatable | 3701200 (3700941–3721647) | 53 / 210 |
| postgres | typescript | 16384 | True | projection | 90906 (90903–90910) | 54 / 214 |
| postgres | typescript | 16384 | True | read | 114936 (114931–114940) | 57 / 226 |
| postgres | typescript | 16384 | True | read-write-read | 185159 (185152–185166) | 8 / 30 |
| postgres | typescript | 16384 | True | repeatable | 3713750 (3713621–3713880) | 53 / 210 |
| sqlite-file | python | 32 | False | projection | 18245 (18239–19915) | 53 / 210 |
| sqlite-file | python | 32 | False | read | 28095 (28086–36347) | 55 / 218 |
| sqlite-file | python | 32 | False | read-write-read | 30419 (30405–32117) | 8 / 30 |
| sqlite-file | python | 32 | False | repeatable | 366076 (365817–663471) | 53 / 210 |
| sqlite-file | python | 32 | True | projection | 18462 (18459–18465) | 53 / 210 |
| sqlite-file | python | 32 | True | read | 28420 (28416–28425) | 55 / 218 |
| sqlite-file | python | 32 | True | read-write-read | 30989 (30982–30996) | 8 / 30 |
| sqlite-file | python | 32 | True | repeatable | 378626 (378497–378756) | 53 / 210 |
| sqlite-file | python | 16384 | False | projection | 71052 (71045–89421) | 54 / 213 |
| sqlite-file | python | 16384 | False | read | 110132 (110123–111279) | 56 / 221 |
| sqlite-file | python | 16384 | False | read-write-read | 177870 (177856–195971) | 8 / 29 |
| sqlite-file | python | 16384 | False | repeatable | 3686912 (3686653–3706025) | 53 / 209 |
| sqlite-file | python | 16384 | True | projection | 71380 (71377–71384) | 54 / 213 |
| sqlite-file | python | 16384 | True | read | 110458 (110453–110462) | 56 / 221 |
| sqlite-file | python | 16384 | True | read-write-read | 178440 (178433–178447) | 8 / 29 |
| sqlite-file | python | 16384 | True | repeatable | 3699462 (3699333–3699592) | 53 / 209 |
| sqlite-file | typescript | 32 | False | projection | 21271 (21265–23018) | 53 / 210 |
| sqlite-file | typescript | 32 | False | read | 32597 (32588–40890) | 55 / 218 |
| sqlite-file | typescript | 32 | False | read-write-read | 37176 (37162–38948) | 8 / 30 |
| sqlite-file | typescript | 32 | False | repeatable | 380381 (380122–2054733) | 53 / 210 |
| sqlite-file | typescript | 32 | True | projection | 21488 (21485–21491) | 53 / 210 |
| sqlite-file | typescript | 32 | True | read | 32922 (32918–32927) | 55 / 218 |
| sqlite-file | typescript | 32 | True | read-write-read | 37746 (37739–37753) | 8 / 30 |
| sqlite-file | typescript | 32 | True | repeatable | 392932 (392802–393061) | 53 / 210 |
| sqlite-file | typescript | 16384 | False | projection | 90578 (90571–109972) | 54 / 214 |
| sqlite-file | typescript | 16384 | False | read | 114610 (114601–117029) | 57 / 226 |
| sqlite-file | typescript | 16384 | False | read-write-read | 184589 (184575–204010) | 8 / 30 |
| sqlite-file | typescript | 16384 | False | repeatable | 3701200 (3700941–3721647) | 53 / 210 |
| sqlite-file | typescript | 16384 | True | projection | 90906 (90903–90910) | 54 / 214 |
| sqlite-file | typescript | 16384 | True | read | 114936 (114931–114940) | 57 / 226 |
| sqlite-file | typescript | 16384 | True | read-write-read | 185159 (185152–185166) | 8 / 30 |
| sqlite-file | typescript | 16384 | True | repeatable | 3713750 (3713621–3713880) | 53 / 210 |

Exact accepted owner-commit labels and appended fact kinds (per sample):

- `postgres/python/32/False/projection, postgres/python/32/True/projection, postgres/typescript/32/False/projection, postgres/typescript/32/True/projection, sqlite-file/python/32/False/projection, sqlite-file/python/32/True/projection, sqlite-file/typescript/32/False/projection, sqlite-file/typescript/32/True/projection`: commits `{"model.start": 2, "turn.admit": 1, "turn.commit": 1}`; facts `{}`.
- `postgres/python/32/False/read, postgres/python/32/True/read, postgres/python/16384/False/read, postgres/python/16384/True/read, postgres/typescript/32/False/read, postgres/typescript/32/True/read, postgres/typescript/16384/False/read, postgres/typescript/16384/True/read, sqlite-file/python/32/False/read, sqlite-file/python/32/True/read, sqlite-file/python/16384/False/read, sqlite-file/python/16384/True/read, sqlite-file/typescript/32/False/read, sqlite-file/typescript/32/True/read, sqlite-file/typescript/16384/False/read, sqlite-file/typescript/16384/True/read`: commits `{"model.done+cell.snapshot+admit": 1, "model.start": 2, "turn.admit": 1, "turn.commit": 1}`; facts `{}`.
- `postgres/python/32/False/read-write-read, postgres/python/16384/False/read-write-read, postgres/typescript/32/False/read-write-read, postgres/typescript/16384/False/read-write-read, sqlite-file/python/32/False/read-write-read, sqlite-file/python/16384/False/read-write-read, sqlite-file/typescript/32/False/read-write-read, sqlite-file/typescript/16384/False/read-write-read`: commits `{"cell.snapshot+admit": 1, "model.done+cell.snapshot+admit": 1, "model.start": 2, "round.outcome": 1, "turn.admit": 1, "turn.commit": 1}`; facts `{"admit": 1, "cell_report": 1, "x_outcome": 1, "x_start": 1}`.
- `postgres/python/32/False/repeatable, postgres/python/16384/False/repeatable, postgres/typescript/32/False/repeatable, postgres/typescript/16384/False/repeatable, sqlite-file/python/32/False/repeatable, sqlite-file/python/16384/False/repeatable, sqlite-file/typescript/32/False/repeatable, sqlite-file/typescript/16384/False/repeatable`: commits `{"cell.snapshot+admit": 49, "model.done+cell.snapshot+admit": 1, "model.start": 2, "round.outcome": 50, "turn.admit": 1, "turn.commit": 1}`; facts `{"admit": 50, "cell_report": 1, "x_outcome": 50, "x_start": 50}`.
- `postgres/python/32/True/read-write-read, postgres/python/16384/True/read-write-read, postgres/typescript/32/True/read-write-read, postgres/typescript/16384/True/read-write-read, sqlite-file/python/32/True/read-write-read, sqlite-file/python/16384/True/read-write-read, sqlite-file/typescript/32/True/read-write-read, sqlite-file/typescript/16384/True/read-write-read`: commits `{"cell.snapshot+admit": 1, "model.done+cell.snapshot+admit": 1, "model.start": 2, "round.outcome": 1, "round.traced": 1, "turn.admit": 1, "turn.commit": 1}`; facts `{"admit": 1, "cell_report": 1, "decide": 1, "x_outcome": 1, "x_start": 1}`.
- `postgres/python/32/True/repeatable, postgres/python/16384/True/repeatable, postgres/typescript/32/True/repeatable, postgres/typescript/16384/True/repeatable, sqlite-file/python/32/True/repeatable, sqlite-file/python/16384/True/repeatable, sqlite-file/typescript/32/True/repeatable, sqlite-file/typescript/16384/True/repeatable`: commits `{"cell.snapshot+admit": 49, "model.done+cell.snapshot+admit": 1, "model.start": 2, "round.outcome": 50, "round.traced": 50, "turn.admit": 1, "turn.commit": 1}`; facts `{"admit": 50, "cell_report": 1, "decide": 50, "x_outcome": 50, "x_start": 50}`.
- `postgres/python/16384/False/projection, postgres/python/16384/True/projection, postgres/typescript/16384/False/projection, postgres/typescript/16384/True/projection, sqlite-file/python/16384/False/projection, sqlite-file/python/16384/True/projection, sqlite-file/typescript/16384/False/projection, sqlite-file/typescript/16384/True/projection`: commits `{"model.done": 1, "model.start": 2, "turn.admit": 1, "turn.commit": 1}`; facts `{}`.

The final SQLite `READ-COST-MODEL` law passed: **executed=1, passed=1,
failed=0**, covering all 32 populations with no skips. Workspace `kiln clippy`,
`kiln fmt`, `//:schema_checks` and the generated-graph drift check passed.

## Commands and retained evidence

Evidence is under the rd-bench fork's `.kiln/FIG-6154/`, archived with the run
(33 MiB after compressing the raw ledgers). `build.json` / `python-build.json`,
build logs, `binary-hashes.json` / `python-binary.json` and `host.json` identify
the builds and host. The four `*-command.json` manifests contain exact argument
vectors, including all 16 selected populations for their dialect and
`--samples 20`. PostgreSQL used `kiln gate lash rd-bench` and
`durable_substrate/bench.py`, which provisions its own synchronous server.
The four `*-time.log` files contain count assertions and peak RSS. Raw ledgers
are `sqlite/results.jsonl.gz`, `postgres/postgres-nodes1.jsonl.gz`,
`python-sqlite/results.jsonl.gz` and
`python-postgres/postgres-nodes1.jsonl.gz`; both gate `host.jsonl` files retain
server settings. `final-stats.json` summarizes the exact totals and frames.
`law-both-2.log` and `final-law-receipt.json` retain the final law's result;
`clippy-both.log` retains the final workspace lint result. The initial park's
red side remains in `law-2.log`, with the main fix identified in `rebase.log`.

```sh
. ./env.sh
kiln build //crates/lash-perf:durable-substrate__bin \
  //crates/lash-vm-worker:lash-vm-worker__bin //:schema_checks \
  --materializations final --build-report .kiln/FIG-6154/build.json
(ulimit -v 8388608; /usr/bin/time -v \
  python3 .kiln/FIG-6154/run_matrix.py sqlite)
(ulimit -v 8388608; /usr/bin/time -v \
  python3 .kiln/FIG-6154/run_matrix.py postgres)
# After the single rebase onto FIG-6186 and correcting the Python fixture:
kiln build //crates/lash-perf:durable-substrate__bin \
  //crates/lash-vm-worker:lash-vm-worker__bin --materializations final \
  --build-report .kiln/FIG-6154/python-build.json
(ulimit -v 8388608; /usr/bin/time -v \
  python3 .kiln/FIG-6154/run_python_matrix.py sqlite)
(ulimit -v 8388608; /usr/bin/time -v \
  python3 .kiln/FIG-6154/run_python_matrix.py postgres)
python3 crates/lash-perf/src/bin/durable_substrate/reads_report.py --inputs \
  .kiln/FIG-6154/sqlite/results.jsonl.gz .kiln/FIG-6154/postgres/postgres-nodes1.jsonl.gz \
  .kiln/FIG-6154/python-sqlite/results.jsonl.gz \
  .kiln/FIG-6154/python-postgres/postgres-nodes1.jsonl.gz
kiln test //crates/lash-perf:durable-substrate__bin__unit_test \
  --test_arg=--exact --test_arg=reads::tests::fifty_dependent_reads_obey_the_cost_model \
  --test_arg=--nocapture --test_output=all
kiln clippy
```

No unresolved populations or counts remain. The wall-time qualification above
is a limitation of these measurements, not a timing acceptance gate.
