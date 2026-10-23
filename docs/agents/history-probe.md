# History component probe

The developer probe checks the FIG-7211 held-frame rule in standard and RLM
protocols. Build both the probe and its worker once:

```sh
kiln build //crates/lash-perf:lash-perf__bin \
  //crates/lash-vm-worker:lash-vm-worker__bin --config=optimized \
  --materializations final --build-report .kiln/history-build.json
```

Locate the binaries with `python3 tools/buck2/outputs.py --report
.kiln/history-build.json --label <label> --single`. Set `LASH_VM_WORKER` to the
worker path. Through `kiln gate lash <fork> -- <script>`, run this matrix once:

```sh
for protocol in standard rlm; do
  for geometry in unbroken compact continue-as; do
    if [ "$protocol:$geometry" = standard:continue-as ]; then continue; fi
    args=()
    case "$geometry" in
      compact) args=(--compact-every 100) ;;
      continue-as) args=(--continue-as-every 100) ;;
    esac
    name="$protocol-$geometry"
    cat /proc/loadavg > ".kiln/$name.load-before"
    (ulimit -v 8388608
     /usr/bin/time -v -o ".kiln/$name.time" \
       perf stat -e instructions:u -o ".kiln/$name.perf" \
       "$probe" history --protocol "$protocol" --turns 1000 --check \
       "${args[@]}" --out ".kiln/$name.json" --store-dir "target/history/$name")
    result=$?
    echo "$result" > ".kiln/$name.exit"
    cat /proc/loadavg > ".kiln/$name.load-after"
  done
done
```

Use Bash, define `probe` as the resolved executable, and keep collecting after
exit 1: the report is written before a growth verdict fails. Each store directory
is emptied first; use a fresh path per geometry. Archive source SHA and patch,
executable hashes, load1, peak RSS and paired instructions. Do not claim
quiet-host wall time from a loaded host.

`standard` is the default: each host input is the same `history turn`, followed
by one fixed text answer. `rlm` uses the TypeScript cell channel. Every host turn
has one provider call and one cell: await `tools.benchmarkEcho` with fixed
`value` and `ordinal`, sum three fixed order amounts, print the checked total and
receipt value, and `await control.finish` with the total and receipt value. The
same variable names are reused, so the fixture does not manufacture growing
binding membership. The RLM control and tool effects, kernel parks, worker CPU
and page work are real; no model or network delay is simulated.

Only RLM admits `--continue-as-every K`; it conflicts with `--compact-every`.
Every K host turns, except the final turn, the cell ends with
`control.continueAs` instead of `finish`. Its fixed task hands over the order
audit. Its seed contains a 3,072-byte ASCII reconciliation memo and the fixed
receipt, plus `history[history.length - 1]` from projected history as concrete seed data in
`handover_request`. The selected item is always the current fixed host input;
its id varies at fixed encoded length. The session's own engine mails and
runs the task. The probe attaches to that follow-on and waits for its answer
before the next host input. Thus 1,000 host inputs with K=100 produce nine
switches and 1,009 code-mode turns. This necessary extra turn is reported;
ordinary text geometry and host input remain fixed. Keep the literal
`"history turn"` unchanged in before/after comparisons: on the measured baseline
it exposes the raw-source history-demand defect, whose replacement is FIG-7243.

A switch row meters the whole switch/follow-on cycle. Its
`switch_root_observed_cpu_ns` also cuts parent CPU at the observation of the
switching input's outcome; this is an observer boundary, so an asynchronously
started follow-on may already have prepared its request. Do not call that cut
exclusive VM execution time for the switching turn. Each row keeps all actual
request sizes and frame ids; the first request in every frame also retains its
message array and initial instructions for independent seed/task accounting. The switch request is the
last held request of the old frame; the task request is the first held request
of the new frame. Report both alongside adjacent sample costs.

The memo's input size is not its prompt contribution. Held bytes are UTF-8 JSON
bytes of the actual provider request's message array, including structural
overhead and whatever seed/task projection that array contains. Held messages
are its length. Neither metric counts resident capacity, old durable frames,
worker heap pages or hidden seed bytes that the model never receives. Report
seed state bytes and actual sent seed bytes separately, using retained reset
requests; do not subtract the seed from the held-state denominator.

The fixture installs the standard-compaction plugin for every geometry, with
automatic pressure compaction and pruning disabled, and a discard trace sink
with omitted content. Real trace records are prepared and delivered, then
released. Administrative compaction happens outside turn windows. Continue-as
and its follow-on happen inside the switching sample.

CPU attribution is exclusive thread CPU during actual future polls and
synchronous scopes. Pending, completion, cancellation and unwind return the
worker's attribution. Components are request assembly, provider lowering/send,
durable commit/SQLite service, acknowledged owner state, plugin/tool work,
trace/observer work and residual parent-process CPU. Component CPU plus explicit
size-measurement CPU conserves parent CPU. Rust allocation counts and positive
reallocation bytes exclude SQLite's C heap and worker-process allocations.
Measurement serialization and request capture run under the measurement scope.
Whole-command instructions include children; per-turn inherited instructions
corroborate the component costs without assigning overlapping spans counters.

RLM rows additionally project cumulative worker-pool work into per-turn deltas:
VM CPU, steps, heap/page/encoding work, IPC bytes and cell executions. Membership
comes from `WorkerWork`; peak/capacity fields are excluded from differencing.
These are separate from parent components. Every cumulative work metric binds
the 1.05 growth rule. Missing VM populations refuse a result.

The gate compares medians of 21-turn windows. Unbroken runs use 10–30 and
980–1000: request build/send divide work by held bytes, and all other components
and SQL families compare absolute work. Reset geometries use 10–30 and 910–930
at K=100, comparing absolute work. Each component checks CPU, allocations,
allocated bytes and SQL statements/VM steps/returned rows; every SQL family also
binds separately. Every ratio must be ≤1.05. Cold unseeded versus seeded
intercepts remain visible; report them separately from growth across equal
seeded frames. Do not waive a failing ratio or attribute a CPU-only difference
to a state-dependent cause without evidence. The JSON retains each sawtooth,
per-request reset sizes, every switch sample and SQL-family counters.

Failed turns, incomplete windows and missing scheduled resets are refusals.
The exact laws are
`latency::history_gate::tests::linear_non_history_work_fails_the_held_state_gate`,
`latency::history_gate::tests::reset_gate_rejects_vm_and_sql_growth_with_seeded_history`,
and `latency::history::tests::continue_as_requires_rlm_and_excludes_compaction`.
The first injects actual turn-proportional CPU and proves rejection; the second
injects VM and SQL work growth even with held seed bytes; the third enforces the
five permitted protocol/geometry combinations.

This probe is outside merge-path performance gates. Instrumentation is behind
the non-default `perf-witness` feature; the developer executable alone installs
the allocation wrapper. It does not change product execution, formats or host
APIs.
