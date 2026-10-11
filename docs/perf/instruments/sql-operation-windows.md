# Physical SQL operation windows

`lash::tracing::sql::collect(operation_id, backend, future)`
returns the future's result and a physical SQL receipt. Use `"sqlite"` or
`"postgres"` and a joinable session/turn/record operation ID. The window starts
when the collector is first polled and ends at its completion snapshot. It follows
the caller task onto SQLite's connection worker. Spawned background tasks and
setup outside the future are excluded. Nested windows contribute to their parent;
do not sum a parent receipt with its children. These are functional diagnostics,
with observer overhead, never timing baselines or automatic regression ceilings.

A construction-time `StoreObserver::with_sql_sink` receives durable summaries on
success, error and cancellation on both stores. Inject it through
`SqliteStoreSetOptions::observer` or `PostgresStorage::connect`. Ordinary metric
observers also enable the `physical store SQL operation` debug record, requiring
a host tracing subscriber. Owner commits include the retained actor key, epoch
and input state revision; an enclosing caller window supplies `parent_operation`.
These IDs are receipt correlation data, not metric attributes. A cancelled
summary is partial: SQLite may finish accepted worker work after the caller drops.
The host owns sink cost and retention. Read and other API windows can be collected
explicitly around any store operation; enabling core telemetry does not inject a
store observer retroactively.

| Receipt field | Quantity, unit and statistic in this operation window |
|---|---|
| `statements` (total and per shape) | Sum of SQLite statement starts or PostgreSQL executor/control calls, including explicit transaction envelopes. Preparation, pool validation pings and protocol messages are excluded; a PostgreSQL multi-command call counts once. Implicit driver rollback/pool cleanup outside the caller window is excluded. This is not a network-round-trip count. |
| `rows_returned` (total and per shape) | Sum of SQLite ROW trace events or `PgRow` values returned through SQLx. A scalar count returns one row. Rows affected by a write and server rows scanned are separate quantities. |
| `rows_affected` (total and per shape) | Actual SQLite `sqlite3_total_changes64` deltas, including trigger/FK writes. PostgreSQL SQLx command-completion rows, including SELECT rows; an outer command does not expose hidden CTE/trigger mutations. Consult `rows_affected_unit`; these are backend quantities, not a cross-backend mutation oracle. |
| `bound_bytes` (total and per shape) | SQLite UTF-8 bytes of bound literals in expanded SQL, including quoting, blob hex and repeated occurrences. PostgreSQL bytes appended by the real binary parameter encoder, excluding protocol framing. Consult `bound_bytes_unit`; the units differ. PostgreSQL query builders capture binds without an extra encode. |
| `unmeasured_bound_statements` | Calls whose bind spelling/encoding could not be measured (SQLite expansion failed, or a PostgreSQL query was constructed before a scope without `perf-witness`). Their payload is unknown; `bound_bytes` remains the measured sum. The complete matrix refuses these windows. |
| `result_bytes` (total and per shape) | SQLite text/blob payload bytes, eight bytes per integer/float, NULL zero. PostgreSQL actual returned column-format payload bytes, NULL zero. Neither includes row or protocol framing. |
| `newly_installed_graph_bytes` | Canonical graph-node bytes actually inserted by the store in this attempt. A duplicate insert contributes zero; an insertion after GC contributes again. Only a successful commit makes these durable; error/cancellation receipts describe attempted work. |
| `sqlite_work.vm_steps` | Sum of SQLite VM instructions, from `StatementStatus::VmStep`, measured per execution by start/end differences, including cached statements. These are neither Lash VM instructions nor CPU cycles. |
| `sqlite_work.fullscan_steps` | Sum of `FullscanStep` advances during full table scans. **Not every row scanned**: indexed access does not contribute every visited row. |
| `sqlite_work.sorts` | Sum of `Sort` operations. |
| `sqlite_work.autoindex_rows` | Sum of rows inserted into automatic indexes (`AutoIndex`). |
| `sqlite_work.reprepares` | Sum of automatic recompilations (`RePrepare`). |
| `elapsed_nanos` | Elapsed caller-window wall time in nanoseconds, including queueing and observation, measured at the snapshot. It is not SQL server time. |
| `configured_shape_limit` | Configured upper bound of 64 retained normalized SQL shapes; not measured cardinality. |
| `unretained_shape_statements` | Sum of calls whose shape could not be retained after the bound. Total statements, rows and work still include them. The shape map's length is retained cardinality, not total unique shapes. |

Shape keys fold whitespace/case, remove comments and replace literals and bind
positions; bound values are discarded before receipt retention. SQLite briefly
expands SQL to measure literal spelling, then frees it; values never enter the shape map. SQL over 4096 bytes
and dollar-quoted SQL use an opaque key. PostgreSQL `sqlite_work` is `null`,
meaning server work is unknown. This instrument does not enable or read
`pg_stat_statements` and adds no SQL queries. Existing PostgreSQL returned-column
byte and lock-statement-elapsed instruments retain their distinct definitions,
as does the eight-trip `model.start` law. Logical commit bytes, domain writes,
graph nodes and history page output remain separate from the physical receipt.

Run the functional fixtures by exact selectors; the PostgreSQL unit target owns
its hermetic private server:

```sh
kiln test //crates/lash-sqlite-store:lash-sqlite-store__unit_test \
  --test_arg=observed_sql::tests::sql_work_receipts_cover_two_history_sizes \
  --test_arg=--exact --test_arg=--nocapture
kiln test //crates/lash-postgres-store:lash-postgres-store__unit_test \
  --test_arg=observed_sql::tests::sql_work_receipts_cover_two_history_sizes \
  --test_arg=--exact --test_arg=--nocapture
```

Each prints ten `SQL_WORK` JSON receipts: commit, a two-node history page, current
window load, process registration and wake at stored-message histories of two and
eight messages. One extra message is committed before each read. The fixture opens
real SQL stores, excludes preparation/setup, and runs no background node. Its laws
require shape sums and returned-row sums to agree with each window's totals.
The exact known-query laws additionally pin one call/one normalized shape and
returned rows; SQLite repeats its cached query after a larger window to prove
previous executions do not enter its work counters. `SQL-OWNER` exercises each
backend's real owner commit and requires the summary's fence and parent identity.

## Complete six-operation matrix

[`durable-substrate`](../../../crates/lash-perf/src/bin/durable_substrate/README.md)
now has `--case operations`, on SQLite **files** or private PostgreSQL. Each sample
opens a fresh session, commits exactly `--prior-turns` earlier turns, then measures
one cell with `--cell-parks` awaits retaining `--value-bytes` bytes per answer.
Samples do not increase the next sample's prior-turn count. These parameters are
independent; existing `prior-300`/ten-await cases retain their original geometry.

The six operation records are `session.open`, `cell.park`, `cell.end`,
`turn.commit`, `history.read` (a two-node page) and `graph.gc` (an unbounded live
closure collection). CellEnd is selected by its actual staged CELL_END node;
`cell.quiet_snapshot` is a separate label. Every transaction carries the retained
actor fence and its own SQL receipt. Background node work is excluded. GC reports
marked-node/swept-blob/swept-edge counts separately from SQL result bytes. Opening
its store handle and session teardown are outside these windows. Each sample's
whole-database GC can see retained roots from earlier, closed samples; its scope
is the database, not just the fresh session.

`operation_distribution` preserves all `samples_us` (one per owning boundary,
so park populations contain multiple entries per cell) alongside nearest-rank
p50/p95/p99/max in milliseconds. Each operation includes load1 and raw Linux CPU
PSI at receipt recording; start host records are also retained. Commit receipts
are emitted after the measured turn finishes, rather than claiming a pressure
snapshot from the earlier individual park. Observer wall time is
diagnostic; latency comparisons require load1 < 8 and a dedicated quiet rerun.

The benchmark recorder's `installed_bytes` now comes from the real successful
store insert count, rather than a process-wide first-seen NodeId set. Existing
PostgreSQL aggregate windows still include background database work and count
write-bearing command rows, WAL and relation growth; they are never substituted
for owning-window SQL bytes. They refuse reset/deallocation changes, disappeared
role/query entries, per-entry epoch/counter regressions and missing role query
visibility, including resets during snapshot reads.

The `perf-witness` graph hooks count actual envelope encodes/encoded bytes,
adoption attempts/adopted bytes and digest passes/hashed payload bytes before staging
deduplication. This witness is process-global: a curve installs an exclusive
collector per measured sample, including any concurrent same-process graph work.
Legacy hashes are SHA-256; graph-envelope hashes are domain-separated
BLAKE3; the fixed domain prefix is excluded from hashed payload bytes. The checkpoint curve exposes raw sample vectors for these counters.
The resident graph append curve also preserves each phase's raw latency vector;
aggregate phase duration is no longer its only timing evidence. Completed frame
curve fixtures are removed after their store handles have dropped.

Whole kernel parks additionally have a synchronous, thread-local
`lash_kernel_vm::perf_witness::Collector` under the opt-in `perf-witness` feature.
Install it on the thread that calls `export_whole`/`import_whole` (a worker's
thread for production cells). Its non-overlapping `machine.export`, `whole.capture`,
`whole.restore` and `machine.import` receipts retain raw nanoseconds and complete
structure task/active-call/object cardinalities. They are neither byte counts nor
VM charge, and are not host-process counters for a subprocess worker. Refused
phases have unknown geometry, rather than a fabricated zero. A functional command
showing the counted phases and the unchanged cold-relay oracle is:

```sh
kiln test //crates/lash-kernel-vm:lash-kernel-vm__unit_test \
  --test_arg=laws::whole::whole_capture_and_import_report_each_real_phase_and_structure \
  --test_arg=--exact --test_arg=--nocapture
```
