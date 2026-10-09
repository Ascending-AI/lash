# Comparable duration history and operation tails

```sh
kiln run //crates/lash-perf:lash-perf__bin -- duration-trend \
  --history crates/lash-perf/fixtures/duration-trend-history.jsonl \
  --csv "$E/duration-series.csv"
kiln run //crates/lash-perf:lash-perf__bin -- receipt-tail \
  --receipt "$E/runtime.json" --slowest 5
# The same command reads a latency receipt or its raw sample ledger.
kiln run //crates/lash-perf:lash-perf__bin -- receipt-tail \
  --receipt "$E/cross-worker/latency.json" --slowest 5
```

`receipt-tail` prints tab-separated per-operation p50, p99, p99.9 and max in
milliseconds, contributing and missing sample counts, and the slowest raw IDs.
Percentiles interpolate at `p * (n - 1)`. Runtime IDs are receipt run indices
and retained turn indices; sampled metric IDs are raw array positions. Latency
IDs are lane/sample coordinates. These identify raw records, not product TurnIds.
Runtime first and subsequent turns are separate; high-traffic knee steps are
separate populations. Run envelopes and per-turn phase sums are explicitly named
`run_total` and `phase_total`, and are never pooled with individual operations.
Latency cases, cold/warm, statuses and poll timeouts stay separate. Signed phase
differences survive; cumulative process CPU endpoints are excluded and the
simulated poll clock is labelled. Missing marks have no fabricated zero.
A latency receipt names its actual raw ledger (including a custom destination);
`--samples <FILE>` can point to a separately retained ledger. Reading the raw
ledger directly also works. An absent ledger is an error, never a summary fallback.
Small populations describe their samples; they cannot certify rare tails.

History records now require host identity (hashed machine/hardware/kernel facts),
the actual build compiler and flags, allocator, backend, configured storage
policy, workload, and all configured workload geometry, including stack size.
Different identities remain separate, with typed `IdentityMismatch` reasons in
the table; missing identity is refused. PostgreSQL policy is read before the
measured window; a failed policy read disables comparisons of that population.
SQLite policy is the harness's configured store options. Neither is a measured
durability claim. History without the current identity shape must be regenerated;
the pre-1.0 baseline version remains unchanged.

The advisory detector retains its existing 50%/five-consecutive-run rule. Once
it detects a shift, the baseline freezes and the finding remains open through
later runs, recovery and compaction. Recent observations plus the original
bounded detection witness are retained. A review in the committed
`scripts/perf_duration_level_shifts.json` closes an accepted shift:

```json
{
  "level_shifts": [{
    "profile": "full",
    "scenario": "standard",
    "metric": "total_ms",
    "commit": "<commit that changed the level>",
    "effective_from": "2026-10-09T10:00:00Z",
    "who": "<reviewer>",
    "reason": "<why the shift is accepted or tracked as a bug>",
    "disposition": "accepted"
  }]
}
```

Use `bug` to record ownership and the reason while keeping the finding open.
`accepted` starts a new comparison window at `effective_from`; it does not mute
a later shift. Selectors are exact, and omitted selectors cover every matching
series, so narrow acknowledgements to the reviewed population. Who and why are
required. The committed file is the review trail; cache eviction still loses
observations and is never an acknowledgement.

Existing `_ms` metric summaries and `metric_summary_ms` distributions both enter
history. The latter use `sampled_operations/` names so pooled operation medians
cannot overwrite per-run metric medians. The CSV carries quantities, units,
window, statistics and full identities for offline review. The trend remains
advisory, and neither this reader nor a receipt establishes a quiet-host baseline
or guarantees someone has read a warning.
