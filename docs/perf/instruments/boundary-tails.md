# Boundary operation tails

`lash-perf boundary` retains a bounded ledger of measured operations alongside
its functional receipt. Use a separate output directory for each workload: the
ledger has the fixed filename `boundary-observations.ledger.json`. The receipt's
`ledger_file` is relative to its directory, so move the two files together.

After sourcing the fork's `env.sh`, run a small SQLite population:

```sh
mkdir -p .kiln/boundary-smoke
kiln run //crates/lash-perf:lash-perf__bin -- boundary \
  --case session-resume --operations 1 --callers 2 --ledger-cap 100000 \
  --store-dir .kiln/boundary-smoke/store \
  --out .kiln/boundary-smoke/receipt.json
kiln run //crates/lash-perf:lash-perf__bin -- receipt-tail \
  --receipt .kiln/boundary-smoke/receipt.json
```

`receipt-tail --receipt` also accepts the ledger directly. `--samples` overrides
the receipt's ledger location when artifacts were retained separately. It prints
one row per backend and boundary: contributing operation count, p50, p99, p99.9,
maximum, and the ten slowest operation IDs with durations and results.
`--slowest` changes that limit. Times in the table are milliseconds.

`python3 scripts/perfreport.py RECEIPT` delegates boundary receipts and ledgers to
that same consumer; no Python percentile implementation exists. Make `lash-perf`
available on PATH, set `LASH_PERF_BIN` to a materialized executable, or pass
`--receipt-tail-bin PATH`. A Kiln build report gives that path:

```sh
kiln build //crates/lash-perf:lash-perf__bin --materializations final \
  --build-report .kiln/boundary-smoke/build.json
perf_binary="$(python3 tools/buck2/outputs.py \
  --report .kiln/boundary-smoke/build.json \
  --label //crates/lash-perf:lash-perf__bin --single)"
python3 scripts/perfreport.py .kiln/boundary-smoke/receipt.json \
  --receipt-tail-bin "$perf_binary"
```

Each ledger record retains the boundary, operation identity, result, backend,
process ID, record ID, and monotonic `start_ns`/`end_ns` offsets from that Meter's
process-local epoch. Offsets can be negative when an interval started before
Meter creation. Clock origins from different child processes cannot be compared;
their interval differences remain valid. Identities name turns, sessions,
processes, cursors, tool calls, trace records or synthetic workload operations as
appropriate. Record IDs distinguish repeated observations of the same identity.

Percentiles use nearest rank on the exact retained operation intervals: for N
sorted samples, take index `ceil(p*N)-1`, clamped to the population. No averages,
buckets, rounding or interpolation enter that population. Existing runtime and
send-latency receipt-tail rows keep their interpolation rule; the boundary table
states its own nearest-rank rule.

A total over N operations is explicitly `measurement: "aggregate"` with its
`operations: N`, including an explicitly aggregate total whose N happens to be
one. For example, `session.invalidate.recovered` is a recovery total across all
sessions. Such records appear in a separate aggregate table with N and total
duration; they never enter operation percentiles. Phase counters remain totals
for functional accounting. UTC-derived ingress dwell remains in diagnostic
counters, outside the monotonic operation-tail population.

`--ledger-cap` bounds retained records across all boundaries, including aggregate
records (default 100,000; zero retains none). The Meter keeps the first records
and counts every subsequent dropped record and its represented operations. The
consumer prints `retained_records`, `dropped_records`, and `dropped_operations`;
percentiles describe retained operations only, never a complete population when
records were dropped. Child writers use the same configured cap and their own
artifact directories; the parent also applies its cap while merging their
ledgers and preserves their drop counts. Functional operation counts continue
even after truncation.

Small populations demonstrate instrumentation and consumer behavior. A p99.9
in a smoke workload is often the maximum; it does not certify a rare tail or a
quiet-host performance baseline.
