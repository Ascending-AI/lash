# FIG-3404 — serde_json::Value materialization: where it dominates, cache vs replace

Measurement spike. Source pinned at `de8ae15d44a3bc3e11e0e7e81e52890ae5df84de`;
none of the measured files differ from `718703dd4` (main at report time), so the
numbers describe current main.

## Method and caveats

- `lash-perf` dhat variant (`//crates/lash-perf:lash-perf__bin__fv_86b254f2`),
  debug build, `--runtime-perf-runs=1 --runtime-perf-warmups=0
  --runtime-perf-turns=2 --runtime-perf-dhat --runtime-perf-dhat-frames=48`.
  Allocator mode `dhat-heap+stats_alloc`: absolute bytes include dhat
  bookkeeping and are not comparable to normal `stats_alloc` runs; site shares
  within a run are consistent.
- Every heap program point in each dhat profile was attributed on two axes:
  *kind* — `tree` (serde_json `Value`/`Map` node construction, `to_value`/
  `from_value`, `Value`/`Map` clone, `json!` internals) vs `ser_bytes`
  (`to_vec`/`to_string` output buffers, not tree churn) — and *site* — the
  lash-crate channel on the stack (`commit_hash`, `compact_contract`,
  `schema_docs` projection, `host_env` lashlang host-environment
  reconstruction, `schema_render_ts` TypeScript signature rendering,
  `lashlang_schema_import`, `catalog_build` registry/preamble plumbing).
- Allocation bytes are the ticket's axis. No wall-clock claim is made; dhat
  runs are ~50x slower than a normal turn.
- `rlm_tool_catalog_cold` calls `refresh_tool_catalog` (registry recompose +
  artifact-cache invalidation) before each measured turn; `warm` does it once
  before the loop (`recomposition_count` 1 vs 0, `cache_state` 0 vs 1).
  The lazy rebuild lands on the first post-invalidation catalog access — in
  the harness, `tool_catalog_metrics`/`await_background_work` before/after
  `run_turn` (phase_probe.rs:623,814) — so cold and warm measured turns are
  byte-identical (~1.28 GB stats_alloc each) while cold's whole-run dhat
  total carries ~one extra artifact build (~50-60 MB) + ~16 MB extra
  recompose. Both run one Lashlang `finish()` cell per turn against the
  73-manifest synthetic catalog.

## Where Value construction dominates

Whole-process dhat totals (build + seed + 2 measured turns):

| scenario | total bytes | Value-tree bytes (share) |
|---|---|---|
| standard | 8.7 MB | 3.28 MB (37.6%) |
| rlm_tool_calls | 16.5 MB | 6.72 MB (40.8%) |
| durable_standard_tool_turn_sqlite | 15.5 MB | 5.42 MB (35.1%) |
| durable_rlm_checkpoint_turn_sqlite | 18.9 MB | 6.83 MB (36.2%) |
| rlm_large_tool_catalog | 277.4 MB | 123.6 MB (44.5%) |
| rlm_tool_catalog_warm | 418.6 MB | 188.7 MB (45.1%) |
| rlm_tool_catalog_cold | 543.1 MB | 253.2 MB (46.6%) |

Value-tree bytes by site (whole run):

| site | standard | tool_calls | dur_std | dur_rlm | large_cat | warm | cold |
|---|---|---|---|---|---|---|---|
| host_env | – | 1.75 MB | – | 1.75 MB | 46.9 MB | 78.1 MB | 109.3 MB |
| compact_contract | 0.77 MB | 0.94 MB | 0.83 MB | 0.94 MB | 47.2 MB | 59.0 MB | 70.8 MB |
| catalog_build | 1.67 MB | 1.69 MB | 2.05 MB | 1.69 MB | 16.4 MB | 29.2 MB | 41.4 MB |
| schema_render_ts | – | 0.09 MB | – | 0.09 MB | 3.4 MB | 9.8 MB | 16.2 MB |
| schema_docs (SchemaContract clone/projection) | 0.40 MB | 0.32 MB | 0.52 MB | 0.32 MB | 5.3 MB | 5.4 MB | 5.6 MB |
| lashlang_schema_import (tree subset) | – | – | – | – | 1.1 MB | 1.6 MB | 2.2 MB |
| commit_hash | 0.31 MB | 0.44 MB | 0.45 MB | 0.54 MB | 0.42 MB | 1.37 MB | 2.28 MB |
| other | 0.13 MB | 1.48 MB | 1.57 MB | 1.50 MB | 2.8 MB | 4.1 MB | 5.4 MB |

Adjacent non-`Value` churn on the same schema pipeline (kept separate from the
tree numbers): `lashlang::json_schema::SchemaImporter` produces ~62/95/129 MB of
`format!`/`String` and `TypeExpr` allocations in large/warm/cold, and
`lash_typescript::signatures::render_schema_type` another ~31/52/73 MB of
signature strings — both driven by the same per-build/per-cell reconstruction.

### Site 1 — schema documents per catalog build: dominant, and not only per build

The single largest channel is reconstructing the lashlang host environment from
catalog schemas. Per call on the 73-tool catalog it costs ~10-15 MB of `Value`
tree (`lashlang_tool_operation_contract` deep-clones each tool's
input/output schema `Value`; `OperationContract::to_binding` re-clones into
bindings; `filtered_tool_catalog` clones the whole catalog when masked paths
exist; `LashlangHostCatalog` is cloned per `host_environment_masking` call),
plus ~12-17 MB of schema-importer string churn on top.

It runs far more often than "per catalog build":

- `TypescriptDialect::render_execution_section` builds `host_environment`
  **twice per preamble build** (typescript.rs:163 for the host-surface section,
  typescript.rs:428 only to read `environment.abilities.sleep`).
- `executor/mod.rs:375` builds it **once per cell** on the plain path.
- `resolve_and_build_deferred_environment_from_references` builds a masked
  environment **twice per cell** with deferred resolutions (deferred.rs:394 for
  ambient classification, deferred.rs:408 for the final environment).

Measured: `rlm_lashlang.deferred_resolve` allocates ~262 MB per turn
(stats_alloc counters, dhat mode) on the large catalog — identical in cold and
warm measured turns, because the session catalog cache
(`tool_catalog_cache_entry`, session.rs:389) only covers the preamble artifact
and does not reach the per-cell rebuild. The per-invalidation artifact rebuild
(~50-60 MB dhat-tb per build: `build_tool_catalog_entry` totals
180/130 MB across 3/2 builds in cold/warm) lands on the first post-refresh catalog access outside `run_turn`
in this harness; in production the same lazy rebuild is paid by the first
`pin_tool_surface`/`shared_tool_catalog` access after invalidation. On the
small benchmark catalog the same per-cell rebuild is ~0.9 MB `Value` tree per
cell — the cost is linear in catalog schema size.

### Site 2 — compact-contract materialization: second, mostly per build

- `resolve_schema_ref_value` (schema_docs.rs:326) deep-copies the entire schema
  whenever it contains a `$ref`, recursively rebuilding every map node — the
  largest single `Value`-tree site in the compact family (~30 MB/run on the
  large catalog, once per compact materialization of a ref-bearing schema).
- `CompactToolContract` is already memoized per contract
  (`compact_contract_shared`, compact_cache) and manifests carry stored compact
  contracts; the remaining churn is `project_tool_catalog` serializing every
  compact contract to `Value` under a per-handle `OnceLock` (re-runs per
  catalog build, ~3-7 MB/build) and `CompactToolContract` owned clones /
  deserialization where `ToolDefinition`s ride inside the deferred-resolution
  snapshot.

### Site 3 — commit hashing: minor, bounded, catalog-independent

`stable_json_string` is `serde_json::to_string`; the `Value` churn is the
callers' `serde_json::to_value` of `OperationId` / `SessionNodeIntent` /
`RuntimeCommitIntent` / `CheckpointIntent`, ~150-270 KB tree per turn plus a
comparable `ser_bytes` output. `RuntimeCommit::measure_budget` adds
`to_vec`-per-component buffer churn (~0.3-1 MB/turn, no `Value` tree). Total
commit-family `Value` tree is ~9% of tree bytes in the worst measured scenario
(`standard`, tiny catalog) and <1% in catalog-heavy ones — bounded and
catalog-independent.

## Ruling

**(a) — cache the derived schema documents keyed by tool-set identity.** The
contained win, correctly scoped: the dominant cost is not `Value` representation
overhead but re-deriving identical documents N times — host environments per
preamble build (×2) and per cell (×1-2), compact-contract projections per
catalog build. A memoized host-catalog/`OperationContract` set keyed by
tool-set identity (registry/catalog generation, surface resource identity),
with masked paths and per-cell globals applied as overlays, removes the
measured ~40-50% schema-pipeline share of allocation bytes in catalog-heavy
workloads, including the per-cell share the existing catalog cache misses.

**(b) — rejected.** Replacing the `Value` representation (canonical writer /
arena) bounds the win at a constant factor per node and still performs the
rebuilds; it cannot beat not doing the work. Its best measured target — the
commit-hash `to_value`→`to_string` pair — is ~9% of tree bytes at worst and
<1% in catalog-heavy runs. Worse, it is
the one site where correctness is byte-exactness: `Value` maps are `BTreeMap`
(sorted keys), so a direct writer must implement sorted-key serialization to
keep `turn_commit_hash` digests identical, or it is a durable change — a large
proof obligation for a small channel.

"Not worth it" is also wrong: this is the largest remaining measured
allocation family, and the existing `tool_catalog_cache_key` seam shows the
identity key already exists.

## Fix ticket scope (filed)

- Memoize `lashlang_resources_from_tool_catalog` / the base
  `LashlangHostEnvironment` keyed by tool-set identity; apply
  `masked_call_paths`, `with_globals`, and `with_process_handles` as cheap
  per-cell overlays.
- `render_execution_section`: build the host environment once (the second call
  reads only `abilities.sleep`).
- `resolve_and_build_deferred_environment_from_references`: share the masked
  environment between ambient classification and the final build (masks differ;
  union-mask once).
- Optionally: hoist `project_tool_catalog`'s per-handle `OnceLock` to a
  per-tool-set cache so catalog rebuilds reuse projected `Value` rows.
- Commit-hash path: leave `stable_json_string`/`to_value` as-is.

## Reproduction

```
kiln run //crates/lash-perf:lash-perf__bin__fv_86b254f2 -- \
  --runtime-perf-scenario <scenario> --runtime-perf-runs=1 \
  --runtime-perf-warmups=0 --runtime-perf-turns=2 --runtime-perf-dhat \
  --runtime-perf-dhat-frames=48 \
  --runtime-perf-dhat-out=<out>.dhat.json --runtime-perf-out=<out>.json
```

with `LASH_RUNTIME_PERF_TURN_TIMEOUT_MS=600000` (debug+dhat turns exceed the
10 s default). Profiles and the bucketing script are not shipped (scratch
instrumentation); the per-site table above is the artifact.
