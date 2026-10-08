# durable-substrate

L12b's bench (FIG-5188): L12a's turn, resume, parked-process and concurrency
shapes, and the substrate's own cold-resume, snapshot, idle and store
measurements, on the durable substrate. Results and their discussion are in
L12b's comparison document in [`docs/perf/`](../../../../../docs/perf/), beside
L12a's baseline; the raw ledger is `docs/perf/durable-substrate-2026-10.jsonl`.

What is production and what is the bench's:

- **Production:** the node runner (`lash_core::runtime::durable::node::serve`),
  the session activation and phase runner, the round runner, cells
  (`lash_vm_broker::cell::run_cell`), the process activation, waits and keys,
  session close, and the SQLite and PostgreSQL stores, with
  `DurableSettings::default()`.
- **The bench's:** the `TurnServices`/`TurnDrive` (a scripted protocol and a
  model that answers at once), the `benchmark_echo` tool and the cell's
  `ext.echo` operation (both `Once`, answering their arguments), and a process
  engine that pins and awaits keys. `recorder.rs` decorates the durable store
  to record each write transaction's label, latency and payload bytes; it
  forwards every call unchanged.

This benchmark drives the durable substrate directly, bypassing facade
`LashSession::send()` and observation. Its measurements exclude the host's
tool registry, projections and event feed; facade wiring exists in
[`lash/src/session.rs`](../../../../lash/src/session.rs).

## Cases

| Case | What it measures |
|---|---|
| `rounds-{1,5,20}` | L12a's N-round turn: one session per batch, one `benchmark_echo` per round |
| `concurrent-{1,10,100}` | L12a's concurrency: S sessions per batch, five rounds of three parallel calls |
| `resume-{1,5,20}` | A turn held at its model call after N rounds; its node stops; a fresh node resumes it |
| `prior-{0,10,100,300}` | The same cold resume after K committed turns on the session (H2) |
| `cell-{0,1024,16384}` | A cell awaiting `ext.echo` ten times, keeping each answer padded by P bytes (H5) |
| `parked-process` | A process parked on a key for `--park-seconds`, then resolved by a host |
| `process-waits-10` | A process resolved ten times while its owner holds it |
| `idle-{0,1000}` | What the deployment does for `--idle-seconds` with K processes waiting (H8) |
| `store` | S2's claim, fence, heartbeat/reap and wake against the real `DurableStore` |

Every batch's sessions are closed after the batch, outside its window, as
L12a's teardown did.

## Running

Build once, then run SQLite cases through `kiln run` and PostgreSQL cases
through `bench.py` under `kiln gate`, which gives each case a fresh private
PostgreSQL 18 cluster with L12a's durable settings:

```sh
. ./env.sh
python3 tools/buck2/bootstrap_native_tools.py   # the pinned PostgreSQL tree
kiln build //crates/lash-perf:durable-substrate__bin --materializations final \
  --build-report .kiln/FIG-5188/build-report.json
cp "$(python3 tools/buck2/outputs.py --report .kiln/FIG-5188/build-report.json \
  --label //crates/lash-perf:durable-substrate__bin --single)" .kiln/FIG-5188/bin/durable_substrate

kiln run //crates/lash-perf:durable-substrate__bin -- --store sqlite \
  --sqlite-dir "$PWD/.kiln/FIG-5188/final/sqlite/db-a" --case rounds-1 --samples 10 \
  --out "$PWD/.kiln/FIG-5188/final/sqlite/sqlite.jsonl"

kiln gate lash <fork> -- python3 crates/lash-perf/src/bin/durable_substrate/bench.py \
  --binary .kiln/FIG-5188/bin/durable_substrate --evidence-dir .kiln/FIG-5188/final/pg \
  --nodes 1 --case rounds-1 -- --samples 10

python3 crates/lash-perf/src/bin/durable_substrate/report.py \
  --inputs docs/perf/durable-substrate-2026-10.jsonl
```
