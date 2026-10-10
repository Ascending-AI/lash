# Runtime fixture reach and counted graph curves

The following reduced selections exercise the named fixtures through the real
`send()` and served durable engine. Run each selector independently:

```sh
kiln run //crates/lash-perf:lash-perf__bin -- \
  --runtime-perf-scenario rlm_large_print --runtime-perf-runs 1 \
  --runtime-perf-warmups 0 --runtime-perf-turns 1 --runtime-perf-smoke \
  --runtime-perf-out "$E/rlm_large_print.json"
```

The same geometry applies to `rlm_large_tool_catalog`,
`rlm_tool_catalog_cold`, `rlm_tool_catalog_warm`, `embed_standard` and
`embed_rlm`. Embedding retains the serving core until every sent turn and the
state export finish; the session handle alone does not own the serving node.

Large print retains its 70-line text and sixteen repeated rows. Its receipt's
`codemode.configured_instruction_limit_per_cell` is the configured bound, not a
measured charge: every code mode fixture runs under the standard instruction budget,
`rlm::InstructionBound::standard()` (20,000,000 units). The catalog fixtures retain the full tool population and
configure their session prompt plan for it. The receipt labels the configured
limits as `prompt.configured_section_bytes_limit_per_call` (524,288 bytes per
section) and `prompt.configured_total_bytes_limit_per_call` (1,048,576 bytes
across the rendered request); product defaults remain unchanged.

```sh
kiln run --config=optimized //crates/lash-perf:lash-perf__bin -- \
  --runtime-perf-scenario frame_residency_curve_sqlite --runtime-perf-runs 1 \
  --runtime-perf-warmups 0 --runtime-perf-turns 1 --runtime-perf-smoke \
  --runtime-perf-out "$E/frame_residency_curve_sqlite.json"
kiln run --config=optimized //crates/lash-perf:lash-perf__bin -- \
  --runtime-perf-scenario resident_graph_append_curve --runtime-perf-runs 1 \
  --runtime-perf-warmups 0 --runtime-perf-turns 1 --runtime-perf-smoke \
  --runtime-perf-out "$E/resident_graph_append_curve.json"
```

The SQLite frame curve sweeps 0, 1,000, 8,000 and 32,000 prior-history rows with
64 current-frame rows. Its verdict requires exactly 64 decoded rows per reopen,
bounded retained heap bytes, and exactly 64 submitted graph rows in every
sampled commit. `frame_residency.prior_<n>.commit_graph_rows_max` counts the
maximum submitted graph rows per commit in that point's measured commit window;
it does not claim to count SQL statements or database rows written. Raw
`frame_residency.prior_<n>.commit_ms` samples and their medians remain elapsed
milliseconds for diagnosis, with no shared-host timing-ratio verdict.

The resident graph curve retains every allocation slope cap, including 48
allocated bytes per resident node for snapshot append. A rejection emits
`resident_graph_rejected_run=<JSON>` on stderr using the existing run receipt
shape, then exits nonzero. Its phase entries count allocations and allocated
bytes in the named operation window, with `samples` as the divisor; the slope
subtracts the zero-resident mean before dividing by resident nodes. This makes
product regressions reviewable without changing their verdict.

The curve samples resident sizes 0, 32, 128 and 512, and a slope counts every
byte a buffer allocates when an append finds it full. The node-pointer and
active-path sequences double there, 16 bytes per resident node each, which is
what the 48 and 64 byte caps leave room for. The id and child-edge indexes
allocate a fixed amount per write at every size (FIG-5673), and the
`lash-perf` unit law
`a_held_snapshot_append_stays_within_its_cap_at_every_resident_size` holds the
snapshot-append cap at each size from 32 to 1,024, not only the three samples.
The event-read phase measures an append that fits the event sequence. When
that sequence is full it is copied to one twice as long, record by record;
that cost is amortized over the appends that follow and no phase caps it.
