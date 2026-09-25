# lash-sim

Seeded boundary simulation for Lash: an unpublished workspace crate that drives
real runtime, protocol, provider, tool, process, and persistence contracts while
a seed chooses modeled boundary delivery order. Tokio scheduling and other
in-process interleavings remain uncontrolled, so this is not a deterministic
simulated world or an exhaustive interleaving harness. ADR 0044 records that
ruling; this README is the current-status ledger for what is implemented and
gated.

## Run modes

The generated workload and modeled boundary schedule are stable for a seed and
generator version, and every run executes the full oracle set. Real execution
may still vary between runs; captured traces and minimized failure packages are
the replay evidence. The `run` command has two modes:

- `--mode evidence` (default): every seed writes trace/replay/minimize
  artifacts plus a best-effort review transcript (`.trace.txt`, with path and
  SHA-256 recorded when present), and runs the workload twice more in the
  SERIAL lane (below). Roughly minutes per seed; this is the bounded evidence
  lane.
- `--mode search`: every seed runs live with the full oracle set plus an
  in-memory determinism replay; nothing is persisted per passing seed. A
  failing seed writes a complete reproducibility package under
  `failures/seed-<hex>/` (trace, best-effort `transcript.txt`, replay report,
  failing oracle, final summary, minimized regression package) and fails the
  run with the exact replay command. Roughly a second per seed, which is what
  makes plan-scale seed budgets real.

Every turn and every effect boundary of a generated world runs where a
deployment runs it: inside a handler of lash-restate's engine on the
in-process Restate server double (`lash-restate-test`), seeded by the
workload's seed, on virtual time (`backend::SimEngine`). SQLite is storage
only. The search lane's server runs live attempts concurrently, so sessions
interleave as they would against a real server; the serial lane
(`sim.oracle.serial-engine-determinism.v1`) runs one live provider turn at a
time on a server that runs one attempt at a time, and requires two runs of one
seed to deliver the same boundaries, reach the same outcome and grant the
server's turn in the same order, with no stall preemption.

Count-based runs partition deterministically with `--shard <i>/<n>`: shard
`i/n` owns every seed index where `index % n == i - 1`, so the union of all
shards covers the configured seed space exactly once. The summary records
`mode`, `shard`, and `configured_seeds`.

```sh
kiln run //crates/lash-sim:lash-sim__bin -- run --out "$PWD/target/lash-sim/search" \
  --profile full-random --seeds 5000 --max-boundaries 2000 \
  --shard 1/9 --mode search
```

### Multi-arm SQLite fault witness

The SQLite transaction wrapper declares `AfterBegin`, `BeforeCommit`, and
`CommitIo` through a `sim_fault!` macro that expands away when the store's
`testing` feature is disabled. The testing injector accepts an ordered plan of
one-shot arms, each targeting a one-based occurrence of one declared point.

Run the bounded composition witness with:

```sh
kiln run //crates/lash-sim:lash-sim__bin -- backend-faults --backend sqlite \
  --out /tmp/lash-sim-sqlite-faults --seed 140050432
```

`--backend postgres` runs the same plan against the PostgreSQL injector when
`LASH_POSTGRES_DATABASE_URL` is set, writing `postgres-faults.json` under
`lash.sim.postgres-substrate-faults.v2` with `sim.oracle.postgres-*` ids; every
failure package and replay hint names the backend it was produced on.
`sqlite-faults` remains a working alias of `backend-faults` for the SQLite
default, which is what the confidence gate invokes.

Expect `/tmp/lash-sim-sqlite-faults/sqlite-faults.json` to use
`lash.sim.sqlite-substrate-faults.v2`. Its `composition_witness.plan` records
the generated workload seed and ID, the two source boundary IDs, arm order,
point occurrences, and the two-attempt policy. The workload seed chooses among
the three ordered pairs of distinct declared points, so a small seed set drives
different fault schedules while replaying one seed keeps the same plan. The
zero-arm control commits on its first attempt. Each single-arm control returns
one injected storage failure and commits on retry. For the documented seed, the
paired run fails first at `after_begin`, then at `commit_io`, exhausts the
two-attempt policy, and leaves the reopened head at the prefix revision.
`repeated_paired` must record the same arm identities, order, attempt outcomes,
and final head. This is an injected operation failure under the recorded retry
bound, not a discovered runtime invariant violation.

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
- The fixed runtime proofs, the agent contracts and the provider, feedback
  and logical-turn laws run each turn on the same engine, under their own
  seeds with serial scheduling. SQLite and PostgreSQL appear only as stores.
- The real SQLite transaction wrapper has a production-absent, `testing`
  feature-gated fault controller. `lash-sim backend-faults` deterministically
  injects aborts after `BEGIN IMMEDIATE` and before commit, a commit-boundary
  `SQLITE_IOERR`, and a mid-sequence close/reopen. Each seed checks typed error
  return, retention of the preceding committed head, rollback of failed work,
  and idempotent operation-receipt replay; oracle failures persist an exact-seed
  reproduction package before the command exits. The same command derives an
  explicit two-arm plan from the generated workload and records zero-, single-,
  paired-, and repeat-run evidence for its bounded composition oracle.
- Generated traces are produced by `lash-sim.generated-workload.v11`, a
  deterministic state-machine generator over sessions, provider scripts,
  queued ingress, cancellation, triggers, observer reconnects, backend
  failure choices, provider mutations, atomic tools, exec-code, durable
  effects under crash and redrive, retries, and duplicates.
- Generated traces include scheduler/completion evidence, a named
  `sim.oracle.operational-coverage.v1` oracle for the operational case set,
  and scenario contract oracles for Runtime, Standard, RLM, and Agent coverage
  without importing scenario test modules. Combined with interleaved live
  turns, suspend/resume, a live failure turn, the invariant floor, and durable
  effects redriven by the engine, this is seeded boundary orchestration over
  real execution,
  not a claim that Tokio interleavings are deterministic.
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
  remaining partially modeled durable-effect, backend-failure,
  provider-mutation, tool, and exec-code boundaries, each with a named oracle
  and artifact evidence.
- Provider manifests include reviewed non-DST exclusions for remaining
  Codex/OAuth/direct provider paths so direct reqwest/OAuth seams are named
  instead of accidental.
- `lash-sim minimize <trace>` writes a minimized package containing the
  minimized trace, replay verdict, oracle verdict, final summary, and package
  manifest. The runner and minimizer share one trace-derived oracle battery;
  minimization preserves the target id, status, and semantic reason across
  every artifact, rejects a live-only target that a serialized trace cannot
  re-evaluate, completes final replay, stages the complete package in a sibling
  temporary directory, and publishes it with one directory rename. An existing
  package is refused without mutation rather than reused or overwritten.
  Failing negative fixtures live under `crates/lash-sim/failure-fixtures/`.
- The confidence gate declares sim lane artifacts under flat
  `target/confidence/<worktree-slug>/<lane>/sim/` roots for default/broad/full,
  sharded `target/confidence/<worktree-slug>/fast/<shard>/sim/` roots for the
  fast lane, and
  `target/confidence/<worktree-slug>/sim-search/<i>-of-<n>/` roots for sharded
  search-fleet runs, including env-gated Postgres conformance evidence when the
  lane is enabled. CI overrides the root to the established unqualified
  `target/confidence/` artifact tree.

## Implemented DST substance

The routine tests for the items below run with
`kiln test //crates/lash-sim:lash-sim__unit_test //crates/lash-sim:test_batch`.
The deferred cross-backend suites run in their named service gates.

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
  the 11 real per-behavior mini-oracles.
- One regression fixture is promoted under `crates/lash-sim/replays/`, and
  the promotion metadata is explicit that it is not a discovered product
  bug. The `queued-active-turn-cancel-race` fixture is a generated
  fast-random DST trace promoted as a deterministic regression GUARD that
  pins the active-turn queued-input/cancel contract; its package manifest
  records `historical_production_regression: false`, so it guards against
  future regressions rather than recording one found in production. No
  product regression has been discovered by this lane to date.
- Generator substance is real: the fast profile is genuinely seed-random,
  provider mutations have distinct executable behaviors, queued-ingress mode
  varies, and a durable effect is a REAL crash and redrive — its first handler
  attempt runs the effect and dies after the engine recorded it, and the
  engine replays the invocation into a redrive that is served the recorded
  result without running the effect again
  (`sim.oracle.durable-effect-exactly-once.v1`).
- Failure capture is a first-class contract: a generated seed whose oracle
  fails persists the full reproducibility package under
  `failures/seed-<hex>/` before the run aborts, in both evidence and search
  modes, and the run error names the failing oracle and the exact replay
  command.
- Real SQLite substrate faults are gated in `scenario-harnesses`: four seeds
  cover the complete point set per PR, while the full soak runs 256 seeds to
  vary the deterministic 1-to-8-commit prefix. Reports explicitly list any
  scenario omitted by a caller-supplied seed bound.

## Search fleet

The confidence gate's search lane (`run_sim_search_lane`) runs `--mode search`
at lane-scaled budgets: 256 seeds @ 500 max boundaries for default
(`LASH_SIM_DEFAULT_SEEDS`/`LASH_SIM_DEFAULT_MAX_BOUNDARIES`), 512 @ 512 for
broad (`LASH_SIM_BROAD_SEEDS`/`LASH_SIM_BROAD_MAX_BOUNDARIES`), and 243 @
2000 for full (`LASH_SIM_FULL_SEEDS`/`LASH_SIM_FULL_MAX_BOUNDARIES`), all
shardable with `LASH_SIM_SHARD`. The weekly Confidence workflow partitions the
full seed space across nine `sim-search:<i>/9` matrix jobs, so the fleet covers every configured seed
exactly once per week. `scripts/confidence-gate.sh sim-search:<i>/<n>` runs
one shard standalone. A dedicated `sim-search:` shard is bounded by wall
clock, not the seed count alone: the gate hands each pass a `--time-budget`
derived from the job cap minus measured fixed cost, and `lash-sim` stops
cleanly at the budget and records `reached_seeds` in the summary, so a slow
shard still produces evidence. The fast lane is the release gate and keeps
its small fixed evidence budget; it never runs the search lane.

## Known limitations

- No real discovered product regression has been promoted under
  `crates/lash-sim/replays/` yet; the plan's done-line keeps that criterion
  open until the search fleet finds one.
