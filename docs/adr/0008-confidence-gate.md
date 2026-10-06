# Confidence gate

## Status

accepted

## Decision

`scripts/confidence-gate.sh` is the executable confidence contract. Its selectors name the evidence being gathered:

- `fast` aggregates scenario, state-machine/property, simulation/provider, minimizer, fault-matrix and performance-guard shards. Each `fast:<shard>` is also a first-class command.
- `default` adds focused SQLite reproduction, backend conformance, coverage artifacts and targeted mutation evidence.
- `broad` adds PostgreSQL evidence when available, model replay, backend contention and bounded mutation. It claims bounded broad evidence.
- `full` requires broad semantics, coverage and full critical-crate mutation; non-full mutation scopes are refused.

The `mutation-packages-rotating` stages each judge a smoke canary and one rotating slice. Their manifests, sidecars and summaries report `bounded_rotating`, leg coordinates, run index and revision. A complete mutant union at one revision requires an unsharded local full run. A green rotating run is evidence about its slices.

## Failure evidence and tool policy

The first failing attempt's logs and artifacts remain evidence. Attempt-qualified artifacts prevent a rerun from replacing them. Retry-to-green and quarantine are refused; nextest uses no retries and `flaky-result = "fail"`.

Required tools are installed at their pinned versions when the gate runs. Any allowed bounded selector that omits coverage or mutation records `not_run`, never a passing result. Coverage supplies LCOV, missing-line text and JSON as a blind-spot map, without a percentage goal.

## Consequences

Confidence uses the storage matrix of SQLite file, SQLite memory and PostgreSQL. Laws run the production runtime over a fault-injecting store with labelled commits, a virtual clock and `SimNodes` ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §14). Upgrade evidence uses synthetic-next. Seeds choose simulation inputs; a failed concurrent run supplies its history, not a guarantee of a repeatable schedule.

Confidence runs locally on demand; no CI workflow runs it. The local entry
point is `just confidence-local` (`scripts/confidence-local.sh`), with its stage
plan in `scripts/confidence-stages.json` and a strict local conclusion. Release
automation validates only the release SHA's full-profile CI. Before cutting,
run the local Confidence runner on that SHA and record its summary in the
release checklist.

The [gate](../../scripts/confidence-gate.sh), the [local runner](../../scripts/confidence-local.sh) and the [release workflow](../../.github/workflows/release.yml) define executable policy. Scenarios and conformance supplement one another because neither coverage alone nor facade tests alone prove durable recovery.
