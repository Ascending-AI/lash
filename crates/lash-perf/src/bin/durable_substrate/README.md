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
timing has no threshold. Both dialects run in the law. The Python release-pin refusal discovered by
this benchmark was fixed by FIG-6186 before its final measurement. The dated
[measurement report](../../../../../docs/perf/declared-reads-2026-10-13.md)
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
