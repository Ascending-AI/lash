# FIG-2237 pool-checkout subspan report

## Seam and metric flow

- PostgreSQL: `acquire_runtime_connection` in `crates/lash-postgres-store/src/lib.rs` times only `PgPool::acquire()`. Runtime-persistence transaction and direct-query paths acquire through that helper before beginning a transaction or executing a statement. `BEGIN` and database work are outside the checkout sample.
- SQLite: `crates/lash-sqlite-store/src/conn.rs` owns one `tokio_rusqlite::Connection` backed by its dedicated connection thread. There is no checkout pool, so `pool_wait_observable=0`, the pool-wait sample vector is empty, and percentile counters are omitted rather than reporting a fake zero.
- Flow: the existing default-off `perf-witness` feature records successful PostgreSQL checkout durations in nanoseconds; the contention window snapshots them, moves them through `RuntimePerfStoreMetrics`, and exports `durable_contention.pool_wait_ms`, its phase profile, and p50/p95 counters. Default builds compile out the timer and recorder call. No dependency, pool-size, timeout, or product-behavior tuning changed.
- Structural smoke: four-worker PostgreSQL load requires at least one checkout sample per completed batch, every sample finite and nonzero, and every checkout no longer than both the maximum observed claim/service span and the whole run. SQLite asserts the seam is unavailable.

## Release witness

One optimized run per scenario, zero warmups, 12 target completions per worker, workers=4. PostgreSQL used the dedicated `postgres:16-alpine` container `lash-fig2237-pg`.

| Scenario | Claim p50 / p95 (ms) | Service p50 / p95 (ms) | Pool wait p50 / p95 (ms) | Pool samples | Observable |
|---|---:|---:|---:|---:|---:|
| `durable_queued_work_contention_sqlite` | 0.526 / 0.937 | 2.383 / 13.589 | N/A | 0 | 0 |
| `durable_queued_work_contention_postgres` | 10.236 / 20.598 | 18.007 / 30.215 | 0.103 / 0.145 | 525 | 1 |

PostgreSQL multi-worker checkout wait was nonzero: minimum 0.066 ms, p50 0.103 ms, p95 0.145 ms, maximum 3.887 ms.

## Mutation probe

- Mutation: replaced `started_at.elapsed()` with `Duration::ZERO` at the PostgreSQL checkout recorder.
- Red: `checkout waits must be measured nonzero durations: [0.0, ...]`; nextest summary: `1 test run: 0 passed, 1 failed, 96 skipped` (`MUTATION_EXIT=100`).
- Reverted and green: `PASS ... durable_queued_work_contention_postgres_smoke_binds_pool_wait_subspan`; summary: `1 test run: 1 passed, 96 skipped`.

## Gates

- API registry/coverage after first compile: passed, 10,410 entries; 1,931/1,931 example-test ratchet rows; 910 internal-consumed rows.
- `cargo check --workspace --all-targets --locked`: passed in 1m36s.
- `cargo nextest run -p lash-perf --locked`: 97 passed, 0 skipped, including the live PostgreSQL structural smoke.
- `cargo nextest run -p lash-internal-core --locked`: 1,395 passed, 2 skipped; five expected slow fault-matrix chunks.
- Feature witness unit: 1 passed (`collector_records_pool_checkout_wait_samples`).
- `cargo nextest run -p lash-internal-postgres-store --locked`: 236 passed, 4 skipped against PostgreSQL 16.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: passed in 2m14s.
- `cargo fmt --all -- --check`, `python3 scripts/check_included_file_formatting.py`, explicit Rustfmt check for the touched included contention file, and `git diff --check`: passed; include checker covered 52 files.
- Container cleanup: removed `lash-fig2237-pg`; exact-name post-check returned 0 containers.

## Deviations

The first PostgreSQL store-suite attempt stopped after 1 failure and 3 passes because the fresh container had not preloaded `pg_stat_statements`; the failing test reported SQLSTATE 55000, `pg_stat_statements must be loaded via shared_preload_libraries`. The dedicated container was recreated with `-c shared_preload_libraries=pg_stat_statements`, after which the complete 236-test store suite passed. No code was changed for this environment-only requirement.
