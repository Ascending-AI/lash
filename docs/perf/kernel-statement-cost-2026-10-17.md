# Files changed

- `docs/perf/kernel-statement-cost-2026-10-17.md` records the matched statement-cost verdict, complete action durations, work identities, limitations and follow-up ownership.
- `docs/perf/README.md` links this report.

This is a measurement and documentation change. No production code, semantic/admission law, test assertion, manifest, generated file, batch membership, budget, stored shape or public API changes are retained. Temporary counter probes in two test-support files were restored byte-for-byte; the patch and restored hashes remain in the evidence archive. No test was deleted and no public cause variant was added.

Raw evidence is under the measuring fork's `.kiln/FIG-6318/`: build reports, scripts, source/artifact hashes, perf/resource records, work reports, test receipts, action logs/XML, the Clippy event receipt and the `attempt-2/` individual-member measurements. Build outputs are outside the evidence archive under `.buck2/FIG-6318/`. The archived round-2 fixtures were reused unchanged. The lane report is also recorded at `/workspace/notes/lash/tasks/lanes/tr-stmtcost.report.md`.

# The cause and why the fix is at the cause

**Sam accepted the measured 1.77× statement cost as “about pre-heap” for 1.0 on 2026-10-17, and FIG-6318 is Done.** The recorded ratio is **1.765989× pre-heap user instructions per kernel statement**; URI remains **2.697241×**. This is an explicit acceptance decision, not a new numerical threshold or a claim that the two revisions execute the same number of host instructions. [FIG-6318, Sam's option-A ruling](https://linear.app/ascending-ai/issue/FIG-6318) records that the pre-heap ratio criterion is closed and future kernel speed is judged against comparable embedded interpreters.

The original whole-batch action still has a **300.085-second timeout receipt**, and this report does not claim a whole-batch pass. All three isolated members subsequently passed: helper-plan independence **225.210 s**, helper differential **57.394 s**, document preparation **37.338 s**. The owner routed the whole-batch timeout separately to **`triage-7c5c`**. It is not repaired or hidden by this ticket's accepted statement-cost verdict.

This lane changes no runtime code and establishes no newly isolated heap cause to fix. Its matched arithmetic results, exact charges and statement counts survive across the two revisions. The cold-plan member is the largest action, but scalar statement volume does not explain all its lowering, preparation, native execution and seal/open cost. The projection below is explicitly a model, not causal attribution. No check or admission law was weakened.

# Proof results with executed-test counts

## Matched local instruction measurements

Current: `20100e460894d917a034a5513578ca153612573f`. Pre-heap: `3fc570b586a416657fcda9f88f1c09682d2b311d` (`af9233b05d^`), exactly round 2's baseline. Both were freshly built through Kiln with `--target-platforms //tools/buck2:profiling -c kiln.rust_profile=optimized --materializations final`. The rebuilt pre-heap `kernel_perf` and Test262 binaries have the **same SHA-256 as the archived round-2 binaries**: `f95d0e2adc92c3e10230699eb4e52ca8deeff05349e043dfbcc3505a6498f7e3` and `34f0b020fa8d1109e8e5bf651d3b46caeb747cb1773fb7392e0a26b92d15aba0`.

Five paired fresh-process samples per workload, alternating revision order each round, ran sequentially through `kiln gate lash tr-stmtcost -- python3 .kiln/FIG-6318/measure.py`. Every sample used `/usr/bin/time -v` and `perf stat -x ';' -e instructions:u,cycles:u`, under an 8 GiB address-space cap. CPU: AMD Ryzen 9 5950X, 32 logical CPUs; Linux 6.8.0-137-generic; perf 6.8.12; the same pinned rustc 1.98.1, opt-level 3, debug assertions off, retained symbols/frame pointers. Compiler/profile identities are retained with every kernel receipt.

The host was shared: sample-start load1 ranged **15.60–53.16**. The instruction counter ran **100.00%** of enabled time in every sample; raw enabled-runtime/running-fraction fields and cycles are retained. Peak local sample RSS was **107,528 KiB**. Local retired instructions are final evidence under the task's ruling; no quiet-host wall-time conclusion is claimed.

Cells are median [minimum–maximum], over five samples. Ratios divide medians; the paired micro ratio is also given below. Whole-process instruction totals include preparation and harness work; the scalar differential cancels its paired zero-iteration control.

| Workload / metric | Pre-heap | Current | Current / pre-heap |
| --- | ---: | ---: | ---: |
| Micro instructions / statement | 1,137.805 [1,137.800–1,137.837] | 2,009.352 [2,009.321–2,009.370] | 1.765989× |
| URI, billion instructions | 9.190947 [9.190802–9.192160] | 24.790199 [24.786308–24.790973] | 2.697241× |
| ID_Start, billion instructions † | 13.821267 [13.820769–13.821669] | 51.855162 [51.840108–51.859990] | 3.751839× |
| Callback heavy law, billion instructions | 16.982425 [16.980950–16.985520] | 31.525421 [31.524781–31.526716] | 1.856356× |
| Primitive-slice heavy law, billion instructions | 3.802055 [3.802052–3.802056] | 11.097887 [11.097879–11.097891] | 2.918918× |
| 120k checksum, billion instructions | 5.104196 [5.104193–5.104198] | 9.499025 [9.498665–9.499664] | 1.861023× |

| Workload | Pre-heap load1 range / peak RSS KiB | Current load1 range / peak RSS KiB |
| --- | ---: | ---: |
| micro | 15.89–53.16 / 80,384 | 15.89–53.16 / 98,304 |
| micro0 | 15.89–50.75 / 80,384 | 15.89–50.75 / 98,304 |
| checksum | 15.89–50.75 / 80,128 | 15.89–50.75 / 98,304 |
| uri | 15.74–50.75 / 41,308 | 15.74–50.75 / 71,324 |
| id_start | 15.74–49.56 / 53,084 | 15.60–49.56 / 107,528 |
| callbacks | 16.11–48.46 / 75,092 | 15.87–52.11 / 83,068 |
| slices | 15.80–53.16 / 37,012 | 15.80–53.16 / 41,504 |

**70/70 perf processes exited successfully; 40/40 Rust benchmark-law executions passed, 0 failed.** These comprise five URI, ID_Start, callbacks and slices laws per revision, plus 30 kernel benchmark processes. Two additional work-instrument invocations completed, each checking observed/plain execution for three programs. These benchmark repetitions measure the requested distributions; they are separate from the developer proof receipts below.

Micro source: `for (let i = 0; i < 12000; i++) { let value = i + 1; }`; control: the same body with zero iterations. Both revisions retain **60,007 / 7 statements**, **384,022 / 22 charge**, **zero heap insertions**, and **24,000 num.add / 12,001 num.lt** calls in the loop (control: one num.lt). Result and source hashes match across all samples and the archive. The formula is `(instructions(loop) - instructions(control)) / (3 * 60000)`; cap 1, `KERNEL_PERF_FIXED=1 KERNEL_PERF_SPLIT=0` gives initial, warmup and timed executions. Median paired current/pre ratio: **1.765982×**. Guest-body-iteration normalization is five times the per-statement value; no claim isolates one assignment from loop control.

The 120k checksum source is exactly `let checksum = 0; for (let i = 0; i < 120000; i++) checksum = checksum + ((i * 17) % 97);`. Both revisions retain **5,760,024 charge**, **600,008 statements**, zero insertions, and the same named-native counts (240,000 num.add; 120,001 num.lt; 120,000 each num.mul and num.rem_trunc). Its result is **Null**, as in round 2: it does not return the checksum binding. All three programs' complete work reports match after sorting named rows and retaining native identity hashes separately. The hashes differ across the historical revisions; all counted work, charges and zero residuals match (`audit.json`). Prepared checksum-run medians were 89.77 → 329.60 ms, advisory on this shared host.

URI uses the exact archived source `test/built-ins/decodeURIComponent/S15.1.3.2_A1.12_T1.js`; both revisions reach the same **45,000,000 Charge bound**. ID_Start uses `test/built-ins/RegExp/property-escapes/generated/ID_Start.js`. † **Its stopping points remain unequal:** pre-heap hits the charge bound; current raises `TS_LONE_SURROGATE_UNSUPPORTED`. Its ratio describes total selected-law cost, not matched guest throughput. The passing selection laws observe these recorded JavaScript failures; they are not JavaScript conformance passes. Both source hashes match the archive. The script derives each full selection-law name from that binary's partition population and refuses an empty selection.

Heavy selectors are `tests::array_callbacks::callback_methods_match_their_replaced_definitions` (11 helpers × 3 sequences, each compared across the two helper libraries) and `tests::guarded_text::primitive_slices_match_generic_bounds_holes_identity_and_utf16_failures` (**6,016 differential pairs** on each revision). Their guest corpora/assertions are unchanged across revisions; `heavy-fixture-diff.patch` records the heap-start/resume API plumbing differences. Their instruction cells measure each complete law, not a newly inferred statement count. No semantic/admission assertion was changed.

Relative to round 2's historical current medians, the scalar differential fell 8.82% and URI instructions fell 14.17%. These are combined-revision observations, not isolated savings credited to individual fixes.

## Unchanged TypeScript deadline and developer receipts

The measured tree's generated `test_batch` contains three integration binaries (`document_preparation`, `helper_differential`, `helper_plan_independence`); the old unit member is now six separate actions. Ran the atomic batch once, and the entire 257-case current unit population once through its owning target with every full test path explicitly selected under `--exact`. No repeat or raised timeout was used.

`kiln test` final receipts:

- Batch: **failed; executed=0, passed=0, failed=0**, one **TIMEOUT**, exit 124; no declared per-case XML. No completed member/case population is claimed for the timed-out action.
- Unit shards: **failed; executed=257, passed=256, failed=1**. Five action passes, one action failure, no action timeout. Both named historical heavy laws passed in this developer run as well as in the separate performance samples.

All per-action durations are retained in `actions.json` and both raw test reports. Execution seconds below are the action wrapper's elapsed interval; driver seconds also include runner overhead. These are deadline receipts, not quiet-host timing claims.

| Action | Status | Execution seconds | Driver seconds |
| --- | --- | ---: | ---: |
| test_batch | TIMEOUT | 300.085 | 300.904 |
| lash-dialect-typescript__unit_test__shard_1 | PASS | 164.568 | 165.549 |
| lash-dialect-typescript__unit_test__shard_2 | FAIL | 64.094 | 64.969 |
| lash-dialect-typescript__unit_test__shard_3 | PASS | 44.534 | 45.267 |
| lash-dialect-typescript__unit_test__shard_4 | PASS | 17.011 | 17.836 |
| lash-dialect-typescript__unit_test__shard_5 | PASS | 25.624 | 26.677 |
| lash-dialect-typescript__unit_test__shard_6 | PASS | 32.323 | 33.169 |

The 300-second budget and batch jobs=2 remain unchanged. The timeout prevented the batch from emitting its member XML; individual member durations and completed-case counts are unavailable. Logs/XML for all completed actions are copied into `action-results/`.

The unit failure is `tests::async_fn::a_function_in_a_tool_argument_is_not_data`, measured revision `crates/lash-dialect-typescript/src/tests/async_fn.rs:234`: expected `Raised("not_data")`, observed `Finished(FunctionValue(...))` for `() => 1`. This lane changed no source. It is recorded as a pre-existing measured-tree red and was not diagnosed, repaired, weakened or rerun.

Workspace `kiln clippy` **passed once** on the measurement-validation tree `69e43ade38e017ca46ea416aadaea1efc88205ef`. Its final receipt line is archived in `clippy.log`; the event receipt is copied as `clippy-events.pb.zst`. The diagnostic sources were restored exactly after measurement (`final-status.txt`). Publication adds documentation only.

No first-party remote exit-9, memory-scale retry or budget change occurred. No stored shape, export, helper definition, kernel library or production encoding changed in this lane, so schema, facade and pinned-image gates do not apply. No law was added or changed and no fix was implemented; a new red/green witness does not apply. The first attempt's clean final pull/rebase had no conflicts; the task forbids rerunning successful measurements/tests just because main moved. The owner separately authorized the three individual member measurements recorded below. Results above certify only the pinned measured revision, not the later fast-forwarded HEAD.

## Individual batch members

The whole-batch timeout lacked member durations, so each member was measured once in isolation. No sharding, batch modification, budget increase or runtime optimization was made. Sam's subsequent acceptance and follow-up routing are recorded above and below.

On `69e43ade38e017ca46ea416aadaea1efc88205ef`, built each member by its exact public label, then invoked each exact owning label **once through `kiln test`**, with all its full law names explicitly selected under `--exact --nocapture --no-test-cache`. No member was rerun. This is the ordinary developer/debug profile on the remote executor, not the optimized local profile used for the earlier paired instruction ratios. The build, commands, raw reports, per-case XML and logs are under `.kiln/FIG-6318/attempt-2/`.

Temporary diagnostic probes exposed existing helper-runner work counts at each completed segment; the gate-six trace law additionally enabled the existing work observer and printed one record per program/slice geometry. Assertions, corpus inputs, semantic/admission decisions, batch membership and budgets were untouched. The two source files were restored byte-for-byte to HEAD after execution; the exact diagnostic patch and restored hashes are retained. Timings below describe these counter-enabled diagnostic executions and include their observer/logging overhead. The start/allocation laws were deliberately left uninstrumented so their allocation accounting was not perturbed.

| Rank | Exact member label (`//crates/lash-dialect-typescript:` prefix) | Execution s | Driver s | Executed / passed / failed | Counted kernel statements |
| ---: | --- | ---: | ---: | ---: | ---: |
| 1 | `helper_plan_independence__test` | 225.210 | 226.119 | 3 / 3 / 0 | 217,186 |
| 2 | `helper_differential__test` | 57.394 | 58.392 | 8 / 8 / 0 | 163,119 |
| 3 | `document_preparation__test` | 37.338 | 38.371 | 3 / 3 / 0 | 458,397 † |

**Member-isolation proof: 3 member actions passed; executed=14, passed=14, failed=0.** Each final `kiln: test passed` receipt names the corresponding report. Every individual member completed within the unchanged 300-second bound.

† Document preparation's **458,397** statements cover the gate-six trace law's **29 programs × 3 slice geometries = 87 executions**. They are a lower bound for the whole member; its JSON allocation law is not work-instrumented, and the start law runs no guest statements. Its start/allocation assertions also passed unchanged (67 baseline-start allocations; host JSON generic 50,633 and plain 10,242, against the existing 81,000 ceilings).

Helper differential counted **163,119 statements in 1,642 completed machine runs**. Its corpus reports **1,357 TypeScript + 282 Python cases, zero mismatches**, reaching 1,052 and 203 helpers respectively; lowering refusals run no machine. The additional host laws are included. The 4,096-draw random law alone accounts for 53,255 statements.

Helper-plan independence counted **217,186 statements**. It reports **676 even + 674 odd TypeScript comparisons and 282 Python comparisons = 1,632 pairs**, with a normal/prepared execution and an alternating-layout execution per pair: **3,264 executions**. Its 4,825 completed work segments imply **1,561 cold seal/reopen continuations** in the alternating path. These are real extra workloads that the scalar micro does not exercise.

**Which member carries the budget:** helper-plan independence is dominant at **225.210 s (75.1% of 300 s)**. With the committed order `document_preparation`, `helper_differential`, `helper_plan_independence` and jobs=2, a schedule using these isolated durations starts plan independence when document preparation finishes at 37.338 s; the modeled critical path is **37.338 + 225.210 = 262.549 s**, leaving **37.451 s**. Helper differential's 57.394 s runs alongside it and is not on that modeled critical path. The summed member time is 319.943 s, not the jobs=2 makespan. Plan independence is therefore the member most exposed to contention, cold-resume/preparation cost and batch headroom. **This model is not a new batch run or a 300-second batch pass.** The earlier 300.085 s whole-batch timeout remains the actual batch receipt; its earlier revision and simultaneous execution prevent inferring a causal explanation solely from these new isolated samples.

**Instruction/heap attribution:** direct member `instructions:u` were not collected: these exact-label executions ran remotely, whose receipts expose no hardware retired-instruction counter. Doing so would require an additional local member execution or changing the execution wrapper; neither was done under the owner's once-per-member/minimal scope. The earlier 70 local perf samples and 100% counter-running fractions remain the actual hardware evidence. The following is only the requested scalar-cost projection, **not measured member instructions**:

`projected pre instructions = counted statements × 1,137.805256`

`projected current instructions = projected pre instructions × 1.765989255 = counted statements × 2,009.351856`

| Member | Projected pre billion instructions | Projected current billion instructions | Projected extra billion instructions |
| --- | ---: | ---: | ---: |
| `helper_plan_independence` | 0.247115 | 0.436403 | 0.189288 |
| `helper_differential` | 0.185598 | 0.327763 | 0.142166 |
| `document_preparation` (gate law only) | 0.521567 | 0.921081 | 0.399514 |

The measured scalar penalty is **76.6% over pre-heap for the same scalar statement geometry**, so it is a plausible common VM contribution. It does **not** establish that the members' total cost is heap-attributable. Plan independence takes **6.03×** document preparation's time while executing **less than half** its counted statements (and document preparation's count is only a lower bound). Lowering, preparation, native work, counter-report construction, seal/open and cold reads are outside a scalar-statement-only cost model. The members use debug code, while the scalar coefficients come from optimized code. Consequently neither the table nor `time / 1.765989` is a valid causal estimate of their pre-heap wall time. The evidence routes the next profile to **helper-plan independence's prepare/run/seal/open phases**, retaining its layout/admission equivalence law; it does not justify sharding, changing a budget, weakening a law, or assigning an unmeasured percentage to heap work.

The old `not_data` unit red is **resolved by main commit `33b220a157`**; that commit is an ancestor of the member-measurement tree. It is no longer an unresolved item and was not rerun or edited here. After restoring the two diagnostic sources, the measured tree was exactly the previously linted tree. No whole-batch run or repeated lint was performed for documentation publication. No compile memory retry applies.

## Documentation publication

The measurement follow-up was complete before publication. The report was rebased without conflicts onto `32c101b06ac42549cf0c7b611372339f350cf7a2` and no runtime source changed in this commit. Per the no-repeat rule and the owner's explicit instruction, the batch, individual members and prior Clippy proof were not rerun to publish the report. No new test execution is claimed for publication. Formatting is run through `kiln fmt` immediately before the documentation commit. No schema, facade, pinned-image or backend gate applies to this documentation-only change.

# Unresolved

There is no unresolved decision blocking this accepted measurement deliverable. **FIG-6318 is Done per Sam's option-A ruling**, and the completed unit is ready to submit. The recorded limitations and remaining work retain their own owners:

- **Kernel speed:** [FIG-6675](https://linear.app/ascending-ai/issue/FIG-6675) replaces the internal pre-heap ratio as the yardstick. It owns the committed same-program comparison of Lash TypeScript with QuickJS and Lash Python with Monty, reference Node/CPython rows, equal-result checks, pinned engines, paired instruction counts and cold-start/RSS evidence. Lash lower/compile/run phase attribution belongs there; it does not authorize a runtime optimization in this report.
- **Product critical path:** [FIG-3843](https://linear.app/ascending-ai/issue/FIG-3843) owns send-to-completion latency qualification and determines whether kernel work is on that critical path. This report makes no end-to-end latency claim.
- **Whole-batch timeout:** the owner assigned its separate cause to **`triage-7c5c`**. The historical action timed out at 300 seconds; isolated member passes and the 262.549-second scheduling model are not a concurrent batch-pass receipt. The batch was not rerun for this publication, as directed.
- **Measurement limits:** native instruction counts for the remotely run members are unavailable; their scalar projections are not hardware observations. Document preparation's work count covers the gate-six law only. ID_Start has unequal stopping points. Quiet-host wall time remains unmeasured and unclaimed.

Sam's acceptance is recorded rather than inferred. No additional ratio decision, code fix or budget change is requested in this lane.
