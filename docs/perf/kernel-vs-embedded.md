# Kernel versus embedded interpreters

FIG-6719 · measured 2026-10-17.

Morning baseline: `4bc83a775f711b66e41be9814350c273ad5c594c`. Cutover head: `46f26e8ab27f1ad4d3f58e73eec0f853021a50bf` (origin/main when this experiment started, after FIG-6709; before the arena and instruction-set cutovers).

Two reversed-order pairs per input, on one host, with hardware `perf stat -e instructions:u` as the primary metric. All **792/792 workload processes passed**: 132 unchanged cold samples and 264 differential-warm samples per revision. Complete result digests agree across all engines, both pairs, both repetition counts and both revisions for each of 11 programs. No kernel changes or performance thresholds were applied.

Host: `turbo2-sam`; AMD Ryzen 9 5950X 16-Core Processor; Linux `6.8.0-137-generic`. Engines: cpython: Python 3.12.3; lash: Kiln optimized; compiler/profile in each Lash receipt; monty: Monty 1.1.0; node: v25.2.1; quickjs: upstream QuickJS 2025-09-13.

Lash reports optimized Rust (`OPT_LEVEL=3`, debug assertions refused by the adapter), `system+stats_alloc`, and observers off. Both use rustc 1.98.1 (48a229cea), LLVM 22.1.8. Exact compiler, flags, executable/source hashes and pinned release archive hashes are retained in the receipts. Reference engines are unchanged between all four runs.

## Method and reproducibility

Cold runs used the committed instrument unchanged at each checkout:

```sh
. ./env.sh
export KILN_RE_PRIORITY=100
(ulimit -v 8388608; /usr/bin/time -v python3 scripts/perf/kernel/embedded.py --fetch-engines \
  --out .kiln/FIG-6719/<cut>-cold --report .kiln/FIG-6719/<cut>-cold.md)
```

The only instrument extension is `--differential-warm`, authorized for this experiment. It wraps each unchanged body in a function returning `result`, called N and 2N times in separate cold processes. N=2 for sum1m, branch1m and fib25; N=100 for the other eight programs. The same extension and wrapped source hashes were used on both revisions. To repeat the baseline warm row, apply only this commit’s driver change to that checkout; keep the Rust code and corpus at the named base. The recorded warm driver source is archived with the evidence. In the commands below, replace `<cut>` with `baseline` or `current` and use fresh output directories:

```sh
(ulimit -v 8388608; /usr/bin/time -v python3 scripts/perf/kernel/embedded.py \
  --differential-warm --out .kiln/FIG-6719/<cut>-warm \
  --report .kiln/FIG-6719/<cut>-warm.md)
```

The baseline warm command reused its preceding optimized artifact with `--kernel PATH`; the head warm command rebuilt through the same optimized Kiln profile. Both artifact hashes match their revision’s cold artifact. Every workload and measuring wrapper had an 8 GiB address-space cap. GNU time recorded peak RSS and load1 was sampled before every process. Counters ran without multiplexing. Nothing waited for a quiet host.

**Warm cost/run = mean over pairs of `(instructions(2N) − instructions(N)) / N`.** This estimates incremental execution on a prepared library/document, canceling fixed startup, parse, preparation, final finish/print and teardown to first order. It includes the repetition loop, function calls, replacement/disposal of prior results and body allocations. Function wrapping changes top-level session bindings to locals; it is not an exact hardware window around the original standalone cell. Literal compilation, allocator/GC effects and Node JIT behavior may not cancel exactly. These are estimates of the warm goal row, not certification of the ≤3× TypeScript / ≤2× Python goals. See the [instrument](instruments/kernel-vs-embedded.md).

Cold instructions include loader, registry, lowering, compilation, execution, result printing, teardown and the GNU time wrapper. They use the original top-level sources, one execution and one finish/print per process. “End-to-end” here is the standalone kernel adapter, without durable storage or a worker lifecycle. Cold and warm rows therefore answer different questions and must not be subtracted from each other.

## Cold end-to-end

Instructions in millions; arithmetic mean of two samples. Peer is QuickJS for TS, Monty for Python. Ratio = mean Lash / mean peer; change = head Lash / baseline Lash − 1.

| Program | Dialect | Base Lash M | Base peer M | Base ratio | Head Lash M | Head peer M | Head ratio | Lash change |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| allocation_churn | TS | 1345.745 | 4.697 | 286.50× | 1306.962 | 4.697 | 278.23× | -2.9% |
| allocation_churn | Python | 1310.143 | 3.557 | 368.31× | 1313.846 | 3.557 | 369.38× | +0.3% |
| branch1m | TS | 37084.020 | 566.490 | 65.46× | 19717.466 | 566.489 | 34.81× | -46.8% |
| branch1m | Python | 85274.604 | 1568.787 | 54.36× | 37072.225 | 1568.787 | 23.63× | -56.5% |
| chain_mutation | TS | 1235.565 | 2.536 | 487.28× | 1263.946 | 2.536 | 498.41× | +2.3% |
| chain_mutation | Python | 1279.923 | 2.406 | 531.88× | 1254.871 | 2.407 | 521.40× | -2.0% |
| dict_record | TS | 2050.962 | 7.343 | 279.33× | 1605.999 | 7.342 | 218.74× | -21.7% |
| dict_record | Python | 1817.364 | 10.301 | 176.42× | 1602.130 | 10.301 | 155.53× | -11.8% |
| fib25 | TS | 6394.327 | 106.860 | 59.84× | 17691.963 | 106.859 | 165.56× | +176.7% |
| fib25 | Python | 33668.432 | 267.049 | 126.08× | 20041.872 | 267.050 | 75.05× | -40.5% |
| large_data | TS | 1454.314 | 4.310 | 337.45× | 1345.286 | 4.310 | 312.16× | -7.5% |
| large_data | Python | 1430.219 | 4.613 | 310.07× | 1350.093 | 4.613 | 292.65× | -5.6% |
| list_map_filter | TS | 1473.840 | 4.483 | 328.79× | 1335.713 | 4.482 | 298.00× | -9.4% |
| list_map_filter | Python | 1340.841 | 3.160 | 424.34× | 1287.930 | 3.159 | 407.73× | -3.9% |
| string_build_join | TS | 1364.821 | 4.554 | 299.66× | 1306.121 | 4.554 | 286.82× | -4.3% |
| string_build_join | Python | 1318.337 | 3.997 | 329.83× | 1312.942 | 3.999 | 328.35× | -0.4% |
| sum1m | TS | 32186.367 | 566.801 | 56.79× | 15599.835 | 566.801 | 27.52× | -51.5% |
| sum1m | Python | 32890.216 | 1050.949 | 31.30× | 16352.654 | 1050.949 | 15.56× | -50.3% |
| workflow_early_return | TS | 1210.708 | 2.440 | 496.19× | 1238.574 | 2.440 | 507.58× | +2.3% |
| workflow_early_return | Python | 1214.511 | 1.725 | 704.25× | 1233.119 | 1.726 | 714.61× | +1.5% |
| workflow_iteration | TS | 1399.703 | 2.806 | 498.87× | 1296.455 | 2.806 | 462.10× | -7.4% |
| workflow_iteration | Python | 1231.407 | 2.081 | 591.73× | 1246.467 | 2.079 | 599.44× | +1.2% |

## Differential warm run-only estimate

Instructions in millions **per body execution**, using the N/2N difference above. Original FIG-5840 heap rows are chain_mutation, allocation_churn and large_data.

| Program | Dialect | Base Lash M | Base peer M | Base ratio | Head Lash M | Head peer M | Head ratio | Lash change |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| allocation_churn | TS | 145.382 | 2.505 | 58.05× | 125.985 | 2.505 | 50.29× | -13.3% |
| allocation_churn | Python | 105.459 | 2.381 | 44.29× | 135.573 | 2.381 | 56.94× | +28.6% |
| branch1m | TS | 38052.180 | 374.506 | 101.61× | 10638.142 | 374.506 | 28.41× | -72.0% |
| branch1m | Python | 75936.375 | 1485.440 | 51.12× | 27714.034 | 1485.443 | 18.66× | -63.5% |
| chain_mutation | TS | 32.615 | 0.441 | 73.88× | 43.023 | 0.441 | 97.46× | +31.9% |
| chain_mutation | Python | 82.161 | 0.988 | 83.19× | 35.223 | 0.988 | 35.66× | -57.1% |
| dict_record | TS | 852.056 | 5.013 | 169.98× | 441.326 | 5.013 | 88.04× | -48.2% |
| dict_record | Python | 608.289 | 10.174 | 59.79× | 458.444 | 10.173 | 45.06× | -24.6% |
| fib25 | TS | 27782.123 | 89.107 | 311.78× | 16638.318 | 89.107 | 186.72× | -40.1% |
| fib25 | Python | 33524.217 | 432.556 | 77.50× | 19740.490 | 432.556 | 45.64× | -41.1% |
| large_data | TS | 235.861 | 1.962 | 120.22× | 124.046 | 1.962 | 63.23× | -47.4% |
| large_data | Python | 223.101 | 3.365 | 66.30× | 139.775 | 3.365 | 41.54× | -37.3% |
| list_map_filter | TS | 274.850 | 2.075 | 132.48× | 105.079 | 2.075 | 50.65× | -61.8% |
| list_map_filter | Python | 139.749 | 1.744 | 80.11× | 57.117 | 1.744 | 32.74× | -59.1% |
| string_build_join | TS | 157.166 | 1.974 | 79.63× | 97.956 | 1.974 | 49.63× | -37.7% |
| string_build_join | Python | 113.589 | 2.605 | 43.61× | 111.387 | 2.605 | 42.77× | -1.9% |
| sum1m | TS | 34526.887 | 416.846 | 82.83× | 7134.024 | 416.846 | 17.11× | -79.3% |
| sum1m | Python | 7814.047 | 983.627 | 7.94× | 7643.028 | 983.628 | 7.77× | -2.2% |
| workflow_early_return | TS | 165.158 | 0.225 | 733.16× | 68.115 | 0.225 | 302.41× | -58.8% |
| workflow_early_return | Python | 16.092 | 0.341 | 47.15× | 11.435 | 0.341 | 33.51× | -28.9% |
| workflow_iteration | TS | 196.492 | 0.530 | 370.89× | 82.937 | 0.530 | 156.55× | -57.8% |
| workflow_iteration | Python | 30.780 | 0.598 | 51.45× | 28.365 | 0.598 | 47.42× | -7.8% |

All seven required warm estimates remain above the ≤3× TS / ≤2× Python targets: head TS spans 17.11×–186.72× and Python 7.77×–56.94× on sum1m, branch1m, fib25, list_map_filter and the three FIG-5840 heap programs. Scope effects are material: TS fib25 grows 176.7% in the unchanged cold row while its repeated-function estimate falls 40.1%. These are different executable geometries; the latter does not explain away the former regression.


## Reference engines

Instruction millions; Node/CPython are references, with no acceptance ratio. Warm columns are per-body deltas.

| Program | Node cold base → head | Node warm base → head | CPython cold base → head | CPython warm base → head |
|---|---:|---:|---:|---:|
| allocation_churn | 344.107 → 345.088 | 0.201 → unavailable | 58.461 → 58.490 | 1.251 → 1.267 |
| branch1m | 380.836 → 375.993 | 18.887 → 18.386 | 1962.473 → 1914.926 | 930.959 → 922.893 |
| chain_mutation | 343.160 → 343.942 | 0.139 → 0.095 | 57.167 → 57.231 | 0.318 → 0.313 |
| dict_record | 348.640 → 349.361 | 1.806 → 1.892 | 64.599 → 64.589 | 5.200 → 5.229 |
| fib25 | 357.529 → 361.243 | 17.102 → 21.271 | 184.822 → 184.880 | 141.738 → 141.704 |
| large_data | 354.480 → 351.205 | 0.710 → 0.719 | 59.905 → 60.051 | 1.640 → 1.634 |
| list_map_filter | 351.923 → 345.318 | 0.363 → 0.320 | 58.146 → 58.051 | 0.810 → 0.810 |
| string_build_join | 345.046 → 350.734 | 0.234 → 0.202 | 58.795 → 58.754 | 1.072 → 1.082 |
| sum1m | 383.312 → 382.239 | 28.935 → 22.314 | 1660.933 → 1641.853 | 709.515 → 709.502 |
| workflow_early_return | 345.809 → 346.412 | unavailable → unavailable | 56.981 → 57.044 | 0.119 → 0.120 |
| workflow_iteration | 344.338 → 349.889 | 0.109 → unavailable | 57.271 → 57.255 | 0.253 → 0.256 |

Node warm estimates with a nonpositive pair delta are **unavailable**, not accepted as a negative/positive run cost. Adaptive JIT and fixed-process variance can defeat the subtraction for this reference engine. The original strict renderer refused after saving the complete receipt; no workload or pair was retried. The owner authorized explicit reference-only refusals without blocking the primary ratios.

| Cut | Program | Pair | N | instructions(N) | instructions(2N) | Signed delta |
|---|---|---:|---:|---:|---:|---:|
| baseline | workflow_early_return | 2 | 100 | 362602021 | 354468536 | -8133485 |
| current | allocation_churn | 2 | 100 | 388707954 | 384692357 | -4015597 |
| current | workflow_early_return | 1 | 100 | 358071236 | 356014113 | -2057123 |
| current | workflow_iteration | 1 | 100 | 378092049 | 369980769 | -8111280 |

## Lash lower / compile / run split

Milliseconds, **base → head**, mean of the cold pair, except the last column uses the warm N/2N delta per body. These are shared-host wall diagnostics, not instruction attribution or certified latency. Registry includes argument/source input and both dialect helper sets. Library is `PreparedLibrary::new`; document compilation is lazy inside cell start. Cold run includes a finish host boundary; the final boundary cancels in the warm delta.

| Program | Dialect | Registry | Lower | Library compile | Document compile + start | Cold run | Warm run estimate |
|---|---|---:|---:|---:|---:|---:|---:|
| allocation_churn | TS | 126.137 → 129.841 | 0.616 → 0.600 | 17.447 → 21.693 | 0.434 → 0.450 | 21.603 → 13.142 | 20.794 → 14.868 |
| allocation_churn | Python | 124.204 → 129.632 | 0.387 → 0.386 | 17.416 → 22.291 | 0.485 → 0.493 | 18.052 → 14.247 | 15.634 → 18.647 |
| branch1m | TS | 127.633 → 128.084 | 0.606 → 0.554 | 18.370 → 21.270 | 0.397 → 0.386 | 3674.592 → 1849.407 | 4213.033 → 1251.796 |
| branch1m | Python | 151.025 → 129.168 | 0.274 → 0.267 | 18.526 → 21.473 | 0.396 → 0.394 | 9742.268 → 3924.818 | 8601.286 → 3589.668 |
| chain_mutation | TS | 128.352 → 137.157 | 0.750 → 0.730 | 17.769 → 24.432 | 0.685 → 0.639 | 3.987 → 5.438 | 3.786 → 5.669 |
| chain_mutation | Python | 125.665 → 129.223 | 0.387 → 0.440 | 17.272 → 22.497 | 0.586 → 0.513 | 10.370 → 3.933 | 10.035 → 4.856 |
| dict_record | TS | 124.542 → 128.206 | 1.249 → 1.230 | 17.212 → 20.764 | 0.641 → 0.616 | 120.591 → 61.245 | 159.440 → 77.023 |
| dict_record | Python | 125.010 → 126.386 | 0.361 → 0.359 | 17.358 → 20.164 | 0.612 → 0.476 | 88.193 → 55.898 | 111.027 → 48.689 |
| fib25 | TS | 125.450 → 135.841 | 0.629 → 0.574 | 17.569 → 21.310 | 0.526 → 0.434 | 530.753 → 1891.761 | 3631.693 → 1962.568 |
| fib25 | Python | 125.468 → 128.169 | 0.367 → 0.309 | 19.659 → 20.428 | 0.390 → 0.378 | 3756.725 → 2171.200 | 4764.283 → 2298.104 |
| large_data | TS | 127.162 → 127.317 | 1.489 → 1.380 | 17.769 → 20.747 | 0.881 → 0.690 | 36.371 → 17.980 | 36.459 → 19.381 |
| large_data | Python | 138.639 → 129.200 | 0.501 → 0.487 | 18.636 → 20.774 | 0.558 → 0.554 | 33.159 → 18.465 | 37.883 → 21.674 |
| list_map_filter | TS | 130.996 → 132.835 | 0.675 → 0.874 | 18.110 → 24.513 | 0.499 → 0.564 | 33.038 → 13.257 | 42.861 → 12.571 |
| list_map_filter | Python | 128.333 → 128.083 | 0.457 → 0.378 | 17.936 → 21.149 | 0.502 → 0.486 | 17.024 → 6.848 | 21.979 → 6.482 |
| string_build_join | TS | 128.919 → 128.202 | 0.613 → 0.752 | 18.012 → 21.004 | 0.442 → 0.424 | 25.215 → 15.049 | 32.471 → 17.441 |
| string_build_join | Python | 128.006 → 129.539 | 0.373 → 0.382 | 18.049 → 21.408 | 0.583 → 0.463 | 19.267 → 18.049 | 36.923 → 16.231 |
| sum1m | TS | 123.256 → 127.991 | 0.474 → 0.488 | 17.765 → 21.184 | 0.374 → 0.374 | 3138.071 → 1494.959 | 3738.950 → 625.193 |
| sum1m | Python | 126.690 → 140.525 | 0.249 → 0.349 | 17.057 → 30.546 | 0.379 → 0.380 | 3292.118 → 1603.637 | 851.674 → 705.129 |
| workflow_early_return | TS | 124.409 → 139.515 | 0.703 → 0.757 | 16.859 → 27.070 | 0.543 → 0.494 | 2.222 → 1.441 | 25.834 → 10.009 |
| workflow_early_return | Python | 125.294 → 131.012 | 0.383 → 0.395 | 17.437 → 20.289 | 0.487 → 0.508 | 2.578 → 1.864 | 1.857 → 1.489 |
| workflow_iteration | TS | 123.944 → 126.516 | 0.718 → 0.765 | 16.822 → 20.836 | 0.592 → 0.541 | 28.571 → 11.173 | 45.814 → 12.218 |
| workflow_iteration | Python | 128.409 → 129.198 | 0.448 → 0.438 | 18.146 → 23.692 | 0.517 → 0.491 | 4.912 → 3.720 | 7.444 → 3.782 |

## Largest remaining gaps

The warm ranks below retain execution work after preparation cancels to first order. Lower/compile/start costs are listed separately to distinguish fixed setup from run work. This time split does not identify an internal kernel cause.

1. **TypeScript workflow_early_return: 302.41× warm peer.** Run estimate 10.009 ms; cold lower 0.757 ms and compile/start 27.564 ms.
2. **TypeScript fib25: 186.72× warm peer.** Run estimate 1962.568 ms; cold lower 0.574 ms and compile/start 21.743 ms.
3. **TypeScript workflow_iteration: 156.55× warm peer.** Run estimate 12.218 ms; cold lower 0.765 ms and compile/start 21.377 ms.
4. **TypeScript chain_mutation: 97.46× warm peer.** Run estimate 5.669 ms; cold lower 0.730 ms and compile/start 25.072 ms.
5. **TypeScript dict_record: 88.04× warm peer.** Run estimate 77.023 ms; cold lower 1.230 ms and compile/start 21.380 ms.

The largest **cold** ratios are separately ranked below; they include setup:

1. **Python workflow_early_return: 714.61× cold peer.** Registry 131.012 ms; lower 0.395 ms; library compile 20.289 ms; document compile/start 0.508 ms; run 1.864 ms. Fixed setup dominates this row in the measured time split.
2. **Python workflow_iteration: 599.44× cold peer.** Registry 129.198 ms; lower 0.438 ms; library compile 23.692 ms; document compile/start 0.491 ms; run 3.720 ms. Fixed setup dominates this row in the measured time split.
3. **Python chain_mutation: 521.40× cold peer.** Registry 129.223 ms; lower 0.440 ms; library compile 22.497 ms; document compile/start 0.513 ms; run 3.933 ms. Fixed setup dominates this row in the measured time split.

For small bodies, cold peer ratios additionally magnify fixed registry/library setup. The warm rank isolates that effect to first order. The TypeScript fib25 cold regression is reported without a kernel fix; detailed execution profiling belongs to a follow-up lane.

## Load, memory and shared-host timing

| Cut / window | Executed / passed | Load1 min–max | Largest workload RSS (MiB) |
|---|---:|---:|---:|
| baseline / cold | 132 / 132 | 5.18–6.93 | 65.02 |
| baseline / warm | 264 / 264 | 4.61–23.13 | 167.93 |
| current / cold | 132 / 132 | 3.28–3.68 | 65.03 |
| current / warm | 264 / 264 | 6.36–23.77 | 178.40 |

Per-program/engine RSS is whole-process peak, base → head in MiB; warm takes the maximum over N and 2N. Cold wall is mean whole-process milliseconds, base → head. Per-sample load1, spawn-to-first-result, wall, RSS and GNU-time diagnostics are in the receipts.

| Program | Engine | Cold peak RSS | Warm peak RSS | Cold wall ms |
|---|---|---:|---:|---:|
| allocation_churn | lash-ts | 45.45 → 49.50 | 117.16 → 127.25 | 195.308 → 202.716 |
| allocation_churn | quickjs | 2.50 → 2.50 | 2.75 → 2.75 | 14.503 → 14.379 |
| allocation_churn | lash-py | 44.67 → 49.00 | 121.16 → 130.83 | 187.992 → 195.814 |
| allocation_churn | monty | 9.75 → 9.75 | 10.00 → 10.00 | 16.083 → 15.478 |
| allocation_churn | node | 58.77 → 58.77 | 66.28 → 66.27 | 67.824 → 67.407 |
| allocation_churn | cpython | 9.75 → 9.75 | 10.00 → 10.00 | 28.206 → 28.883 |
| branch1m | lash-ts | 42.41 → 46.50 | 43.17 → 47.25 | 3856.834 → 2028.798 |
| branch1m | quickjs | 2.50 → 2.50 | 2.50 → 2.50 | 57.237 → 50.758 |
| branch1m | lash-py | 41.91 → 45.75 | 42.41 → 46.25 | 9944.312 → 4105.964 |
| branch1m | monty | 9.50 → 9.50 | 9.50 → 9.25 | 336.311 → 312.580 |
| branch1m | node | 63.80 → 64.03 | 64.03 → 64.03 | 74.899 → 68.159 |
| branch1m | cpython | 9.75 → 9.75 | 9.75 → 9.75 | 128.859 → 123.990 |
| chain_mutation | lash-ts | 42.67 → 47.00 | 58.16 → 73.25 | 178.282 → 196.471 |
| chain_mutation | quickjs | 2.50 → 2.50 | 2.75 → 2.50 | 13.948 → 15.744 |
| chain_mutation | lash-py | 42.16 → 46.00 | 57.67 → 67.25 | 182.071 → 183.294 |
| chain_mutation | monty | 9.75 → 9.75 | 9.75 → 9.75 | 16.034 → 16.513 |
| chain_mutation | node | 58.53 → 58.53 | 66.02 → 65.58 | 67.545 → 68.030 |
| chain_mutation | cpython | 9.75 → 9.75 | 10.00 → 10.00 | 27.735 → 28.601 |
| dict_record | lash-ts | 48.43 → 52.50 | 148.41 → 158.39 | 291.805 → 241.272 |
| dict_record | quickjs | 2.50 → 2.50 | 2.75 → 2.75 | 14.184 → 15.863 |
| dict_record | lash-py | 47.67 → 51.75 | 167.93 → 178.40 | 259.794 → 230.926 |
| dict_record | monty | 9.75 → 10.00 | 10.25 → 10.25 | 16.407 → 17.580 |
| dict_record | node | 59.02 → 59.02 | 66.52 → 66.28 | 71.977 → 68.774 |
| dict_record | cpython | 9.75 → 9.75 | 9.75 → 10.00 | 27.597 → 29.040 |
| fib25 | lash-ts | 42.92 → 46.50 | 43.16 → 46.75 | 703.710 → 2081.248 |
| fib25 | quickjs | 2.50 → 2.50 | 2.75 → 2.75 | 21.282 → 22.621 |
| fib25 | lash-py | 42.16 → 46.25 | 42.41 → 46.25 | 3932.327 → 2352.085 |
| fib25 | monty | 9.50 → 9.50 | 9.50 → 9.50 | 58.536 → 57.598 |
| fib25 | node | 63.52 → 63.77 | 64.56 → 64.02 | 69.874 → 70.134 |
| fib25 | cpython | 9.75 → 9.75 | 9.75 → 9.75 | 37.421 → 38.840 |
| large_data | lash-ts | 44.66 → 49.25 | 78.66 → 96.50 | 213.120 → 199.470 |
| large_data | quickjs | 2.75 → 2.75 | 2.75 → 2.75 | 15.507 → 15.608 |
| large_data | lash-py | 43.66 → 47.75 | 83.42 → 94.50 | 220.619 → 196.777 |
| large_data | monty | 10.50 → 10.50 | 10.50 → 10.50 | 16.422 → 17.161 |
| large_data | node | 58.78 → 58.78 | 67.04 → 67.02 | 70.607 → 68.636 |
| large_data | cpython | 10.00 → 10.00 | 10.00 → 10.00 | 28.381 → 27.313 |
| list_map_filter | lash-ts | 43.42 → 47.50 | 52.66 → 57.25 | 210.898 → 202.776 |
| list_map_filter | quickjs | 2.75 → 2.75 | 2.75 → 2.75 | 15.323 → 15.685 |
| list_map_filter | lash-py | 42.41 → 46.50 | 50.36 → 54.50 | 191.155 → 185.130 |
| list_map_filter | monty | 9.75 → 10.00 | 10.00 → 10.00 | 16.642 → 16.844 |
| list_map_filter | node | 58.78 → 58.77 | 66.27 → 66.28 | 69.011 → 66.973 |
| list_map_filter | cpython | 10.00 → 10.00 | 10.00 → 10.00 | 28.360 → 29.306 |
| string_build_join | lash-ts | 44.92 → 49.50 | 79.91 → 90.50 | 202.205 → 194.644 |
| string_build_join | quickjs | 2.50 → 2.75 | 3.00 → 3.00 | 15.032 → 15.381 |
| string_build_join | lash-py | 44.66 → 48.75 | 85.41 → 99.75 | 193.915 → 198.676 |
| string_build_join | monty | 10.50 → 10.00 | 10.50 → 10.00 | 16.640 → 16.896 |
| string_build_join | node | 58.53 → 58.52 | 66.20 → 66.28 | 65.971 → 68.756 |
| string_build_join | cpython | 10.00 → 10.00 | 10.00 → 10.00 | 27.083 → 27.155 |
| sum1m | lash-ts | 42.41 → 46.25 | 43.16 → 46.75 | 3310.699 → 1676.238 |
| sum1m | quickjs | 2.50 → 2.50 | 2.50 → 2.50 | 49.366 → 48.014 |
| sum1m | lash-py | 41.91 → 45.75 | 42.41 → 46.25 | 3464.959 → 1812.391 |
| sum1m | monty | 9.25 → 9.00 | 9.25 → 9.25 | 220.461 → 205.976 |
| sum1m | node | 65.02 → 65.03 | 65.28 → 65.28 | 75.905 → 76.838 |
| sum1m | cpython | 9.75 → 9.75 | 9.75 → 9.75 | 112.506 → 112.872 |
| workflow_early_return | lash-ts | 43.40 → 47.25 | 65.91 → 81.75 | 170.363 → 198.259 |
| workflow_early_return | quickjs | 2.50 → 2.50 | 2.75 → 2.75 | 14.324 → 15.347 |
| workflow_early_return | lash-py | 42.41 → 46.25 | 47.91 → 51.75 | 172.205 → 179.911 |
| workflow_early_return | monty | 9.75 → 9.50 | 9.75 → 10.00 | 15.810 → 16.424 |
| workflow_early_return | node | 58.53 → 58.77 | 59.52 → 59.77 | 68.501 → 66.423 |
| workflow_early_return | cpython | 9.75 → 9.75 | 10.00 → 10.00 | 35.276 → 28.383 |
| workflow_iteration | lash-ts | 43.91 → 48.00 | 74.41 → 84.25 | 198.168 → 187.918 |
| workflow_iteration | quickjs | 2.75 → 2.75 | 2.75 → 2.75 | 14.699 → 14.002 |
| workflow_iteration | lash-py | 42.66 → 46.50 | 62.16 → 72.75 | 180.367 → 184.017 |
| workflow_iteration | monty | 9.75 → 9.50 | 10.25 → 10.00 | 15.921 → 17.143 |
| workflow_iteration | node | 58.77 → 58.77 | 60.53 → 60.53 | 66.446 → 67.118 |
| workflow_iteration | cpython | 9.75 → 9.75 | 10.00 → 10.00 | 27.641 → 27.231 |

## Evidence and limits

Evidence root: `.kiln/FIG-6719/` in the kbench-peer fork, archived with the run. The four `<cut>-<window>/receipt.json` files include all raw counts, commands, digests, resource measurements and Lash phase receipts; `samples.jsonl` and per-process stdout, stderr, perf TSV and GNU-time files preserve the individual pairs. `compare.py` validates cross-revision equality and renders this table without executing engines.

Unchanged cold driver SHA-256: `ea987df69007752d33040ff30534e5421070bbb2849dfc458fa810b8b16655e4`. Warm driver SHA-256: `07e9adb98ee18aea86359fe005062e676a15cf882e247b75041d379e27289c3f` (archived as `embedded-warm-measured.py`).

After sampling, the renderer was updated to expose Node reference refusals and to remove the obsolete pre-FIG-6672 heading. Measurement, workload and pairing code stayed identical.

Quiet-host wall latency remains unmeasured. Per-phase RSS, large-state park/resume and an exact acknowledged warm hardware window are outside this experiment; no claim is made about them. The differential warm estimate does not certify the original cell’s warm goal under identical binding scope. No test laws were added or changed: the proof is the complete paired experiment and its equality checks.
