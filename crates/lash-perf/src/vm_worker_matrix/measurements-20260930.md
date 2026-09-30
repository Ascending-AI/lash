# Integrated worker measurements

The optimized FIG-4162 matrix completed on one shared Linux x86_64 machine. All results are retained, including failed diagnostic budgets. The JSON alongside this report carries all 269 distributions, units, counts, source hashes, binary checksum and machine metadata.

Measurement base: `e589d1162f961a7410dcf6cf2a73fd34cf43a6f8` plus this lane's source changes. Optimized executable SHA256: `75c6e629d473062ff4b99bf2297ec8aa1a63a2904c4e96f2126cad79bdbc63f9`. The paired control retains the f0 `State`/`execute` boundary and translated ten-feed source on this tree; resumed segments use the owned VM interface. Historical f0 figures are context, not the matched control.

AMD Ryzen 9 5950X, 32 logical CPUs, Linux 6.8.0-137-generic, rustc 1.98.1 (48a229cea 2026-09-01), System allocator, schedutil governor, all CPUs eligible. Start UTC: 2026-09-30T17:47:03.287076+00:00. Load averages at start: [52.3583984375, 55.22802734375, 84.80810546875]; after validation: [31.26806640625, 50.451171875, 63.27587890625]. Other lanes were active. No outliers or failed thresholds were removed.

Each warm baseline and worker population has 10,000 checked executions, alternating order; every cold worker population has 200 exec/prewarm/run observations. The baseline creates fresh VM state on every observation, including the warm control. Cold describes helper startup; it excludes pool teardown and allows warm page caches. There is no separate cold executable-start population for the in-process control. Runtime construction for the reference stays outside its timer. No provider, durable journal, store or real tool latency is included.

The benchmark retains the canonical optimized workspace feature graph: worker/client/lashlang testing features are enabled, while no test frontend, hook or continuation probe is invoked. The packaged helper is separately built and verified with testing disabled. These timings describe the optimized developer matrix, not a separately sampled registry/release embedding.

## Service time

All cells below are p50 / p99 / max in milliseconds. Throughput is reciprocal measured closed-loop service time, not offered-load capacity.

| Workload | Baseline warm | Worker warm | Worker cold |
| --- | --- | --- | --- |
| zero-effects | 0.112 / 0.298 / 26.568 | 0.269 / 0.639 / 5.592 | 2.237 / 4.143 / 4.358 |
| profiler-fresh | 0.125 / 0.363 / 4.478 | 0.352 / 0.881 / 7.558 | 3.447 / 6.926 / 7.800 |
| profiler-ten-feeds | 3.524 / 27.017 / 90.131 | 8.985 / 50.406 / 101.314 | 13.179 / 22.479 / 27.937 |
| scalar-0 | 0.342 / 4.585 / 30.938 | 0.792 / 7.799 / 47.092 | 3.749 / 7.137 / 8.161 |
| parallel-0 | 0.156 / 2.192 / 14.184 | 0.471 / 3.579 / 26.630 | 5.758 / 13.103 / 24.555 |
| scalar-1 | 0.368 / 4.817 / 34.183 | 0.892 / 7.757 / 63.716 | 3.524 / 5.037 / 8.535 |
| parallel-1 | 0.490 / 2.180 / 7.934 | 1.062 / 4.281 / 19.050 | 5.339 / 9.103 / 9.692 |
| scalar-10 | 0.380 / 1.416 / 6.042 | 1.430 / 4.367 / 26.463 | 3.991 / 5.361 / 7.979 |
| parallel-10 | 0.774 / 3.889 / 21.949 | 1.477 / 6.892 / 19.435 | 5.330 / 10.594 / 12.205 |
| scalar-100 | 0.951 / 7.473 / 338.070 | 9.402 / 46.582 / 227.182 | 9.678 / 12.487 / 12.974 |
| parallel-100 | 4.366 / 22.372 / 70.922 | 7.307 / 33.739 / 112.922 | 14.182 / 31.524 / 37.122 |
| value-32 | 0.384 / 5.256 / 30.990 | 1.165 / 10.169 / 33.400 | 5.755 / 15.739 / 21.631 |
| value-8192 | 0.443 / 5.910 / 33.558 | 1.381 / 13.414 / 34.276 | 5.442 / 14.442 / 15.982 |
| value-1044480 | 1.283 / 4.831 / 23.498 | 15.386 / 30.283 / 56.179 | 27.261 / 58.220 / 65.099 |
| resumed-segments | 0.984 / 7.016 / 36.859 | 13.258 / 53.568 / 157.808 | 15.520 / 47.816 / 68.806 |
| guest-error-replacement | 0.310 / 2.666 / 27.155 | 4.877 / 14.346 / 30.141 | 9.351 / 22.087 / 29.730 |

| Workload | Baseline cases/s | Worker warm cases/s | Worker cold cases/s |
| --- | ---: | ---: | ---: |
| zero-effects | 7951.4 | 3477.1 | 416.8 |
| profiler-fresh | 7157.8 | 2629.0 | 273.3 |
| profiler-ten-feeds | 193.7 | 82.9 | 73.1 |
| scalar-0 | 1642.2 | 778.0 | 253.0 |
| parallel-0 | 3997.0 | 1566.5 | 160.3 |
| scalar-1 | 1557.4 | 728.2 | 279.0 |
| parallel-1 | 1785.4 | 821.5 | 181.0 |
| scalar-10 | 2310.3 | 624.1 | 249.4 |
| parallel-10 | 1105.9 | 576.3 | 177.3 |
| scalar-100 | 706.5 | 85.6 | 101.9 |
| parallel-100 | 183.4 | 112.2 | 65.2 |
| value-32 | 1235.4 | 505.4 | 150.9 |
| value-8192 | 1125.2 | 436.0 | 164.4 |
| value-1044480 | 681.6 | 61.7 | 33.8 |
| resumed-segments | 739.6 | 61.6 | 56.0 |
| guest-error-replacement | 2400.8 | 187.4 | 97.7 |

## Effect exchanges and budgets

The zero-resource-effect paired overhead is 0.155 / 0.475 / 5.422 ms. Its p50/p99 pass the fixed 1/5 ms budget. Finish and cancellation checkpoint messages are part of the production protocol, while effect counts below count admitted echo resource calls.

Effect exchange includes parent decode/echo/encode, IPC and guest work until the next request. A parallel aggregate is a whole batch, not a per-leaf RPC. Large values include serialization. Every population is compared with the fixed 100/500 microsecond diagnostic threshold; this is not a portable CI wall-clock gate.

| Workload | Samples | p50 / p99 / max us | Over p50 or p99 budget |
| --- | ---: | --- | --- |
| scalar-1 | 10000 | 105.338 / 1281.293 / 8043.203 | yes |
| parallel-1 | 10000 | 123.392 / 464.194 / 9369.539 | yes |
| scalar-10 | 100000 | 72.276 / 248.477 / 5537.756 | no |
| parallel-10 | 10000 | 224.362 / 1003.890 / 8828.751 | yes |
| scalar-100 | 1000000 | 77.987 / 513.076 / 35215.677 | yes |
| parallel-100 | 10000 | 1429.942 / 7329.859 / 38812.298 | yes |
| value-32 | 10000 | 125.977 / 2992.784 / 8506.715 | yes |
| value-8192 | 10000 | 154.060 / 3098.163 / 17600.324 | yes |
| value-1044480 | 10000 | 4677.737 / 10108.119 / 19399.350 | yes |
| resumed-segments | 100000 | 101.281 / 1315.807 / 17278.077 | yes |

## Memory and checkout

RSS includes shared pages; VmHWM is the process lifetime high-water mark. These are neither PSS nor OS memory limits. Parent figures include both the reference and worker client, plus sample buffers; they are not a standalone production-host measurement. Worker memory probes run separately from latency timing, every hundredth warm observation (100 samples) and after each cold run (200 samples). Guest-error workers are reaped before the terminal is observed, so their post-work RSS/HWM is unavailable, never zero.

| Warm workload | Parent RSS p50/p99/max KiB | Parent HWM max KiB | Worker RSS p50/p99/max KiB | Worker HWM max KiB |
| --- | --- | ---: | --- | ---: |
| zero-effects | 11232.000 / 11588.000 / 11588.000 | 11588 | 11948.000 / 11948.000 / 11948.000 | 11948 |
| profiler-fresh | 11848.000 / 12140.000 / 12140.000 | 12140 | 12452.000 / 12452.000 / 12452.000 | 12452 |
| profiler-ten-feeds | 13856.000 / 14360.000 / 14368.000 | 14368 | 14080.000 / 14084.000 / 14088.000 | 14088 |
| scalar-0 | 13828.000 / 13904.000 / 13904.000 | 13904 | 14128.000 / 14128.000 / 14144.000 | 14144 |
| parallel-0 | 13980.000 / 13980.000 / 13980.000 | 13980 | 14128.000 / 14128.000 / 14128.000 | 14128 |
| scalar-1 | 14044.000 / 14140.000 / 14140.000 | 14140 | 14128.000 / 14128.000 / 14128.000 | 14128 |
| parallel-1 | 14284.000 / 14284.000 / 14284.000 | 14284 | 14128.000 / 14128.000 / 14128.000 | 14128 |
| scalar-10 | 14308.000 / 15108.000 / 15116.000 | 15116 | 14128.000 / 14128.000 / 14128.000 | 14128 |
| parallel-10 | 13676.000 / 14008.000 / 14008.000 | 14008 | 14120.000 / 14124.000 / 14124.000 | 14124 |
| scalar-100 | 18172.000 / 22000.000 / 22080.000 | 22080 | 14152.000 / 14152.000 / 14152.000 | 14152 |
| parallel-100 | 14716.000 / 14976.000 / 14976.000 | 26144 | 15172.000 / 15236.000 / 15236.000 | 15464 |
| value-32 | 15000.000 / 15012.000 / 15012.000 | 26144 | 15136.000 / 15136.000 / 15136.000 | 15464 |
| value-8192 | 15040.000 / 15040.000 / 15040.000 | 26144 | 15160.000 / 15160.000 / 15160.000 | 15464 |
| value-1044480 | 20792.000 / 20792.000 / 20792.000 | 26144 | 19264.000 / 19264.000 / 19264.000 | 20132 |
| resumed-segments | 18368.000 / 18372.000 / 18372.000 | 26144 | 10848.000 / 10852.000 / 10852.000 | 20132 |
| guest-error-replacement | 18616.000 / 18616.000 / 18616.000 | 26144 | unavailable | unavailable |

| Cold workload | Parent RSS p50/p99/max KiB | Parent HWM max KiB | Worker RSS p50/p99/max KiB | Worker HWM max KiB |
| --- | --- | ---: | --- | ---: |
| zero-effects | 11732.000 / 11732.000 / 11732.000 | 11732 | 11904.000 / 11940.000 / 11944.000 | 11944 |
| profiler-fresh | 12140.000 / 12140.000 / 12140.000 | 12140 | 12372.000 / 12412.000 / 12416.000 | 12416 |
| profiler-ten-feeds | 14524.000 / 14524.000 / 14524.000 | 14524 | 13852.000 / 13892.000 / 13892.000 | 13892 |
| scalar-0 | 13980.000 / 13980.000 / 13980.000 | 13980 | 13008.000 / 13048.000 / 13048.000 | 13048 |
| parallel-0 | 13980.000 / 13980.000 / 13980.000 | 13980 | 12108.000 / 12148.000 / 12152.000 | 12152 |
| scalar-1 | 14284.000 / 14284.000 / 14284.000 | 14284 | 13012.000 / 13048.000 / 13052.000 | 13052 |
| parallel-1 | 14284.000 / 14284.000 / 14284.000 | 14284 | 13176.000 / 13216.000 / 13216.000 | 13216 |
| scalar-10 | 15896.000 / 15896.000 / 15896.000 | 15896 | 13032.000 / 13068.000 / 13072.000 | 13072 |
| parallel-10 | 14008.000 / 14008.000 / 14008.000 | 14008 | 13296.000 / 13336.000 / 13336.000 | 13336 |
| scalar-100 | 22080.000 / 22080.000 / 22080.000 | 26144 | 13240.000 / 13276.000 / 13280.000 | 13280 |
| parallel-100 | 14976.000 / 14976.000 / 14976.000 | 26144 | 14468.000 / 14504.000 / 14508.000 | 14508 |
| value-32 | 15016.000 / 15016.000 / 15016.000 | 26144 | 12732.000 / 12772.000 / 12772.000 | 12772 |
| value-8192 | 15040.000 / 15040.000 / 15040.000 | 26144 | 12856.000 / 12896.000 / 12896.000 | 12896 |
| value-1044480 | 16648.000 / 16648.000 / 16648.000 | 26144 | 12756.000 / 12796.000 / 12796.000 | 15624 |
| resumed-segments | 18368.000 / 18368.000 / 18368.000 | 26144 | 11560.000 / 11600.000 / 11600.000 | 11600 |
| guest-error-replacement | 18616.000 / 18616.000 / 18616.000 | 26144 | unavailable | unavailable |

| Workload | Warm checkout p50/p99/max us | Cold checkout p50/p99/max us |
| --- | --- | --- |
| zero-effects | 0.240 / 0.842 / 137.960 | 0.301 / 1.423 / 2.405 |
| profiler-fresh | 0.240 / 0.842 / 8.496 | 0.501 / 1.563 / 2.064 |
| profiler-ten-feeds | 3.138 / 10.418 / 1555.236 | 4.096 / 7.093 / 53.220 |
| scalar-0 | 0.301 / 1.744 / 66.055 | 1.833 / 2.915 / 18.245 |
| parallel-0 | 0.300 / 1.243 / 198.774 | 0.792 / 1.873 / 2.635 |
| scalar-1 | 0.311 / 1.714 / 202.772 | 0.622 / 2.485 / 3.236 |
| parallel-1 | 0.290 / 1.262 / 51.457 | 1.673 / 3.176 / 3.537 |
| scalar-10 | 0.290 / 1.323 / 54.572 | 0.411 / 1.533 / 1.603 |
| parallel-10 | 0.300 / 1.623 / 707.782 | 0.872 / 3.056 / 8.927 |
| scalar-100 | 0.801 / 2.375 / 143.741 | 1.413 / 2.685 / 15.589 |
| parallel-100 | 0.992 / 2.936 / 616.140 | 0.922 / 2.645 / 2.865 |
| value-32 | 0.531 / 1.833 / 543.433 | 2.074 / 3.748 / 481.997 |
| value-8192 | 0.682 / 2.104 / 2445.274 | 1.523 / 3.156 / 3.347 |
| value-1044480 | 0.942 / 2.304 / 584.991 | 1.373 / 3.497 / 4.118 |
| resumed-segments | 3.928 / 12.671 / 2406.048 | 4.389 / 9.367 / 23.345 |
| guest-error-replacement | 1.002 / 3.145 / 150.894 | 2.204 / 3.257 / 3.938 |

Checkout times for multi-cell and resumed workloads sum all admissions in that case. ResetDone precedes reuse. Error discard, reap and replacement happen inside the worker service timer; the separate reset-or-discard samples for guest errors only time the final empty checkout drop.

| Prewarmed width | Batch p50/p99/max ms | Completed cases/s |
| --- | --- | ---: |
| 1 | 0.691 / 5.830 / 36.205 | 932.7 |
| 2 | 0.559 / 2.198 / 8.414 | 3016.0 |
| 4 | 0.548 / 2.011 / 13.766 | 6375.3 |

Each width has 10,000 complete batches and includes thread launch/join. The saturated one-slot probe has 10,000 actual queued admissions: 46.167 / 429.209 / 3378.171 us. The holder is released after the waiter has entered the item/byte queue, with no artificial sleep.

## Preset decision

Retain one prewarmed worker, maximum four, two queued inputs/eight MiB, and the explicit existing protocol/VM/deadline/restart bounds. The width matrix supports bounded concurrency, but thread overhead and a shared machine do not establish the optimum for arbitrary hosts. Reducing compute/CPU ceilings or the larger RLM/process state envelope from short echo workloads would reject valid workloads without evidence. These limits remain host-selectable resource admission bounds, not latency targets. Effect budget failures remain unresolved.

## Evidence

Raw CSV: `.benchmarks/fig-4162/data-final/samples.csv`; 2,871,800 samples; SHA256 `740701fafcdf58fa5030fa4ed56824a971cb2c2a9bdec369cfc5b7e9a5f64c92`. Independent reconstruction verified every count, nearest-rank p50/p99 and maximum. Raw data and gate logs stay in the fork; the compact complete distributions and metadata are committed in `measurements-20260930.json`.

The sampled base has the ignored FIG-4275 Await/ResourceOperationBatch parking gap. FIG-4275 landed as `7fa019b40c` during this lane's landing; its runtime changes are outside this frozen timing population. FIG-3821 stays open as instructed. FIG-4408 owns standalone registry consumption of the build-identity closure. macOS execution/release publication were not dispatched. Error-workload post-work memory remains unavailable with this terminal-based sampler.
