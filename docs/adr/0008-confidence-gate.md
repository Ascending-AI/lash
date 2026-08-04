# Confidence Gate

## Status

accepted

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
  Docker bootstrap is available, generated SQLite/Postgres dynamic backend
  reruns, static model replay evidence for generated/minimized traces, backend
  contention evidence, and targeted mutation. It is not a true full confidence
  claim.
- `full`: true full confidence. It includes broad semantics and full
  cargo-mutants over the same critical crates; the lane refuses non-full
  mutation scopes.

Coverage is not a percentage goal. The gate writes LCOV, missing-line text, and
summary JSON under `target/confidence/<lane>/coverage/` so uncovered source is
reviewed as a blind-spot map.

Mutation testing is required for lanes that claim it. `cargo-mutants` absence
fails `default`, `broad`, and `full` unless `LASH_CONFIDENCE_BOOTSTRAP=1`
installs the pinned tool version. Mutation success is never faked as a skipped
pass, and a targeted bounded run is never labeled as `full`.

### Failure evidence and quarantine policy

The first failing attempt is evidence, not a disposable prelude to a green
rerun. Its logs and generated artifacts must be retained before any rerun. CI
artifact names include the workflow attempt number, so a later attempt cannot
replace the first attempt's upload. A rerun supplements the original failure;
it never changes that attempt's conclusion or evidence.

Retry-to-green and quarantine are temporary incident controls, not test
results. Either one requires an entry in `.github/test-quarantines.json` with:

- an accountable `owner`;
- an `issue_url`;
- an explicit `rca_status`; and
- an ISO `expires_on` date.

`scripts/check_test_quarantines.py` validates that contract and rejects expired
entries. The check runs in CI and the local push gate. A passing retry does not
permit the original failure to be classified as flaky: the issue must retain
the first-failure artifact, and the RCA must explain the failure before the
quarantine can be removed.

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

- PR CI runs the `fast:<shard>` commands in parallel with workspace tests and
  then validates a small aggregate `fast:summary` artifact. Local
  `scripts/confidence-gate.sh fast` runs the same shards sequentially for a
  single-machine check. Local evidence defaults to
  `target/confidence/<worktree-slug>/`, where the slug includes an absolute-path
  checksum, so concurrent and same-basename worktrees cannot overwrite one
  another. CI explicitly sets `LASH_CONFIDENCE_OUT_DIR` to
  `target/confidence`, preserving its artifact contract; the variable remains
  an explicit override elsewhere.
- The `Confidence` workflow runs `full` on a weekly schedule and supports
  manual `default`/`broad`/`full` dispatch.
- `just confidence`, `just confidence-fast`, `just confidence-broad`, and
  `just confidence-full` are the local entry points.
- Missing tools are actionable failures with deterministic bootstrap commands.
  Use `LASH_CONFIDENCE_BOOTSTRAP=1` when a machine should install the required
  cargo subcommands.
- The durable fault matrix lives in
  `crates/lash-core/src/runtime/tests/runtime_scenarios/fault_matrix.rs`; every
  row must point at an executable test or carry a concrete blocked rationale.
- `sim/backend-contention/backend-contention.json` records deterministic
  `RuntimePersistence` lease contention, stale completion fencing, reopen, and
  dead-owner reclaim evidence through SQLite and, when Postgres is configured,
  `lash-postgres-store` production-facing session-store APIs.
- Workflow artifact uploads are attempt-qualified. Operators must inspect and
  retain the first failing attempt even when a later attempt passes.
