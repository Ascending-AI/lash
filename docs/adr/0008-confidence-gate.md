# Confidence gate

## Status

accepted

## Decision

`scripts/confidence-gate.sh` is the executable confidence contract. Its selectors name the evidence being gathered:

- `fast` aggregates scenario, state-machine/property, simulation/provider, minimizer, fault-matrix and performance-guard shards. Each `fast:<shard>` is also a first-class command.
- `default` adds focused SQLite reproduction, backend conformance, coverage artifacts and targeted mutation evidence.
- `broad` adds PostgreSQL evidence when available, model replay, backend contention and bounded mutation. It claims bounded broad evidence.
- `full` requires broad semantics, coverage and full critical-crate mutation; non-full mutation scopes are refused.

The weekly workflow's `mutation-packages-rotating` legs each judge a smoke canary and one rotating slice. Their manifests, sidecars and summaries report `bounded_rotating`, leg coordinates, run index and revision. A complete mutant union at one revision requires an unsharded local full run. A green rotating run is evidence about its slices.

## Failure evidence and tool policy

The first failing attempt's logs and artifacts remain evidence. Attempt-qualified uploads prevent a rerun from replacing them. Retry-to-green and quarantine are refused; nextest uses no retries and `flaky-result = "fail"`.

Required tools fail a lane when absent; `LASH_CONFIDENCE_BOOTSTRAP=1` installs pinned tools. Any allowed bounded selector that omits coverage or mutation records `not_run`, never a passing result. Coverage supplies LCOV, missing-line text and JSON as a blind-spot map, without a percentage goal.

## Consequences

Confidence uses the storage matrix of SQLite file, SQLite memory and PostgreSQL. Host laws distinguish the in-process Restate server double, live Restate and lash-sim's in-process effect host. Upgrade evidence uses synthetic-next. Seeds choose simulation inputs; a failed concurrent run supplies its history, not a guarantee of a repeatable schedule.

The Confidence workflow runs weekly and supports manual selectors. Releases require the latest scheduled main run to succeed, certify an ancestor of the release SHA and complete within eight days. An explicit non-blank override reason bypasses only that precondition; release-SHA full-profile evidence remains independently required. No automatic ticket creation follows a red weekly.

The [gate](../../scripts/confidence-gate.sh), [workflow](../../.github/workflows/confidence.yml) and [release workflow](../../.github/workflows/release.yml) define executable policy. Scenarios and conformance supplement one another because neither coverage alone nor facade tests alone prove durable recovery.
