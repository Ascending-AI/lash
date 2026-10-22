# Retained process owners

A claimed process keeps its acknowledged registry record, driver, execution
root, active run facts and immutable deferred resolution winners under the same
`OwnerState` revision. Every successful commit adopts its complete process view
before another pass can use it. A refused, uncertain, cancelled or invalid
acknowledgement invalidates that projection; recovery loads one coherent view.
Succession still releases the old claim so the next claim builds the new
composition.

Warm passes read the fenced external frontier: bounded mailbox entries, cancel,
external reference, registry event sequence and registry update time. Those
registry facts can change without advancing actor revision. The store refreshes
them again under the commit fence before projecting the actual driver/root and
registry reducer results into `ActorCommit.process_views`. Retained process
commits require exactly one matching view and no session-head receipt;
nonretained commits return no process views. No successful postimage requires a
second full record read.

Step outcomes checkpoint their original ordered namespace publications into the
existing process driver, in the outcome transaction. Private candidate pages
remain subject to the store's graph and manifest checks. Cold composition uses
this current namespace state plus only active runs; it never folds completed
process history. This changes the existing baseline driver shape in place.

Deferred resolution is immutable by execution and operation use. The owner
adopts the winner, holds, claim debt and actor acknowledgement as one transition.
A resident winner needs no store lookup or resolver call. Cold resume loads the
recorded winner before considering discovery. Refused and unavailable outcomes
stay pinned; an infrastructure discovery fault leaves no pin and can retry.
Every invocation still fits its own arguments and records its own admission.
Dispatch and read operations keep their per-invocation behavior.

Quiet process owners, including child-session processes, use the host's required
`OwnerRetentionSettings`: idle duration and resident-owner count. Active bodies
and uncommitted work retain the claim until they can reach a durable boundary.

Hosts that implement `DurableStore` must provide `begin_process_retained` with
bounded mail and the scalar `ProcessFrontier` facts. `ExecutionResolutions`
implementations must return the exact actor acknowledgement through
`commit_acknowledged`, including the Existing-winner path. A `ResolutionOwner`
uses `process_view: None` outside a retained process; retained callers carry their
authenticated view under the unchanged expected revision. `ActorCommit`
constructors supply `process_views` (empty for other actors). First-use callers
pass their claimed `ActorContext`; component converters and engine runtime
construction consume that same context's acknowledged view. New typed causes are
`MailPageTooSmall`, `ProcessPostimage`, `ResolutionDiscovery` and
`StepRefusal::Resolution`.
