# 0065: Concurrent settlement is recorded by the logical Run

## Status

Accepted. Concurrent settlement follows the Run-owned contract in
[ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md). The
[arc-baseline text](https://github.com/Ascending-AI/lash/blob/48f11c5fa761cabb4991e8497218bb870db83169/docs/adr/0065-concurrent-settlement-is-a-durable-group-at-the-effect-host-seam.md)
records the group-service design this decision rejects.

## Decision

The sole active admitted Run segment records membership, independent attempts,
selection, final-or-cancel decisions, rank, protected drain and consumed prefixes
in its opener journal. Replay follows those facts rather than racing ready
futures again. There is one execution journal; SQL stores hold domain state.

Whole-round admission fixes all leaves, aliases and positions before any body
starts. An immediate winner cannot leave a pending sibling unowned. Each X has
its own durable receipt. Replay registers the recorded command prefix before
waiting on an older unresolved handle, so an unfinished sibling cannot hide a
recorded retry, timer or later attempt.

Consumer policy retains the distinctions between `race`, `any`, `all`,
`allSettled` and a list batch. `race` selects the first decision; `any` waits
for success; `all` rejects early; `allSettled` returns source order. The list
batch waits for every leaf and reports its first rejection in written order.
Duplicate aliases consume one unique call. Empty aggregates retain their
language semantics; an empty race never fabricates a result.

Timer leaves and admitted effect handles use `AggregatePlan`, recorded timer
admission and the Run's selection schedule. `GroupWakePolicy` remains a sansio
consumer-policy enum; it is not a service or execution owner.

An early result leaves losing calls live under the logical Run. Program effects
continue beside them. Only the logical terminal records Closing and discharges
cancellation and protected finals. A physical cut quiesces local attempts and
transfers the entire Run, including unconsumed decisions and source seals;
it never closes the logical owner. Retirement waits for all recovery, consumer
and material dependencies and retains an identity fence.

## Evidence

The [tool-run contract](../architecture/tool-run-contract.md) pins K1/K3/K6/K9
and L03-L06/L09/L13/L16-L18. `RunCoordinator` implements those contracts in
`crates/lash-core-execution/src/tool_dispatch/run_coordinator/`.
`crates/lash-restate/src/tests/run_coordinator_on_the_double/aggregate.rs`
exercises aliases, timers, early rejection, surviving losers and logical close;
`run_coordinator_on_the_double.rs` holds the transitive protected-drain oracle.

## Consequences

An aggregate has durable selection without an independent rank object, payload
service or dispatcher. Independent attempt replay avoids rerunning completed
siblings. Selection, presentation and incorporation remain separate recorded
facts, and a consumer cannot destroy work that its logical owner still owes.
