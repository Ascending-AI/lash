# Confidence Gate

## Status

accepted

Amended 2026-09-24 (FIG-3669), **partly implemented**:
[ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md)
makes Restate the only effect engine and the SQL stores storage only. This ADR
specifies SQL-engine behaviour: the SQLite and PostgreSQL backend conformance
and the lease, fencing and reopen contention evidence in the `default` and
`broad` lanes. FIG-3861 removed the SQLite SQL effect engine and its rows; descriptions of it below are historical. Session and process lease passages await their own cutovers.

## Decision

Lash has one executable confidence contract: `scripts/confidence-gate.sh`.
The gate has explicit lanes instead of an implicit pile of local commands:

- `fast`: deterministic Runtime, Standard Protocol, RLM Protocol, and Agent
  Scenario harnesses; runtime state-machine property checks; deterministic
  simulation/provider proof shards; minimizer fixture evidence; durable
  fault-matrix metadata; and performance guard identity tests. The local
  `fast` command is an aggregate over first-class `fast:<shard>` commands so
  CI can run the same evidence in parallel.
- `default`: `fast` plus Sqlite backend conformance, production-backed backend
  contention evidence, coverage blind-spot artifacts, and targeted
  cargo-mutants evidence for high-risk direct/model and
  deterministic-simulation paths.
- `broad`: bounded broad evidence. It runs a full-profile generated simulation
  under explicit seed/boundary budgets, Postgres conformance when an env URL or
  Docker bootstrap is available, static model replay evidence for
  generated/minimized traces, backend contention evidence, and targeted
  mutation. It is not a true full confidence claim.
- `full`: true full confidence. It includes broad semantics and full
  cargo-mutants over the same critical crates; the lane refuses non-full
  mutation scopes. In the weekly workflow the per-package mutation stage is
  `mutation-packages-rotating`: each leg judges a bounded slice of its
  package's mutant space (a smoke canary plus one `--shard` slice), and the
  slice index rotates with the run number so successive scheduled runs sweep
  the space. A green weekly is therefore rotating evidence — the manifest,
  stage name, and run summary record which slice ran at which revision — and
  never a complete mutant union at one revision. That union exists only in an
  unsharded local `full` run.

Coverage is not a percentage goal. The gate writes LCOV, missing-line text, and
summary JSON under `target/confidence/<lane>/coverage/` so uncovered source is
reviewed as a blind-spot map.

Mutation testing is required for lanes that claim it. `cargo-mutants` absence
fails `default`, `broad`, and `full` unless `LASH_CONFIDENCE_BOOTSTRAP=1`
installs the pinned tool version. Mutation success is never faked as a skipped
pass, and a targeted bounded run is never labeled as `full`. A bounded
rotating leg is labeled `bounded_rotating` in its manifest, sidecars, stage
name, and the run summary — with its leg coordinate, run index, and revision —
never as a complete union; the `full` label is reserved for a verified
complete mutant union at one revision.

### Failure evidence and quarantine policy

The first failing attempt is evidence, not a disposable prelude to a green
rerun. Its logs and generated artifacts must be retained before any rerun. CI
artifact names include the workflow attempt number, so a later attempt cannot
replace the first attempt's upload. A rerun supplements the original failure;
it never changes that attempt's conclusion or evidence.

A retry-to-green or quarantine is not permitted; nextest sets `flaky-result =
"fail"` and no retries.

FIG-515 is the case study for this rule. A real lease/replay signal was
dismissed as a flake at least four times in one day. Preserving the first
failure and requiring owned, expiring RCA metadata would have kept that signal
visible instead of allowing repeated green reruns to erase its significance.

## Why

Line coverage and flaky end-to-end-only tests do not establish confidence for
Lash's contracts. The high-value risks are invalid runtime states, durable
replay errors, duplicate ingress, retries, cancellation, lease loss, provider
failures, and backend drift. A single gate makes those risks visible and gives
CI and local development the same language for confidence.

## Consequences

- PR CI does not run the confidence gate (#1370 dropped the `fast:<shard>`
  lane). Local `scripts/confidence-gate.sh fast` runs the fast shards
  sequentially for a single-machine check. Local evidence defaults to
  `target/confidence/<worktree-slug>/`, where the slug includes an absolute-path
  checksum, so concurrent and same-basename worktrees cannot overwrite one
  another. CI explicitly sets `LASH_CONFIDENCE_OUT_DIR` to
  `target/confidence`, preserving its artifact contract; the variable remains
  an explicit override elsewhere.
- The `Confidence` workflow runs `full` on a weekly schedule and supports
  manual `default`/`broad`/`full` dispatch. The weekly run's per-package
  mutation legs are bounded rotating slices, so its mutation evidence is a
  sweep across runs, not a complete union at one revision.
- Releases require the latest scheduled `Confidence` run on main to succeed,
  finish within eight days (inclusive), and certify an ancestor of the release
  SHA or the SHA itself. Age uses the latest job completion timestamp, rather
  than mutable run metadata. Missing, red, stale, or unrelated evidence refuses
  release; an older green run cannot substitute. Release dispatch accepts
  `confidence_override_reason`: a non-blank reason explicitly bypasses only
  this precondition and is logged as a warning. Full-profile CI for the release
  SHA remains independently required (FIG-1160).
- Red weeklies do not automatically create tickets.
- `just confidence`, `just confidence-fast`, `just confidence-broad`, and
  `just confidence-full` are the local entry points.
- Missing tools are actionable failures with deterministic bootstrap commands.
  Use `LASH_CONFIDENCE_BOOTSTRAP=1` when a machine should install the required
  cargo subcommands.
- The durable fault matrix lives in
  `crates/lash-core/tests/runtime/tests/runtime_scenarios/fault_matrix.rs`; every
  row must point at an executable test or carry a concrete blocked rationale.
- `sim/backend-contention/backend-contention.json` records deterministic
  `RuntimePersistence` lease contention, stale completion fencing, reopen, and
  dead-owner reclaim evidence through SQLite and, when Postgres is configured,
  `lash-postgres-store` production-facing session-store APIs.
- Workflow artifact uploads are attempt-qualified. Operators must inspect and
  retain the first failing attempt even when a later attempt passes.

## Amendment (FIG-4125, 2026-09-29)

G2: [ADR 0044](0044-tests-must-be-independent-of-what-they-test.md) supersedes
the deterministic-simulation claim for lash-sim. Its randomised runs use virtual
skipped time and dump full history on checker failure; seeds alone do not
reproduce scheduling.

## Amendment (FIG-4153, 2026-09-29)

The weekly `full` run's per-package mutation evidence is bounded and rotating:
each `mutation-packages-rotating` leg judges the smoke canary plus one `--shard`
slice whose index derives from the leg coordinate and the run number. The
evidence artifacts — `mutation-evidence.json`, `confidence-summary.json`,
`mutation-shard.json`, and the run summary — record the leg, slice, run index,
and revision, and report `bounded_rotating` scope rather than `full`. `full`
remains reserved for an unsharded complete mutant union at one revision, which
only a local `scripts/confidence-gate.sh full` run produces; the rotating mode
was already in place and this amendment aligns the labels with it rather than
adding a mode.
