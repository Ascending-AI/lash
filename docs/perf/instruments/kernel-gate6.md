# Kernel gate 6 receipts and history

For the maintained external kernel speed yardstick, use
[the QuickJS/Monty comparison](kernel-vs-embedded.md) and its
[recorded results](../kernel-vs-embedded.md) (FIG-6675). The pre-heap ratio is
accepted for 1.0; these receipts remain useful for internal change comparisons.

`kernel_perf` measures TypeScript workloads on the in-process kernel with a
scripted host. Lowering, registry assembly, preparation, the initial run and
warmups precede the timed window. Each raw sample names its iteration, run and
start-plus-run nanoseconds, allocation and reallocation call counts, and charge.
Machine destruction and receipt construction follow the window. The receipt
names the source SHA256, executable SHA256, actual build compiler and flags,
allocator, fixed/adaptive iteration mode, split state and `observe` (`nothing`,
`work` or `context`). `result_sha256` hashes the result Datum's compact UTF-8
JSON with recursively sorted object keys; ordered collections and typed numeric
spellings are preserved. Hashing happens outside the measured windows. Means remain
labelled diagnostic means. `run_calls` counts charge-sliced calls, not operations
or exclusive CPU. `split_*_wall_share` columns are diagnostic shares of the
charge-sliced run with exports between steps, which changes dispatch geometry
and perturbs execution. They are neither exact inclusive CPU shares nor shares
of the ordinary unsplit run. Split and observed runs cannot enter ordinary
comparison or timing history; the maintained caller rejects a nonempty
`KERNEL_PERF_OBSERVE` before starting a process.

Unknown, empty, partially matched and refused selections exit nonzero. The
maintained caller requires exactly one complete row per selected process and
retains stdout, stderr, GNU time resources, load and every raw iteration. It also
requires identical counts across the fixed warm population. A refused process
cannot append an incomplete invocation to history.

Build through Kiln, then resolve paths from its build report:

```sh
kiln build --config=optimized --materializations=final \
  --build-report "$E/build.json" \
  //crates/lash-dialect-typescript:kernel_perf__example \
  //crates/lash-perf:lash-perf__bin
kernel="$(python3 tools/buck2/outputs.py --report "$E/build.json" --label //crates/lash-dialect-typescript:kernel_perf__example --single)"
reader="$(python3 tools/buck2/outputs.py --report "$E/build.json" --label //crates/lash-perf:lash-perf__bin --single)"
(ulimit -v 8388608; /usr/bin/time -v python3 scripts/perf/kernel/measure.py \
  --kernel "$kernel" --reader "$reader" --record-baseline \
  --out "$E/baseline" --history "$E/kernel-history.jsonl" \
  --iterations 3 --rounds 3 baseline image_host_environment heap_comprehension_build)
(ulimit -v 8388608; /usr/bin/time -v bash scripts/perf/kernel/kland-gate6-paired.sh \
  --kernel "$kernel" --reader "$reader" --baseline "$E/baseline/receipt.json" \
  --out "$E/current" --history "$E/kernel-history.jsonl" \
  --iterations 3 --rounds 3 baseline image_host_environment heap_comprehension_build)
```

This produces 18 independent processes for the three-workload A/A experiment.
Change the current executable for a change comparison. Pairs match recorded
kernel rounds; these are temporally separate processes, not simultaneous runs.
`summarize.py <baseline> <current>` reproduces the table offline. The committed
`crates/lash-perf/fixtures/kernel-baseline.json` records all 29 workloads on the
implementation host. It is an archived example, not a universal timing budget;
it predates result/Observe identity and is refused by current readers. Record
a fresh local baseline to compare current receipts.
A different host, workload, geometry, compiler, allocator or build configuration
refuses comparison; record an explicit local baseline instead. Artifact hashes
are evidence and can differ when comparing changes. Missing identities, result
digest mismatches and zero-sample populations refuse before comparison or
history mutation, including cached history. Charge or allocation movement
appears as an integer delta.

The existing Performance workflow records all workloads, retains receipts and
feeds `lash-perf kernel-history` before saving the existing history cache. Its
log and step summary are the maintained readers, reviewed by the person running
the manual Performance workflow. One invocation contributes one observation
per workload, pooling its raw rounds; three rounds do not count as three history
runs. `kernel_run_ms` is run-only, and `total_ms` is start-plus-run, not process
startup. The existing >50%/five-consecutive-run detector warns advisory, retains
its detection witness, and uses the committed level-shift acknowledgements.
Integer counts retain their own units and are printed beside timing history,
including total allocations across timed samples. They never become milliseconds
or a frozen allocation ceiling.

Host identities partition history. Fresh hosted runners may never accumulate
five comparable observations; the reader reports insufficient data and refuses
cross-host comparisons. Sustained-regression decisions need repeated invocations
on the same identified host. Cache loss also loses observations. This workflow
provides a reader and artifacts, not quiet-host certification or a notification
service, and no shared-runner wall-time gate is added.
