# Durable PostgreSQL commit cost

The 1.0 instrumentation baseline adds six physical-cost histograms to the
existing injected `StoreObserver`. Use the same runtime instruments and exporter
as other Lash metrics; no separate exporter or database sampler is installed.
Construct PostgreSQL storage with an observed `StoreObserver` to enable them.

Every durable transaction attempt records `lash.durable.commit.label` (for
example `round.outcome` or `turn.commit`) and `lash.outcome` (`success`, `error`,
or `cancelled`). Each attempt reports:

| Metric under `lash.durable.commit.` | Unit | Meaning |
| --- | --- | --- |
| `acquire_wait.duration` | µs | Pool checkout wait, including checkout validation. |
| `transaction.duration` | µs | Elapsed time from beginning the transaction to its return, including its fence, domain writes and explicit commit or rollback. |
| `sql_statements` | count | Attempted executions, including BEGIN and explicit COMMIT/ROLLBACK. Preparation, checkout validation and wire protocol messages are excluded. |
| `returned_bytes` | bytes | Raw returned column-value bytes before decoding; NULL values and wire framing are excluded. |
| `lock_statement_elapsed` | µs | Sum of elapsed time of writes, explicit row/table locks and advisory-lock statements. Includes execution and network time: an upper bound on lock wait, not pure server lock time. |
| `group_commit.members` | count | Outcome records submitted in this transaction attempt, including single-member batches; zero for transactions with no grouped outcomes. |

These observations count physical attempts independently of logical execution
permits. A refusal retains the work attempted before it. A cancelled operation
reports the partial cost measured up to cancellation; SQLx's subsequent implicit
rollback is not an observed execution. Returned bytes reveal sizes only: no
query text, bind values, credentials, or tool/model payloads are exported.
Statements and their ordering are unchanged by instrumentation. Group commit
retains its existing limits and window.

For exact server lock waits, enable PostgreSQL `log_lock_waits` in the operator's
diagnostic environment and inspect `pg_stat_activity`'s `wait_event_type` and
`wait_event` (and `pg_locks` for the blocker). Server lock-wait sampling is a
post-1.0 follow-up, separate from these client-side upper bounds.

Host wait resolution now emits its wake hint through the backend after the
resolution commits. Notification failure does not turn that committed resolution
into a store failure. Periodic polling remains the correctness fallback when a
hint is lost.
