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
| `normalized_sql_texts` | Statement texts the window's observations normalized into shape keys. Each distinct text is normalized once per process; every later start, row, end and payload observation of it reuses the key, so a warm window counts zero. Keys are cached under the text's length and digest, never the text, for up to 4096 distinct texts; texts past that bound are normalized on each observation and counted each time. |

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
closure collection). The script's cell end is the `model.start` after its last
declared park: that transaction consumes the cell's answer and retires its root.
It has no standalone CELL_END node. The later `turn.commit` remains its own
window, so neither owning boundary is lost or counted twice.
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
An immutable-cache hit records zero envelope encodes and encoded bytes: zero
means no materialization occurred. The checkpoint curve gives each run, point
and sample a distinct edit identity, so paired center points measure fresh
changed-state encodes while retained large values still reuse their images.
Assignment finishes before the collector starts; capture through load is the
measured window.
The resident graph append curve also preserves each phase's raw latency vector;
aggregate phase duration is no longer its only timing evidence. Completed frame
curve fixtures are removed after their store handles have dropped.

Kernel parks and resumes on the persistent heap are counted, not timed, by
the opt-in `perf-witness` feature: `lash_kernel_vm::perf_witness`
(`enable_work`, `take_work`) counts VM steps on the calling thread; a
worker's page source counts the pages it loaded through the wire and sealed
into its parent's staging, and returns them in its progress receipts; the
broker's spool counts staged bytes (`DiskSpool::seal`); and both stores
count retirement steps (`retire_step`). They are neither byte counts of the
SQL window nor VM charge. The counted released-worker harness that reads them
(`pages_loaded`, `pages_sealed`, `spool_bytes`, `retirement_batches`), its
scenarios and its committed baseline are in
[the durable substrate README](../../../crates/lash-perf/src/bin/durable_substrate/README.md#retained-heap-baseline-fig-6196).


## Turn SQL work: joined execution receipts (FIG-6621)

The maintained replay route extends `--sql-shapes` on SQLite and PostgreSQL:

```sh
. ./env.sh
just perf-replay chat-short --turns 2 \
  --sql-shapes .kiln/sql-work/shapes.json
# Private PostgreSQL, using the same replay arguments:
# kiln gate lash <fork> -- scripts/ci/with-service.sh pg -- just perf-replay \
#   chat-short --turns 2 --store postgres --sql-shapes .kiln/sql-work/shapes.json
```

Select one workload. Use `chat-long-compacting --turns 51` to include a committed corpus compaction, or a workload file with a shorter `compact_every` interval. Beside the shape sidecar this writes `turn-sql-work.json`
and `turn-sql-work.txt`, and prints a top ten by statement count and an exact
same-turn repeated-read section. The JSON retains the complete bounded rows;
the text is a summary, not an automatic performance gate. The artifact carries
one producer experiment ID, start/build identity, binary digest, process ID,
backend, actual SQL worker thread, workload digest and successful terminal
population. A replay turn label names a round window: concurrent sessions are
not an exclusive per-session attribution, and their same-turn claim is refused.

Each invocation has an exact-SQL SHA-256 statement ID, normalized shape,
executing origin, SQL-visible relation **set**, operation/retained actor fence
when present, and start/finish phase and turn. SQLite origins are the store
closures on its connection worker; PostgreSQL origins are the executor's source
file and line. Missing owners remain unowned; missing relation sets remain
unknown (including tables hidden inside server routines). Neither is borrowed
from a neighboring operation. Relation participation is not physical table or
index work: a join counts once, with its whole returned/affected rows and byte
cost. `work.totals` and `per_backend` keep the existing backend-specific units above. Never sum
SQLite literal bytes with PostgreSQL encoded bytes, or treat SELECT command
rows on PostgreSQL as mutations.

The meter installs before session creation, retains session-start and compaction
shape cuts, and keeps the joined observer through maintenance and shutdown.
`work.active` contains executions that have not observed completion. Completed
rows retain both cuts, so cross-window work is visible. `conserved` checks the
sum over completed and active rows against independently maintained totals;
replay also checks the old statement/returned-row witness and SQLite's completed
connection change counts. In SQLite the joined affected-row delta is finalized
at the next statement or connection-call boundary, because PROFILE can precede
`total_changes` updates (e.g. DELETE's truncate path). FK/trigger subprogram start notifications on a live handle count as part of its one invocation. Dropped PostgreSQL streams
stay active. Active work, unknown bind-byte counts or overflow refuses a complete
export; the JSON/text is written before that refusal. A known empty relation set
and a measured zero payload remain different from unknown.

Repeated reads use full SQL plus full parameter equality and origin within the
same window; a digest alone never proves equality. Keys are private and are
not serialized. SQLite INTEGER, BLOB, NULL and no-bind keys are exact. SQLite
TEXT keys are **exact modulo embedded NUL**, because expanded SQL truncates at
NUL; Lash's session/turn/input identifiers cannot contain NUL. REAL binds refuse
repeat qualification because SQLite expands them with only 15 significant
digits. PostgreSQL SELECT/WITH candidates with data-modifying CTEs, hidden Lash server routines or unsupported syntax stay unqualified; row locks do not imply cache validity. Primitive keys include encoded type, NULL marker, lengths
and actual binary payload; custom encodings that can carry deferred patches
stay unqualified. Oversized keys also stay unqualified. `repeat_identities_complete` refuses a whole-population exact-repeat claim when any candidate is excluded. `read_bind_classes`
counts invocations in each class (including excluded ones); the repeated rows
carry their class, allowing shares of the **reported** repeat population to be
computed without pretending the excluded population is zero. A repeated query
can return changed state: these counts never certify caching validity.

Only `--sql-shapes` enables the extra joins/keys and existing operation scopes.
Production builds compile them out without `perf-witness`. The existing shape
limit is 4096 pairs; joined rows are bounded at 32768, exact parameter/read keys
at 65536, individual keys at 65536 bytes and key admissions at 16 MiB. Active executions and pending PostgreSQL binding captures are each bounded at 1024. Query objects retain only epoch tokens, so pending equality values disappear with the collector. Overflow
is explicit and refuses a complete claim. No values leave the observer, and all
private equality keys disappear when its exclusive collector drops. Enabled
allocation and CPU results include observer work; compare useful work using an
on/off pair from the same executable with `perf stat -e instructions:u`, record
load1 and `/usr/bin/time -v` peak RSS, and keep host wall time unqualified.
