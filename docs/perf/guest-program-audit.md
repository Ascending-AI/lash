# Guest-program audit

FIG-7466. Audited at `a1eddaf26abdf2a1f789b23b5fdad7e53bc0ae34`, then corrected the programs listed below.

The inventory follows the programs that the maintained perf and profile entry points execute, including included sources and generated cells. Host-side Rust, Python report code, scripted provider replies, store fixtures and the conformance corpus recorder are not guest programs. The identity instrument also reads every conformance document and installed definition for canonical encoding and hashing, without executing their guest bodies. User-selected source paths are identified as open-ended inputs; their runners have no default cell.

An opcode microbenchmark deliberately repeats an operation. Arithmetic, branching, recursive calls, allocation, list mutation and record accumulation remain valid microbenchmarks. A JSON codec, sort implementation or local replacement for a host tool would bypass functionality the user can call. No guest regex, hashing, date, math-function or sorting implementations were found. The full JSON codec found is reserved to FIG-7465; a small JSON object encoder in gate six is corrected here.

## Corrections

- `compare/workloads/array_read.py` was a **twin mismatch**. It constructed 1 through 10 and combined ten reads in one addition chain. The TS twin constructs 0 through 9 and updates the accumulator after each read. Python now does the same work. Its checksum becomes `45 * n`, matching TS. Old Python `array_read` source hashes and rows are not comparable.
- The TS/Python `workflow_iteration` and `workflow_early_return` programs **reimplemented the echo tool** as a local identity function. They now await `perf.echo`, which crosses the kernel host-operation boundary. They moved from `embedded/` to `workflows/`; `workflows.py` records both dialects, checks the full result and retains both PMU pairs. CLI peer rows were retired because the peer CLIs have no Lash host adapter. All earlier workflow rows and peer ratios are not comparable.

- `shared/gate_six.rs`'s `heap_variable_concat` copied each element with a nested push loop. It now calls the shipped `Array.concat` builtin. Earlier rows for this program are not comparable.

- `shared/gate_six.rs`'s `language_host_environment` assembled a JSON object with string concatenation and `String`. It now uses `JSON.stringify` before the existing native parse. The result checksum is unchanged; old source hashes select a different cohort.

## Compare corpus

Each name below identifies both `crates/lash-perf/src/compare/workloads/<name>.js` and `.py`, included by `compare/workloads.rs`. The instruction gate and counted-history recipe use all 17 pairs.

| Program | Classification | Evidence |
|---|---|---|
| empty_loop | ok | Direct operation microbenchmark; no replacement algorithm. |
| prop_read | ok | Direct operation microbenchmark; no replacement algorithm. |
| prop_write | ok | Direct operation microbenchmark; no replacement algorithm. |
| array_read | twin mismatch, fixed | Aligned inputs, ten reads and ten accumulator updates. |
| func_call | ok | Direct operation microbenchmark; no replacement algorithm. |
| func_closure_call | ok | Captured accumulator call microbenchmark. |
| int_arith | ok | Direct operation microbenchmark; no replacement algorithm. |
| float_arith | ok | Direct operation microbenchmark; no replacement algorithm. |
| map_set_string | ok | Native Map/string conversion and Python dict/str. |
| string_build1 | ok | Explicit concatenation opcode microbenchmark. |
| int_to_string | ok | Native string conversion. |
| sort_bench | twin mismatch, other lane | Python 1,000 strings versus TS fourteen 1,000-element fill/sort cases. FIG-7458 owns both files; untouched here. |
| json_transform | reimplements JSON, other lane | Python hand-written encode/decode versus TS native JSON. FIG-7465 owns the correction; untouched here. |
| records | ok | Builtin filter/map/reduce and Python comprehensions/sum. |
| templating | ok | Native template literals and f-strings. |
| object_build | ok | Direct operation microbenchmark; no replacement algorithm. |
| object_sort | ok, other lane | Both build the same records and use builtin sorting; Python key sort versus TS comparator is the natural dialect API. FIG-7441 owns both files. |

## Kernel and cell profiles

`crates/lash-dialect-typescript/shared/gate_six.rs::programs` supplies `kernel_perf` and its phase/work/guest profiles. Every row uses the production worker registry through `examples/kernel_perf/registry.rs`. These are TS-only programs, with no Python twin.

| Program | Classification |
|---|---|
| `baseline` | ok |
| `language_host_environment` | reimplements JSON object encoding, fixed |
| `async_await` | ok |
| `direct_unwrap` | ok |
| `general_fanout` | ok |
| `loop_control` | ok |
| `indexed_assignment` | ok |
| `projected_values` | ok |
| `large_data` | ok |
| `cache_pressure` | ok |
| `projected_operations` | ok |
| `type_system_stress` | ok |
| `wrapped_error_paths` | ok |
| `tool_control_host_environment` | ok |
| `snapshot_projected_state` | ok |
| `continue_as_seed_host_environment` | ok |
| `syntax_text_host_environment` | ok |
| `integer_range_host_environment` | ok |
| `fanout_expression_host_environment` | ok |
| `image_host_environment` | ok |
| `heap_list_iteration` | ok |
| `heap_nested_loop` | ok |
| `heap_allocation_churn` | ok |
| `heap_deep_chain_mutation` | ok |
| `heap_comprehension_build` | ok |
| `heap_variable_concat` | reimplements array concat, fixed |
| `rows_print_3000` | ok |
| `heap_shallow_chain_mutation` | ok |
| `heap_deep_chain_mutation_24` | ok |

`host-json` in `crates/lash-vm-worker/examples/kernel_perf.rs::host_json_probe` has four variants: `nested`, `generic`, `silent`, `nulls`; `all` selects them together. They use native JSON.stringify or direct console output, with native host-produced data. All are **ok**. `generic` deliberately requests the supported null-replacer path.

Paired sources in `scripts/perf/kernel/embedded/`, each `.ts` and `.py`:

| Program | Classification |
|---|---|
| `allocation_churn` | ok; native operations or explicit opcode/heap microbenchmark |
| `branch1m` | ok; native operations or explicit opcode/heap microbenchmark |
| `chain_mutation` | ok; native operations or explicit opcode/heap microbenchmark |
| `dict_record` | ok; native operations or explicit opcode/heap microbenchmark |
| `dict_record_plus` | ok; native operations or explicit opcode/heap microbenchmark |
| `dict_record_template` | ok; native operations or explicit opcode/heap microbenchmark |
| `fib25` | ok; native operations or explicit opcode/heap microbenchmark |
| `large_data` | ok; native operations or explicit opcode/heap microbenchmark |
| `list_map_filter` | ok; native operations or explicit opcode/heap microbenchmark |
| `string_build_join` | ok; native operations or explicit opcode/heap microbenchmark |
| `sum1m` | ok; native operations or explicit opcode/heap microbenchmark |

Paired sources in `scripts/perf/kernel/workflows/`:

| Program | Classification |
|---|---|
| `workflow_iteration.ts`, `.py` | reimplements echo, fixed; 128 native host awaits |
| `workflow_early_return.ts`, `.py` | reimplements echo, fixed; 64 negative branches and 64 native host awaits |

## Embedded and generated perf cells

| Location | Programs | Classification |
|---|---|
| `crates/lash-perf/src/bin/durable_substrate/cells.rs::cell_source` | H5 echo loop, retained padding, fresh-padding variant | ok; awaits `tools.extEcho` |
| `bin/durable_substrate/reads.rs::source` | TS/Python declared read chain; mixed read/write; payload/count variants | ok; native tool awaits and equal assertions |
| `bin/durable_substrate/heap.rs::source` | list-update, objects-update, string-length, drop-subgraph, next-cell, worker-resets | ok; builtin repeat, mutation and host phase gates |
| `bin/durable_substrate/turn.rs` | Scripted standard turns | ok; no guest cell; cell sessions delegate to `cells.rs` |
| `bin/durable_substrate/process.rs`, `processes.rs`, `process_registry.rs`, `operations.rs`, `store_bench.rs` | Native engine/step, lifecycle and store fixtures | ok; host implementations, no guest-language replacement |
| `bin/growturn.rs::provider` | Growing payload, wait/park, sessions variants | ok; `tools.growPayload`, `tools.growWait`, builtin length and print |
| `bin/identity.rs::AGENT_CELLS`, `workflow` | Search/fetch summary, deploy retry, order report, generated processItem steps | ok; source lowering/identity only, not guest execution; native tools, sort, Set, formatting and strings |
| `bin/admission.rs` | Workload generator cells | ok; uses `workload/provider.rs` |
| `latency/provider.rs::cell_source` | Echo and finish cells | ok; native tool and control calls |
| `latency/history.rs::cell_text` | Ordinary, continued-frame and growing-heap history cells | ok; native echo, reduce, control.finish/continueAs |
| `latency/startup.rs` | Helper spawn-to-Ready | ok; no guest program |
| `workload/provider.rs::cell_source`, `process_body`, `queued_response`, `admitted_response` | Serial/parallel tools, attachments, child process start/await, sleep, terminal and combined cells | ok; native tools, repeat/slice, Promise.all and process controls |
| `offered_load/populations.rs`, `boundary/seeded.rs` | Generated turn cells | ok; uses `workload/provider.rs` |
| `boundary/observation/mod.rs::loop_document`, `commit_document`, `QUIET_DOCUMENT`, `BLANK_DOCUMENT` | Kernel mark loop, commit/yield, sleep, empty return | ok; native mark/sleep operations |
| `boundary/observation/namespace.rs` | Constant-return redefinition cells | ok; minimal namespace publication probe |
| Other `boundary/` files | Host facade/token/transport/attachment/event probes | ok; no additional guest source |
| `runtime_perf/providers/profiles.rs` | RLM finish, streamed cell, globals, large print, echo, async completion, process handles, process async, settlement 2/8 children, spawn, oblique search/judge, ingress, deep composition, llm.query, high-traffic tool/child/finish; delegated-child replies | ok; native operations, JSON.stringify, process/agent APIs and supported builtins |
| `runtime_perf/measurement/checkpoint.rs::checkpoint_exec_code` | Three echo processes and finish | ok; native process start/await and echo |
| `runtime_perf/measurement/{checkpoint_curve,frame_residency_curve,resident_graph_curve}.rs` and `lash-protocol-rlm/src/executor/perf_fixture.rs` | Checkpoint binding fixtures | ok; host globals patches/capture/restore and `assign_one` native list construction/rebinding |
| Other `runtime_perf/measurement/` files | Native host/store/provider loops | ok; provider cells come from `profiles.rs` |
| `replay/control.rs::workload` | Standard serial/parallel and TS/Python cell serial/parallel | ok; identical lookup args; Promise.all / asyncio.gather |
| `replay/recorder.rs` | Captured cell source | open-ended input; records supplied source, no generated replacement |

## Replay corpus

Locations below are under `crates/lash-perf/workloads/replay/`. Sources are retained as the `cell` arrays in each JSON file. `replay/script.rs` resolves these sources; `profile-turn` selects one of these workloads explicitly.

| File | Classification |
|---|---|
| `chat-long-compacting.json` | ok; standard provider/tool replies, no guest cell |
| `chat-long-one-frame.json` | ok; standard provider/tool replies, no guest cell |
| `chat-short.json` | ok; standard provider/tool replies, no guest cell |
| `concurrent-16.json` | ok; standard provider/tool replies, no guest cell |
| `crash-resume.json` | ok; native tool/control calls and builtin strings |
| `prior-turn-probe.json` | ok; standard provider/tool replies, no guest cell |
| `py-codemode.json` | ok; native tool/control calls and builtin strings |
| `ts-codemode.json` | ok; native tool/control calls and builtin strings |
| `ts-record-sort-200k.json` | ok; native builtin sort and bounded full-key oracles |
| `warm-capabilities.json` | ok; standard provider/tool replies, no guest cell |

`scenarios/generate.py` creates 24 exploration workloads. Each file was inspected:

| File | Classification |
|---|---|
| `scenarios/analysis-py-heavy.json` | ok; native fixture tools, retained chunks/slices |
| `scenarios/analysis-py-small.json` | ok; native fixture tools, retained chunks/slices |
| `scenarios/analysis-py-typical.json` | ok; native fixture tools, retained chunks/slices |
| `scenarios/analysis-ts-heavy.json` | ok; native fixture tools, retained chunks/slices |
| `scenarios/analysis-ts-small.json` | ok; native fixture tools, retained chunks/slices |
| `scenarios/analysis-ts-typical.json` | ok; native fixture tools, retained chunks/slices |
| `scenarios/chat-heavy.json` | ok; standard tool or text replies, no guest cell |
| `scenarios/chat-small.json` | ok; standard tool or text replies, no guest cell |
| `scenarios/chat-typical.json` | ok; standard tool or text replies, no guest cell |
| `scenarios/coding-heavy.json` | ok; standard tool or text replies, no guest cell |
| `scenarios/coding-small.json` | ok; standard tool or text replies, no guest cell |
| `scenarios/coding-typical.json` | ok; standard tool or text replies, no guest cell |
| `scenarios/fanout-heavy.json` | ok; standard tool or text replies, no guest cell |
| `scenarios/fanout-small.json` | ok; standard tool or text replies, no guest cell |
| `scenarios/fanout-typical.json` | ok; standard tool or text replies, no guest cell |
| `scenarios/output-burst-heavy.json` | ok; standard tool or text replies, no guest cell |
| `scenarios/output-burst-small.json` | ok; standard tool or text replies, no guest cell |
| `scenarios/output-burst-typical.json` | ok; standard tool or text replies, no guest cell |
| `scenarios/reread-heavy.json` | ok; standard tool or text replies, no guest cell |
| `scenarios/reread-small.json` | ok; standard tool or text replies, no guest cell |
| `scenarios/reread-typical.json` | ok; standard tool or text replies, no guest cell |
| `scenarios/workflow-heavy.json` | ok; native fixtureSlow waits |
| `scenarios/workflow-small.json` | ok; native fixtureSlow waits |
| `scenarios/workflow-typical.json` | ok; native fixtureSlow waits |

## Entry-point closure

| Entry point | Guest source selected |
|---|---|
| `just perf-gate-instructions`, `perf-history-record` | `scripts/perf-gate-local.sh` then `perf-gates.py::instructions`: all compare pairs and the replay `budgets.json` subset. Source hashes/script digests prevent comparison across changed programs. |
| `just perf-gate-startup` | Worker spawn through Ready; no guest. |
| `just profile-cell`, `profile-cell-mem`, `profile-cell-diff` | Required file argument; no default example. `cell_profile.rs` / `examples/cell_mem.rs` execute caller source through the production registry. |
| `bin/cellpath.rs` | Required `--source` path; no default example. Executes caller source through the facade. |
| `just profile-turn` | Required workload name; replay corpus above. No default example cell. |
| `profile-runtime`, `profile_runtime_stack.py`, `profile_diff.py` runtime mode | Runtime provider profiles above. |
| `profile_heap.py`, `profile_paged_heap.py`, `profile_tasks.py` | Boundary/latency/growturn/replay families above; paged-heap defaults to objects-update; tasks uses concurrent-16. |
| `profile_kernel_cost.py`, `perf/kernel/measure.py`, `kland-gate6-paired.sh` | Gate-six programs above or explicitly supplied file. |
| `perf/kernel/embedded.py` | Eleven paired pure programs, automatically discovered. |
| `perf/kernel/workflows.py` | Two paired workflow programs with real host awaits. |

## Capability gaps and other lanes

No absent Lash native capability blocks these corrections. The missing peer CLI host adapters are a comparison limitation, not a reason to emulate tools in guest code. Native Python math/JSON imports are already implemented through the package registry; see `lash-ext-py-helpers/src/pure_modules.rs` and `lash-dialect-python/src/tests/pure.rs`.

FIG-7465 owns `json_transform.py`; FIG-7458 owns `sort_bench.*`; FIG-7441 owns `object_sort.*`. This commit edits none of them. Those findings are reported here so the audit does not silently certify the old JSON and sorting comparisons.

## Measurement and proof

Measurement contract: report changed-workload costs, not a runtime speedup. Same optimized artifact and host for old/new workflow source; cold-process hardware `perf stat -e instructions:u`, reversed before/after order in pair 2, one workflow invocation per operation, 8 GiB address-space cap. The Python array row uses a cold-process low/high slope at 1,000 and 100,000 trips, divided by ten reads per trip, with the same artifact and production registry. Preparation is amortized by the subtraction. The normal compare harness also ran for the changed array row. Gate-six bodies were extracted into the embedded runner's `result` wrapper; the runner supplies the single finish boundary. The original and corrected built-in gate-six programs also ran and retained their canonical result digests. Every output must match its independent expected checksum. Shared-host wall time is not claimed.

Two pairs per version, before/after then after/before. Arithmetic means; ratios are after/before. The changed source is the cause of the work difference, so none of these ratios is a runtime speedup claim. Cold counts are instructions per whole program invocation. The array slope is instructions per read. Full samples, source hashes, artifact hash and checksums are in [fig-7466.json](evidence/fig-7466.json).

| Program | Before instr/op | After instr/op | After / before |
|---|---:|---:|---:|
| workflow_iteration/ts | 933,592,465.000 | 1,006,797,079.000 | 1.078412 |
| workflow_iteration/py | 880,159,020.500 | 939,176,514.000 | 1.067053 |
| workflow_early_return/ts | 930,405,688.500 | 1,125,577,416.000 | 1.209771 |
| workflow_early_return/py | 875,895,791.500 | 917,296,214.000 | 1.047266 |
| heap_variable_concat/ts | 947,232,009.000 | 3,215,970,366.000 | 3.395124 |
| language_host_environment/ts | 1,115,749,717.500 | 1,115,738,019.500 | 0.999990 |
| array_read/py | 1,979.100 | 2,174.901 | 1.098934 |

The workflow full-result checksums match across both versions and dialects. `heap_variable_concat` and `language_host_environment` retain their existing result checksum. The array checksum deliberately changes from `55*n` to `45*n` and now matches TS. Native `concat` copies the accumulated array, while the old push loop mutated it in place; the 3.395x cold ratio is a workload correction.

The unfixed twin law produced Python 55,000 versus TS 45,000 at 1,000 trips. After the correction both produce 45,000. No test files were added or changed.

The history recipe now fetches origin/main and accepts a clean HEAD on its ancestry, including a landed commit overtaken by another lane. It still refuses dirty or unlanded commits. The behind-main recipe run and final validation will be retained in the lane result file and ticket comment after landing, since this check requires landed code.

Normal compare-harness confirmation, two low/high pairs after one warmup: Python array reads 1,999.101 to 2,194.901 instructions/read, after/before 1.097944. TS remains 1,050.301 instructions/read. Both corrected dialects return 45,000 and 4,500,000. No harness errors.

`kiln sync` left no tracked generated drift. The corrected optimized build passed. `kiln clippy` passed on retry after a report edit during the first run caused an input-size upload refusal; no lint failure occurred. The workflow runner, built-in corrected kernel programs and normal array comparison all executed successfully. No new tests were added.
