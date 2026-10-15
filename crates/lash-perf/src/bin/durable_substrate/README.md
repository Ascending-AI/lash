# durable-substrate

L12b's bench (FIG-5188): L12a's turn, resume, parked-process and concurrency
shapes, and the substrate's own cold-resume, snapshot, idle and store
measurements, on the durable substrate. Results and their discussion are in
L12b's comparison document in [`docs/perf/`](../../../../../docs/perf/), beside
L12a's baseline; the raw ledger is `docs/perf/durable-substrate-2026-10.jsonl`.

What is production and what is the bench's:

- **Production:** the node runner (`lash_core::runtime::durable::node::serve`),
  the session activation and phase runner, the round runner, cells (kernel
  documents, their snapshots saved by `lash_vm_broker::kernel::store`), the
  process activation, waits and keys, session close, and the SQLite and
  PostgreSQL stores, with `DurableSettings::default()`.
- **The bench's:** the `TurnServices`/`TurnDrive` (a scripted protocol and a
  model that answers at once), the `benchmark_echo` tool and the cell's
  `tools.ext_echo` operation (both `Once`, answering their arguments), and a process
  engine that pins and awaits keys. `recorder.rs` decorates the durable store
  to record each write transaction's label, latency and payload bytes; it
  forwards every call unchanged.

The substrate cases drive the durable substrate directly, bypassing facade
`LashSession::send()` and observation. Their measurements exclude the host's
tool registry, projections and event feed; facade wiring exists in
[`lash/src/session.rs`](../../../../lash/src/session.rs).

The `declared-reads` case below uses the production facade, registry,
RLM protocol and engine through `send()`; only its fixed model/provider
and projection handle factory belong to the benchmark.

## Cases

| Case | What it measures |
|---|---|
| `operations` | Six owning SQL windows on fresh fixed-prior-turn sessions; independent `--prior-turns`, `--cell-parks`, `--value-bytes` |
| `rounds-{1,5,20}` | L12a's N-round turn: one session per batch, one `benchmark_echo` per round |
| `concurrent-{1,10,100}` | L12a's concurrency: S sessions per batch, five rounds of three parallel calls |
| `resume-{1,5,20}` | A turn held at its model call after N rounds; its node stops; a fresh node resumes it |
| `prior-{0,10,100,300}` | The same cold resume after K committed turns on the session (H2) |
| `cell-{0,1024,16384}` | A cell awaiting `tools.ext_echo` ten times, keeping each answer padded by P bytes (H5) |
| `cell-fresh-16384` | `cell-16384` whose padding is a new text at each await |
| `parked-process` | A process parked on a key for `--park-seconds`, then resolved by a host; reports resolve-start and resolve-acknowledgement to resume latency without a correctness threshold |
| `process-waits-10` | A process resolved ten times while its owner holds it |
| `idle-{0,1000}` | What the deployment does for `--idle-seconds` with K processes waiting (H8) |
| `store` | S2's claim, fence, heartbeat/reap and wake against the real `DurableStore` |

Every batch's sessions are closed after the batch, outside its window, as
L12a's teardown did.

## Running

Build once, then run SQLite cases through `kiln run` and PostgreSQL cases
through `bench.py` under `kiln gate`, which gives each case a fresh private
PostgreSQL 18 cluster with L12a's durable settings. Every node runs its cells
on the `lash-vm-worker` that `--vm-worker` (or `LASH_VM_WORKER`) names: a build
output has no worker beside it. `kiln run` names the built worker; a direct
run of the binary names it itself. A session that parks before its turn
commits (a worker that does not start, among others) refuses the case at once
with the park's reason.

```sh
. ./env.sh
python3 tools/buck2/bootstrap_native_tools.py   # the pinned PostgreSQL tree
kiln build //crates/lash-perf:durable-substrate__bin //crates/lash-vm-worker:lash-vm-worker__bin \
  --materializations final --build-report .kiln/FIG-5188/build-report.json
B="$(python3 tools/buck2/outputs.py --report .kiln/FIG-5188/build-report.json \
  --label //crates/lash-perf:durable-substrate__bin --single)"
W="$(python3 tools/buck2/outputs.py --report .kiln/FIG-5188/build-report.json \
  --label //crates/lash-vm-worker:lash-vm-worker__bin --single)"

kiln run //crates/lash-perf:durable-substrate__bin -- --store sqlite \
  --sqlite-dir "$PWD/.kiln/FIG-5188/final/sqlite/db-a" --case rounds-1 --samples 10 \
  --out "$PWD/.kiln/FIG-5188/final/sqlite/sqlite.jsonl"

kiln gate lash <fork> -- python3 crates/lash-perf/src/bin/durable_substrate/bench.py \
  --binary "$B" --evidence-dir .kiln/FIG-5188/final/pg \
  --nodes 1 --case rounds-1 -- --samples 10 --vm-worker "$W"

python3 crates/lash-perf/src/bin/durable_substrate/report.py \
  --inputs docs/perf/durable-substrate-2026-10.jsonl
```

## Six-operation sweep

See [physical SQL windows](../../../../../docs/perf/instruments/sql-operation-windows.md)
for counter units, owner scope and graph-work definitions. The sweep records
actual CellEnd separately from quiet snapshots, store-installed bytes, raw samples,
load1 and CPU PSI. The PostgreSQL aggregate counters are separate and refuse
incomplete statistics. Use a fresh evidence directory on each invocation.

```sh
. ./env.sh
kiln build //crates/lash-perf:durable-substrate__bin \
  //crates/lash-vm-worker:lash-vm-worker__bin --materializations final \
  --build-report .kiln/FIG-5927/operation-build.json
B="$(python3 tools/buck2/outputs.py --report .kiln/FIG-5927/operation-build.json \
  --label //crates/lash-perf:durable-substrate__bin --single)"
export LASH_VM_WORKER="$(python3 tools/buck2/outputs.py --report .kiln/FIG-5927/operation-build.json \
  --label //crates/lash-vm-worker:lash-vm-worker__bin --single)"

for prior in 10 100 1000; do
  for parks in 1 10; do
    for bytes in 0 16384; do
      name="h${prior}-p${parks}-v${bytes}"
      (ulimit -v 8388608; /usr/bin/time -v "$B" --store sqlite \
        --sqlite-dir "$PWD/.kiln/FIG-5927/$name/sqlite" --case operations \
        --prior-turns "$prior" --cell-parks "$parks" --value-bytes "$bytes" \
        --samples 10 --out "$PWD/.kiln/FIG-5927/$name/sqlite.jsonl")
      (ulimit -v 8388608; /usr/bin/time -v kiln gate lash <fork> -- \
        python3 crates/lash-perf/src/bin/durable_substrate/bench.py \
        --binary "$B" --evidence-dir ".kiln/FIG-5927/$name/postgres" \
        --nodes 1 --case operations -- --prior-turns "$prior" \
        --cell-parks "$parks" --value-bytes "$bytes" --samples 10)
    done
  done
 done
```

The commands export the pinned worker runfile for direct binary invocations.
These functional instruments include observer overhead. Keep the time command's
peak RSS evidence and reject latency comparisons at load1 >= 8. Do not use a live
PostgreSQL database. This sweep is a measurement recipe, not an additional landing
gate; the small SQLite law pins the six owning boundaries deterministically.

## Durable commit baseline (FIG-5957)

SQLite performance cases use `Full`: exactly-once admission across power loss
requires every boundary commit to be durable before returning. PostgreSQL
runs require fsync and synchronous commit enabled; `bench.py` supplies both.
Hermetic law servers with flushes disabled certify logic only.

`--case counted-no-tool --case counted-one-tool --case counted-one-park`
runs 16 concurrent cold sessions per batch (`--samples` batches), excluding
session creation and close from the counted windows. The direct one-tool
fixture makes two model calls around one `Once` tool; the one-park cell also
makes two model calls, one `Once` tool outcome and one VM admission park.
Geometry is checked per session. Reports contain all physical SQLite write
transactions and writer service nanoseconds from the FIG-5933 instrument,
plus acceptance-to-terminal p50/p99. Stderr `durability-window … begin/end`
markers delimit matching fsync/fdatasync traces, including checkpoint syncs
that occur within the window. Counts include cold actor claims and wake work.
Latency's `fast` fixture retains its separate send-to-host-completion window.

Run under an 8 GiB address-space cap and retain peak RSS. Host load1 must be
below 8 for a qualified timing; traces and loaded-host percentiles are counts
and unqualified diagnostics, never a passing fast-gate timing certificate.

## Fifty dependent declared reads (FIG-6154)

`--case declared-reads` runs TypeScript and Python, traced and untraced,
with 32-byte and 16,384-byte answer bodies. Every answer has an ordinal prefix;
the next request carries the complete previous answer. Recorded `Repeatable`
and `Read` use the identical cell source, input/output schemas and provider
answer function. The projection control calls a host-seeded function through
its dialect's saved-function calling convention, reading a private fixed
handle. Its provider uses the same answer function. A fourth population reads,
records one `Once` write of that answer, and reads again.

The benchmark re-executes its own worker, installing both shipped dialects
and one pure fixture function that creates the projection handle. It changes
no shipped helper/library definition. The optional `--read-population` selects
an exact population such as `typescript-read-32-false`; omitting it runs all
32 populations. Selection leaves every assertion enabled.

Each sample is a fresh session in the case's database. Workers are prewarmed;
creation, projection seeding and completed deletion lie outside the measured
send-to-outcome window. Accepted owner transactions stop at `turn.commit`;
mail, node maintenance and the later session release are excluded. Facts count
successful appends before terminal pruning, not rows left at the end. Installed
bytes count actual new canonical graph payloads, including content deduplication;
SQL metadata/index/WAL bytes are separate units and are not inferred. Worker
pool counters give both IPC directions and bytes. The first read-plan commit
is timed separately; its `model.done+cell.snapshot+admit` receipt also includes
the fused model response and initial snapshot, so it is not an estimate of an
exclusive plan CPU cost. Trace admission/export uses the production OpenTelemetry
adapter with a local in-memory exporter and omitted content.

The maintained `READ-COST-MODEL` law checks exact owner labels and fact counts;
timing has no threshold. The cacheable law runs all 32 populations on fresh
SQLite memory stores with one shared worker service and two runtime threads.
Both dialects run in the law. The Python release-pin refusal discovered by
this benchmark was fixed by FIG-6186 before its final measurement. The dated
[measurement report](../../../../../docs/history/perf/declared-reads-2026-10-13.md)
records that gap.

Build once as above, then run a capped local SQLite matrix and the same
matrix through the private synchronous PostgreSQL gate. Use exact
`--read-population` flags to measure a dialect separately (the dated report
retains both dialects' command manifests). Default `--samples 10` reports
nearest-rank p50/p95/p99; choose the sample count explicitly. At fewer than
100 samples, p99 is the maximum. Retain `/usr/bin/time -v` peak RSS and host
load; wall timing is diagnostic under shared-host load.

```sh
(ulimit -v 8388608; /usr/bin/time -v "$B" --store sqlite \
  --sqlite-dir .kiln/FIG-6154/sqlite/db --case declared-reads --samples 20 \
  --vm-worker "$W" --out .kiln/FIG-6154/sqlite/results.jsonl)
(ulimit -v 8388608; /usr/bin/time -v kiln gate lash <fork> -- \
  python3 crates/lash-perf/src/bin/durable_substrate/bench.py --binary "$B" \
  --evidence-dir .kiln/FIG-6154/postgres --case declared-reads -- \
  --samples 20 --vm-worker "$W")
python3 crates/lash-perf/src/bin/durable_substrate/reads_report.py --inputs \
  .kiln/FIG-6154/sqlite/results.jsonl .kiln/FIG-6154/postgres/postgres-nodes1.jsonl
kiln test //crates/lash-perf:durable-substrate__bin__unit_test \
  --test_arg=--exact --test_arg=reads::tests::fifty_dependent_reads_obey_the_cost_model
```

## Retained heap baseline (FIG-6196)

One command runs six scenarios at H = 100, 1000, 5000, 10000 (100×), on the
production `LashSession::send()` path, the durable SQLite engine and an isolated
released worker. H counts entries/objects; for the string case it counts bytes,
using 256-byte units of the requested population: 25,600, 256,000, 1,280,000 and
2,560,000 bytes (also 100×). This puts immutable payloads above the fixed control
header overhead. Run with a fresh evidence directory:

```sh
. ./env.sh
(ulimit -v 8388608; /usr/bin/time -v kiln run //crates/lash-perf:durable-substrate__bin -- \
  --store sqlite --sqlite-dir "$PWD/.kiln/FIG-6196/baseline" \
  --case heap-scaling --out "$PWD/.kiln/FIG-6196/baseline.jsonl")
```

Stdout is a text table, including least-squares slopes of log count against log
H (the growth exponent). JSONL retains every park interval, including all three
reset intervals. The committed [before table](../../../fixtures/heap-baseline.txt)
names the main SHA measured with the whole-state runtime; the
[paged table](../../../fixtures/heap-baseline-paged.txt) is the persistent
heap's (FIG-6193), measured with `kiln run --config=optimized` at H = 100,
1000 and 5000. At H = 10,000 the objects-update sample does not finish within
the sample limit (FIG-6198 owns that). These are counts;
wall time is diagnostic, with no threshold. The fixture is historical evidence,
not a golden count oracle, and does not replace `kernel-baseline.json` or the
advisory `duration-trend-history.jsonl`.

Each sample uses its own fresh database, core and worker pool. Heap construction
ends at gate 0, outside the measurement. Each gate defers its outcome. The observer waits for a durable pending
completion and for every checked-out worker to return to the idle pool, then
supplies that external completion. Gate 1 closes the interval covering one
released-worker resume and the next committed park. Guest updates touch
one list entry or one retained object's field. The immutable-string case builds `"x".repeat(256 * population)` before measurement and reads
only length; the source/document itself stays small. Dropping removes one binding to H objects while retaining a
separate H-element list. The next-cell case carries the seeded state into a cell
that only awaits the gate. The reset case does the same one-entry operation
three times, with worker reset/release required in each interval. D and W are
fixed semantic changes/accesses. This is a worker reset, not a claim that the helper's
immutable preparation cache is cold or that its PID changed.

The existing performance witness owns the counters. The shared
`RecordingStores`/`Recorder` used by declared reads certifies exactly one accepted
owner park per measured interval (`committed_parks` in JSONL); every store call
is forwarded unchanged. `vm_steps` counts actual
machine statement steps without allocating or locking in the VM loop.
Graph encode counts every envelope actually produced,
before staging deduplication; graph decode counts every decoder envelope read,
including repeated reads and canonical checks. Decode's canonical verification
may itself encode, so encoded counts include that real work. Immutable encoding
cache hits correctly do not count newly produced envelopes.
`serialized_body_bytes` also counts the logical token buffers built before a
cache lookup, and `decoded_body_bytes` counts materialized logical tokens fed
to typed decoders. These expose whole-value work that envelope deduplication
can hide, especially for the immutable-string case. Nodes can stay constant
while their bytes grow. Graph counts sum the isolated worker's deltas and the broker/store process's existing collector, so host-side terminal reconstruction and canonical
checks remain visible too. The worker sends count deltas in existing progress
receipts; an ordinary worker sends no diagnostic field.

IPC counts actual encoded frames in both directions. SQL counts come from the
physical SQLite trace, with affected rows from the existing write witness.
`sql_expanded_bytes` counts expanded statement text, including quoted/hex bound
values, rather than logical payload or WAL bytes. The SQL scope is the isolated
core's complete interval, including its runner's background work and the completion observer's SQL. Buffer peaks
observe simultaneously live transport/read-ahead allocations and node byte
buffers at serialization/receive seams; they exclude the semantic heap,
allocator overhead, immutable caches and RSS. Keep `/usr/bin/time`'s independent
peak RSS beside the table. Neither a buffer counter nor SQL bytes alone proves
that traversal is bounded.

Page counters: `pages_loaded` counts pages the released worker's page port
read from its parent, `pages_sealed` the pages it sealed into the parent's
staging, `spool_bytes` the bytes the parent wrote to its activations' spools,
and `retirement_batches` the store's bounded retirement steps (the node's
retirement task runs them in the background, so they land in whichever
interval they run in).

The counted laws assert, over each scenario's measured interval at two or more
H, that a count's growth exponent stays at most 0.25: a park's sealed pages and
spooled bytes at a fixed change (list, objects, drop, resets), the pages a
cold resume loads at a fixed working set (list, objects, resets), the pages a
string length read loads, and the pages a no-op next cell loads and seals.
Counts only, never wall time. Every interval still requires worker
release/reset and exactly one owner park; the command-line run asserts the
laws over its populations, and the harness law runs all six scenarios at
H = 100 and 2,000 on fresh SQLite memory stores:

```sh
kiln test //crates/lash-perf:durable-substrate__bin__unit_test \
  --test_arg=heap::tests::six_scenarios_obey_the_paged_heaps_counted_laws \
  --test_arg=--exact
```
