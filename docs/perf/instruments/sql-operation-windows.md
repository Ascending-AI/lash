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
| `sqlite_work.vm_steps` | Sum of SQLite VM instructions, from `StatementStatus::VmStep`, measured per execution by start/end differences, including cached statements. These are neither Lash VM instructions nor CPU cycles. |
| `sqlite_work.fullscan_steps` | Sum of `FullscanStep` advances during full table scans. **Not every row scanned**: indexed access does not contribute every visited row. |
| `sqlite_work.sorts` | Sum of `Sort` operations. |
| `sqlite_work.autoindex_rows` | Sum of rows inserted into automatic indexes (`AutoIndex`). |
| `sqlite_work.reprepares` | Sum of automatic recompilations (`RePrepare`). |
| `elapsed_nanos` | Elapsed caller-window wall time in nanoseconds, including queueing and observation, measured at the snapshot. It is not SQL server time. |
| `configured_shape_limit` | Configured upper bound of 64 retained normalized SQL shapes; not measured cardinality. |
| `unretained_shape_statements` | Sum of calls whose shape could not be retained after the bound. Total statements, rows and work still include them. The shape map's length is retained cardinality, not total unique shapes. |

Shape keys fold whitespace/case, remove comments and replace literals and bind
positions; bound values are never expanded for these receipts. SQL over 4096 bytes
and dollar-quoted SQL use an opaque key. PostgreSQL `sqlite_work` is `null`,
meaning server work is unknown. This instrument does not enable or read
`pg_stat_statements` and adds no SQL queries. Existing PostgreSQL returned-column
byte and lock-statement-elapsed instruments retain their distinct definitions,
as does the seven-trip `model.start` law. Logical commit bytes, domain writes,
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
