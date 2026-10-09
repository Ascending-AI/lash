# PostgreSQL statements for one facade workload

`lash-perf boundary --case pg-facade --pg-statements` reports the product
PostgreSQL store's server statement executions by query shape. It adds no
product SQL instrumentation and changes no product queries. The ordinary
boundary receipt still counts facade operations; those counts and SQL calls
are different quantities. Neither is a count of wire round trips.

Build the binary once, then run it against a disposable PG18 service:

```sh
. ./env.sh
E="$PWD/.kiln/FIG-5724"
mkdir -p "$E"
kiln build //crates/lash-perf:lash-perf__bin --materializations final \
  --build-report "$E/build.json"
B="$(python3 tools/buck2/outputs.py --report "$E/build.json" \
  --label //crates/lash-perf:lash-perf__bin --single)"
kiln gate lash fig-5724 -- scripts/ci/with-service.sh pg -- bash -c '
  exec "$1" boundary --case pg-facade --operations 8 --callers 1 \
    --workload smoke-v1 --store-dir "$2/store" --out "$2/run.json" \
    --postgres-url "$LASH_POSTGRES_DATABASE_URL" \
    --pg-statements --pg-statements-top 10
' bash "$B" "$E"
```

Use your fork name and a fresh `--store-dir` for each invocation. The service
must preload `pg_stat_statements`, calculate query IDs, and enable statement
tracking (`top` or `all`). The connection supplied by `--postgres-url` is an
administrative connection: it must create databases, login roles and the
extension, and read other roles' statistics (a superuser on the private service
suffices). Do not point this command at a production server.

The opt-in run creates a UUID-named database and login role, installs the
extension in that database, and provisions the committed product baseline
schema as the workload role. Nodes, store pools and sessions are opened before
the first snapshot. All measured connections use that role and database;
observation uses the administrative role. Cleanup drops the private database
and role on success or a returned error. An externally killed process can leave
these resources behind; remove the named database and role from its disposable
server before reusing it. Passwords and connection URLs are omitted from the
receipts.

The window begins immediately before the first send and ends after the last
send settles, before node shutdown. It includes durable engine and background
SQL by the workload role during that interval. It excludes provisioning, store
verification, node/session setup, observer queries and shutdown. It subtracts
the before snapshot for every query ID, including shapes shared by setup and
measurement, and omits shapes with no calls or plans in the window. It never
resets global statistics.

The SQL filter uses both database OID and executing role OID, requires a
non-null query ID, and selects top-level statements. This excludes other
workloads even on the same server and excludes administrative queries in the
run database. Top-level filtering avoids counting both a statement and its
nested statements when the server tracks `all`. Query IDs are local to the
server/catalog and PostgreSQL major; do not join different runs by ID alone.
[PostgreSQL 18 documents these identities and counters](https://www.postgresql.org/docs/18/pgstatstatements.html).

Two sidecars accompany `run.json`:

- `run.pg-statements.txt`: top N shapes by total execution time and by calls,
  also printed to stdout. `--pg-statements-top` defaults to 10.
- `run.pg-statements.json`: every measured query ID and PostgreSQL's normalized
  representative SQL, calls, rows, total and mean execution milliseconds,
  shared blocks hit/read and plans. Plans are `null` when tracking is disabled.
  Mean time is the window's total divided by its calls, not a subtraction of
  cumulative means. The receipt records PG version, workload identity and
  size, private database/role OIDs, gate identity, window timestamps in Unix
  milliseconds, baseline calls and setup-only
  query IDs.

These are diagnostic receipts; SQL execution time is not client latency, and
shared block counters are not bytes transferred. The tool rejects a window if
statistics reset, any entry is evicted, a baseline shape disappears, its
`stats_since` changes, or counters decrease. Global eviction rejection is
conservative: another tenant's eviction can invalidate this run. An invalid
window produces no statement sidecar. Server tracking settings must remain
fixed during the window.

EXPLAIN is skipped and the receipt says why: this seam has no representative
bound parameters. Guessing them or replaying write statements with
`EXPLAIN (ANALYZE, BUFFERS)` would not describe the measured workload safely.
