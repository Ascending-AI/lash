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
the allocation wrapper. It does not change product execution or stored formats.

Checkpoint cost has three independent populations (FIG-7230):

| Population | Per-turn facts | Meaning |
| --- | --- | --- |
| Bytes written | `pages_sealed`, `seal_bytes_published` | Physical page envelopes crossing from the worker to the broker's staging, including repeated publications; not deduplicated fresh SQLite bytes. |
| Work examined | `seal_nodes_visited`, `seal_rows_examined`, `seal_nodes_hashed`, `seal_boundary_hashes`, `seal_pages_read` | Traversal visits, examined rows, semantic page hash attempts, boundary hashes and logical page reads (including cache hits) during checkpoint projection or finished-cell carry. Visits count work at each traversal site, not distinct node identities. |
| Ownership retained | `ownership` on each turn and frame-change row | Distinct semantic pages/bytes reachable from authoritative roots, current-head live heap, lease pins, obsolete pages still pinned, and pages eligible for collection. |

`seal_attempts` counts entered checkpoint, carry and heap publication windows,
including nested windows and budget retries; it is a population witness, not a
count of committed roots. Counters are opt-in under `perf-witness` (the counted
worker's existing `testing` feature enables it). Production builds compile out
collection and the publication-byte summation. These facts change no guest
charge, parked format, effect, result or release identity.

The ownership census reads the probe's SQLite graph outside turn CPU,
allocation and instruction windows. It follows the root catalog and edge
relation with DAG identities deduplicated. Retirement holds schedule collection;
they are excluded as live owners. Other roots, including retained revisions and
leases awaiting reaping, remain pinning catalog roots; lease expiry can
remove read authority before reaping removes its pin. The live-heap denominator follows
the current session head and environment heads, not old execution rows.
Superseded checkpoint-root eligibility is separate from page eligibility: a
shared page stays pinned while any live root reaches it. A zero collectable
count can mean retirement already peeled the pages; it is not a deletion
receipt. The census covers semantic heap envelopes, not code packages, process
RSS or allocator capacity. Whole-command `perf stat` includes this observer's
work, so do not equate it with the sum of measured turn instructions.

Add `--growing-heap` to each of the three RLM geometry invocations to exercise a
heap that grows while the turn's change stays constant. Each cell appends one
`1` to `checkpointHeap`, without printing the list or changing the answer. The
first cell of each frame initializes a fresh list; continue-as's automatic
follow-on also appends once. The variable membership stays fixed. Run both the
fixed and growing workloads to obtain the six-case RLM matrix.

Every seal work and publication counter binds the existing absolute 1.05
window rule; none is divided by heap or held history size. Retention compares
pinned pages/live pages and pinned bytes/live bytes, allowing a growing live
heap while requiring its ownership overhead to stay bounded. Reset geometries
also compare absolute pinned pages, bytes and superseded pinned checkpoint
roots at equal frame ages. The report retains VM and ownership facts on both
sides of each frame change. Missing seal fields, missing ownership populations
or empty live heaps refuse an RLM result.

The additional exact laws are
`latency::history_gate::tests::checkpoint_gate_requires_work_and_bounds_retained_ownership`
and
`latency::history_ownership::tests::retired_checkpoint_releases_only_pages_without_a_live_referrer`.
The first rejects missing measurements, growing seal work with flat output, and
excess retention while allowing proportional live-heap growth. The second
keeps a retained control and a shared current page pinned while an obsolete
checkpoint and then its unreferenced private pages become collectable.

Broker `staged_bytes` counts live page envelopes in the measured worker service's
staging spools, excluding manifest scratch. It falls on pruning, settlement and
last-owner drop, including resident and spilled spools; cumulative write bytes
do not fall. The retention byte ratio includes both store pins and live staging,
and reset geometries additionally bind absolute staged bytes. The spool law is
`kernel::pages::spool_laws::retained_staging_bytes_follow_prune_settlement_and_last_owner_drop`
on `//crates/lash-vm-broker:lash-vm-broker__unit_test`. The counted history worker
uses `target/history-spools` under the probe's working directory.
