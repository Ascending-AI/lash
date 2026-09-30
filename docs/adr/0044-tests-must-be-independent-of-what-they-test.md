# Tests must be independent of what they test

## The rule

Do not derive a test's expected value by running the implementation under test
or duplicating the transformation that can be wrong. Independence concerns
the code path, not authorship. Author-written literal expectations, state
invariants, classifications and API contracts are valid oracles.

Implementation-free invariants describe required outcomes without reproducing
the implementation. A caller-shaped emission invariant can detect a missing
effect; replay of a hand-built envelope cannot prove that production emits it.

Differential execution runs a scenario live and explicitly redrives its own
journal, then compares committed state and checks that local work is not
re-executed. A replay-capable host that is never redriven proves no recovery
property.

Captured provider traffic and prompt snapshots give independent evidence for
observed behavior. Synthetic fixtures remain appropriate when a specified wire
shape, parser branch or protocol metadata is the oracle. Escaped-defect
records cannot measure prevented defects, and ordinary unit tests can discover
unanticipated internal failures.

## Where a durability test stands

A durability test enters above the emission point through the public API and
uses a host that journals and replays. The current store matrix is SQLite
file, SQLite memory and PostgreSQL. The host matrix is the in-process Restate
server double, live Restate and lash-sim's in-process effect host. SQLite
memory is SQL storage, not a separate map-backed store. Upgrade proofs use
the `synthetic-next` tier.

## What is not a test

Types, visibility and lints can exclude a defect at every call site. Use them
when the constraint can be expressed directly, such as a controller-free tool
body or an explicit typed outcome. Tests then prove behavioral claims that
those constraints alone do not establish.

## Simulation

Lash-sim generates scenarios over real execution and minimizes failures.
An oracle that consumes observed runtime boundaries can establish a runtime
property. An oracle that verifies facts it computes from its own model proves
only the model's property and must be named accordingly.

Virtual time does not control Tokio interleavings. The clock advances scheduled
sleepers and uses bounded yields for unscheduled progress. Lash-sim is not an
exhaustive scheduler. Such a scheduler cannot cover SQLite threads, PostgreSQL,
live Restate or provider transport; adding it does not replace evidence at
those boundaries.

## Deletions

A claim that a test cannot fail needs mutation evidence: break the production
behavior the test claims to guard and observe whether it goes red. Review
alone is insufficient. A justified deletion states the claimed property and
whether other evidence covers it or the property is explicitly unclaimed.

## Consequences

Choose evidence for the property at risk. Keep ordinary unit tests unless they
mechanically duplicate their implementation. Add caller-shaped emission laws
and small explicit redrive scenarios where they prove missing coverage.

A simulation or CI failure remains a finding until root-cause analysis proves
otherwise. Calling it flaky requires evidence, not a successful rerun. ADR 0008
owns retention of the first failing artifact; this ADR owns the credibility of
the oracle.

## Implementation

- [Effect replay invariant](../../crates/lash-sim/src/invariants/effect_window.rs) and [virtual clock](../../crates/lash-sim/src/clock.rs).
- [Restate test host](../../crates/lash-restate-test/src/lib.rs) and [store gate matrix](../../scripts/ci/store-tests.sh).
- [Mutation gate stages](../../scripts/ci/confidence-stage.sh) and [synthetic upgrade features](../../crates/lash-upgrade-harness/Cargo.toml).
