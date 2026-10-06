# Tests must be independent of what they test

## The rule

Do not derive a test's expected value by running the implementation under test
or duplicating the transformation that can be wrong. Independence concerns
the code path, not authorship. Author-written literal expectations, state
invariants, classifications and API contracts are valid oracles.

Implementation-free invariants describe required outcomes without reproducing
the implementation. A caller-shaped emission invariant can detect a missing
effect; a hand-built record cannot prove that production writes it.

Crash-cut execution runs a scenario live, cuts it at a labelled commit, resumes
it on another owner, then compares committed state and counts body executions
per admitted identity and outcome lookups per resume
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §2). A
scenario that is never cut proves no recovery property.

Captured provider traffic and prompt snapshots give independent evidence for
observed behavior. Synthetic fixtures remain appropriate when a specified wire
shape, parser branch or protocol metadata is the oracle. Escaped-defect
records cannot measure prevented defects, and ordinary unit tests can discover
unanticipated internal failures.

## Where a durability test stands

A durability test enters above the emission point through the public API and
runs the production runtime over a fault-injecting store that labels every
commit, with a virtual clock and `SimNodes` (ADR 0132 §14). The store matrix
is SQLite file, SQLite memory and PostgreSQL. SQLite memory is SQL storage,
not a separate map-backed store. Upgrade proofs use
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
exhaustive scheduler. Such a scheduler cannot cover SQLite threads, PostgreSQL or provider
transport; adding it does not replace evidence at
those boundaries.

A law injects faults and pauses only at trait seams. At the store and
deployment seams it arms a `Script`, generated from the operation lists; at any
other injected trait it implements (the stalled execution, an effect layer, the
provider) it holds the call at a `Gate`. Runtime code
carries no test hook. An in-process race with no trait between its steps stays
covered by real-timing laws and the soak; a law that must order it extracts
that boundary as an injected trait first.

A store may carry a pause or a step observer at a point inside one of its own
transactions, snapshots or open-time procedures, where no trait can be drawn
because both sides are statements on one connection. The list is closed:

- the writer fence: PostgreSQL's `AfterFence` and SQLite's
  `SqlitePauses::pause_after_fence` hold a transaction that has read the fleet
  epoch and not yet written;
- SQLite's two read-snapshot pauses, between a parent row's read and its
  children's (`pause_queued_work_hydration`,
  `pause_process_event_page_after_identity`);
- SQLite's open-time migration steps (`SqliteMigrationHook`) and finalize's
  commit (`SqliteFinalizeHook`).

Each compiles only under the store's `testing` feature or its own unit tests.
None of them faults an operation of the store traits; a law that needs a store
call to fail, lose its reply or wait arms the `Script`. The migration hook may
crash or fail the step it observes, which is a step of open and not a store
operation. Adding a point means adding it here.

## Deletions

A claim that a test cannot fail needs mutation evidence: break the production
behavior the test claims to guard and observe whether it goes red. Review
alone is insufficient. A justified deletion states the claimed property and
whether other evidence covers it or the property is explicitly unclaimed.

## Consequences

Choose evidence for the property at risk. Keep ordinary unit tests unless they
mechanically duplicate their implementation. Add caller-shaped emission laws
and small explicit crash-cut scenarios where they prove missing coverage.

A simulation or CI failure remains a finding until root-cause analysis proves
otherwise. Calling it flaky requires evidence, not a successful rerun. ADR 0008
owns retention of the first failing artifact; this ADR owns the credibility of
the oracle.

## Implementation

- [Script](../../crates/lash-core-store/src/testing/script.rs) and [Gate](../../crates/lash-core-store/src/testing/gate.rs), over the [store](../../crates/lash-core-store/src/store/runtime_store_decorator.rs) and [deployment](../../crates/lash-core-execution/src/runtime/deployment_store_decorator.rs) operation lists.
- [Simulator backend faults](../../crates/lash-sim/src/backend_fault.rs) as script arms, and the in-store points of [SQLite](../../crates/lash-sqlite-store/src/testing.rs) and [PostgreSQL](../../crates/lash-postgres-store/src/postgres/testing.rs).
- [Effect window invariant](../../crates/lash-sim/src/invariants/effect_window.rs) and [virtual clock](../../crates/lash-sim/src/clock.rs).
- [Store gate matrix](../../scripts/ci/store-tests.sh).
- [Mutation gate stages](../../scripts/ci/confidence-stage.sh) and [synthetic upgrade features](../../crates/lash-upgrade-harness/Cargo.toml).
