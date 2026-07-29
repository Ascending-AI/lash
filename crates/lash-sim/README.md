# lash-sim

Deterministic Simulation Harness for Lash: an unpublished workspace crate that
drives real Lash runtime, protocol, provider, tool, process, and persistence
contracts inside a deterministic simulated world. The architecture is recorded
in `docs/adr/0009-deterministic-simulation-harness.md`. This README is the
current-status ledger for what is actually implemented and gated.

## Run modes

Every generated run is deterministic by seed and generator version and runs
the full oracle set. The `run` command has two modes:

- `--mode evidence` (default): every seed writes trace/replay/minimize
  artifacts plus a best-effort review transcript (`.trace.txt`, with path and
  SHA-256 recorded when present), and re-runs through a serialized in-memory
  reference and the real `lash-sqlite-store` backend for cross-backend
  equivalence. Roughly minutes per seed; this is the bounded evidence lane.
- `--mode search`: every seed runs live with the full oracle set plus an
  in-memory determinism replay; nothing is persisted per passing seed. A
  failing seed writes a complete reproducibility package under
  `failures/seed-<hex>/` (trace, best-effort `transcript.txt`, replay report,
  failing oracle, final summary, minimized regression package) and fails the
  run with the exact replay command. Roughly a second per seed, which is what
  makes plan-scale seed budgets real.

Count-based runs partition deterministically with `--shard <i>/<n>`: shard
`i/n` owns every seed index where `index % n == i - 1`, so the union of all
shards covers the configured seed space exactly once. The summary records
`mode`, `shard`, and `configured_seeds`.

```sh
cargo run -p lash-sim -- run --out target/lash-sim/search \
  --profile full-random --seeds 5000 --max-boundaries 2000 \
  --shard 1/9 --mode search
```

## Current executable evidence

- OpenAI-compatible, direct OpenAI Responses, Anthropic, and Google Provider
  Wire Scripts run through real provider crates via the production
  `LlmHttpTransport` seam and are included in the canonical provider matrix;
  Codex/OAuth/auth-flow exclusions are manifest-reviewed instead of
  accidental.
- Provider byte-stream handling is additionally property-tested: shared
  proptest strategies live behind the `proptest-support` feature of
  `lash-llm-transport`, with chunk-split invariance properties over the SSE
  framing layer and the Anthropic/Google stream parsers.
- Selected generated traces replay through real Lash SQLite session
  persistence via `SqliteSessionStoreFactory`, with durable peer stores and
  reopened-session evidence in the replay report.
- Full-lane Postgres trace replay is implemented as `lash-sim replay-postgres
  <trace> --out <artifact-root>`, gated by `LASH_POSTGRES_DATABASE_URL` or the
  confidence gate's Docker bootstrap, and writes replay/divergence artifacts.
- Generated traces are produced by `lash-sim.generated-workload.v8`, a
  deterministic state-machine generator over sessions, provider scripts,
  queued ingress, cancellation, triggers, observer reconnects, backend
  failure choices, provider mutations, atomic tools, exec-code, process waits,
  process wakes, worker lease/failover, retries, and duplicates.
- Generated traces include scheduler/completion evidence, a named
  `sim.oracle.operational-coverage.v1` oracle for the operational case set,
  and scenario contract oracles for Runtime, Standard, RLM, and Agent coverage
  without importing scenario test modules. Combined with the interleaved live
  turns, suspend/resume, live failure-turn, invariant floor, and real worker
  failover below, this is true DST and not only operation-level
  conformance/replay.
- Runtime, Standard, RLM, and Agent scenario contract metadata is exported
  from production/test-independent modules and serialized into `lash-sim`
  summaries alongside the generated oracle verdicts.
- Each exported Runtime, Standard, RLM, and Agent scenario contract also has a
  generated trace-slice artifact under
  `scenario-contract-slices/<suite>/<test>.json`; the slice ties the
  contract's semantic oracle to concrete generated boundary events, a
  contract-specific generated transition shape, required evidence assertions,
  a family negative fixture, and matching verdicts.
- Generated summaries include explicit model-only boundary reviews for the
  remaining partially modeled durable-effect, worker, backend-failure,
  provider-mutation, tool, exec-code, and process-wake boundaries, each with a
  named oracle and artifact evidence.
- Provider manifests include reviewed non-DST exclusions for remaining
  Codex/OAuth/direct provider paths so direct reqwest/OAuth seams are named
  instead of accidental.
- `lash-sim minimize <trace>` writes a minimized package containing the
  minimized trace, replay verdict, oracle verdict, final summary, and package
  manifest; minimization preserves the failing oracle id and semantic reason
  when the input is a failure. Failing negative fixtures live under
  `crates/lash-sim/failure-fixtures/`.
- The confidence gate declares sim lane artifacts under flat
  `target/confidence/<lane>/sim/` roots for default/broad/full, sharded
  `target/confidence/fast/<shard>/sim/` roots for the fast lane, and
  `target/confidence/sim-search/<i>-of-<n>/` roots for sharded search-fleet
  runs, including env-gated Postgres conformance evidence when the lane is
  enabled.

## Implemented DST substance

Each item below is landed and gated by `cargo test -p lash-sim`:

- The scheduler actually interleaves work: provider turns are spawned as live
  futures whose scripted-transport SSE chunks are released by
  scheduler-delivered `ProviderEvent` boundaries in seeded order, and the
  generated lane asserts a peak of at least two concurrent live turns
  (`sim.oracle.provider-turn-interleaving-depth.v1`).
- Tool, durable-effect, and exec-code coverage pass through real turns that
  SUSPEND and RESUME: a generated suspend session runs a real
  `session.turn().run()` over the real `ScriptedLlmHttpTransport`, parks on a
  tool/durable/exec await key, and is resumed only by a scheduler-delivered
  completion boundary (`sim.oracle.generated-suspend-resume.v1`). Both the
  tool-call exchange that suspends the turn and the post-resume exchange
  exercise real provider wire parsing.
- A non-retryable provider FAILURE is driven through a LIVE turn: a malformed
  mid-stream SSE chunk is delivered to a parked turn via the
  scripted-transport gating, and
  `sim.oracle.live-provider-failure-terminalizes.v1` asserts the turn
  terminalizes with a terminal failure and commits no provider output (no
  leaked partial assistant prose, no Final Value).
- The invariant floor is enforced: graph acyclicity, exactly one active Agent
  Frame, monotonic usage accounting, and Final Value as a semantic outcome
  distinct from transcript/prose, each as a named failing-capable oracle and
  re-verified from recorded facts on replay. The Runtime, Standard, RLM, and
  Agent suites each emit distinct per-contract generated semantic oracles,
  with package guards preventing protocol/agent contracts from sharing the
  same backing verdict or high-risk selected evidence while still retaining
  the 13 real per-behavior mini-oracles.
- Two regression fixtures are promoted under `crates/lash-sim/replays/`, and
  the promotion metadata is explicit that neither is a discovered product
  bug. The `queued-active-turn-cancel-race` fixture is a generated
  fast-random DST trace promoted as a deterministic regression GUARD that
  pins the active-turn queued-input/cancel contract; its package manifest
  records `historical_production_regression: false`, so it guards against
  future regressions rather than recording one found in production.
  Separately, the broad/full lane's cross-backend comparison surfaced a
  behavioral divergence on the active-turn-cancel shape which, on
  investigation, was a replay-FIDELITY gap in the harness's own SQLite
  re-drive — NOT a product bug (see Known limitations). It is retained as the
  `cross-backend-sqlite-active-turn-divergence` regression fixture. No
  product regression has been discovered by this lane to date.
- Generator substance is real: the fast profile is genuinely seed-random,
  provider mutations have distinct executable behaviors, queued-ingress mode
  varies, and worker failover is generated as REAL failover — a second worker
  incarnation reclaims the crashed owner's session-execution lease at a
  strictly higher fencing token and CONTINUES the queued work the dead owner
  could not, rejecting its stale completion
  (`sim.oracle.worker-failover-continues-work.v1`). The abstract model no
  longer fabricates worker fencing: it carries the real reclaim/fence facts
  produced by the live lease store, re-verified by the SQLite/Postgres
  backend replays.
- Failure capture is a first-class contract: a generated seed whose oracle
  fails persists the full reproducibility package under
  `failures/seed-<hex>/` before the run aborts, in both evidence and search
  modes, and the run error names the failing oracle and the exact replay
  command (`generated_seed_failure_writes_reproducibility_package`).

## Search fleet

The confidence gate's search lane (`run_sim_search_lane`) runs `--mode search`
at lane-scaled budgets: 256 seeds @ 500 max boundaries for default
(`LASH_SIM_DEFAULT_SEEDS`/`LASH_SIM_DEFAULT_MAX_BOUNDARIES`), 512 @ 512 for
broad (`LASH_SIM_BROAD_SEEDS`/`LASH_SIM_BROAD_MAX_BOUNDARIES`), and 5000 @
2000 for full (`LASH_SIM_FULL_SEEDS`/`LASH_SIM_FULL_MAX_BOUNDARIES`), all
shardable with `LASH_SIM_SHARD`. The weekly Confidence workflow partitions the
full seed space as shard `1/9` on the main full job plus eight
`sim-search:<i>/9` matrix jobs, so the fleet covers every configured seed
exactly once per week. `scripts/confidence-gate.sh sim-search:<i>/<n>` runs
one shard standalone. The fast lane is the release gate and keeps its small
fixed evidence budget; it never runs the search lane.

## Known limitations

- The cross-backend SQLite comparison once appeared to diverge on the
  active-turn-cancel shape. Investigation (the backend-equivalence test
  `crates/lash-sim/tests/cross_backend_active_turn_divergence.rs`, which
  drives two real cores — in-memory vs lash-sqlite-store — over an un-gated
  transport) showed the real stores commit IDENTICAL output across the
  active-turn enqueue / cancel / claim / complete orderings. The apparent
  divergence was a replay-FIDELITY gap in the harness's OWN cross-backend
  re-drive: the old path re-drove a recorded trace in fixed order with
  provider exchanges gated to the original in-memory run's recorded exchange
  counts, which deadlocked on the active-turn enqueue and could surface an
  extra exchange / empty output. The lane no longer runs that separate gated
  re-drive; it re-runs the SAME workload through the SAME scheduler-driven
  driver, parameterized only by the store factory
  (`replay_workload_on_sqlite`), comparing observable Lash state. No product
  fix was needed — there was no product bug.
- No real discovered product regression has been promoted under
  `crates/lash-sim/replays/` yet; the plan's done-line keeps that criterion
  open until the search fleet finds one.
