# Process transitions are events; the record is a fold

The process event log is the durable history of a process. The process record is
its transactionally maintained read projection, not a second source of truth.

The governing invariant is:

> No field of the process record may be written except as the projection of an
> appended event. Replaying the log into an empty record fold must reproduce the
> stored record field-for-field.

Registration supplies the immutable base of the fold. Every subsequent process
transition is an event. The store inserts that event and saves its projected
record in the same transaction. Reads continue to use the stored projection;
they do not refold the log.

The former `wake_target` exception is closed. Wake subscription is queryable
edge state in the indexed `wake_session_id` column, not a lifecycle-record
field. `process.subscription_retargeted` is its durable audit event; retargeting
updates the edge and discards pending deliveries to the old target in one
transaction. Session deletion clears the indexed edge without changing the
process record or event log.

This decision closes the split that previously let first-started facts, wait
entry and clearance, external references, and abandon requests mutate the
record without an event. Lifecycle events are reserved runtime facts with
deterministic replay identities. They are wake-inert unless a separate,
producer-declared event carries wake semantics.

## Context

The process-subsystem end state was subjected to three independent adversarial
reviews. Their consensus retained the existing process ontology and the
same-transaction event projection, while identifying the eventless lifecycle
mutations as the source of record/log divergence. The maintainer ratified the
result on 2026-07-26.

Seven rulings settle the wider end state:

1. Observation uses typed outcomes, weak observers, payload-free tombstones,
   and a mandatory prune watermark. Counted receipts are rejected.
2. Undeliverable wakes become typed durable discards — target gone, expired, or
   retargeted — visible in reports and re-drivable only by explicit host action.
3. Lash has one trust domain, the host. Correctness fences and tool-layer
   visibility remain; a separate authorization apparatus does not.
4. Constant and selector dedupe-key variants are deleted.
5. Session deletion always retains process execution. Hosts compose any
   cancellation policy explicitly.
6. Maximum attempts are producer-declared at registration beside recovery
   disposition, not configured as a factory knob.
7. The delivery sequence is staged: the early process waves may proceed with
   the current parallel work, while later delivery and observer waves wait on
   their prerequisite contracts and land as a stacked change.

## Supersession and rendering

The durable-core design's “Grants are a counted reference” heading is
superseded by the weak-observer ruling. A live observer does not extend the
retention lifetime by holding a counted receipt.

The durable-core fork-rendering requirement is discharged by a three-layer
story: a live row while retained, a host projection carrying a mandatory prune
watermark after projection, and a typed “no longer retained” result once
neither layer can render the process. Absence is therefore explicit rather than
being confused with an empty or inaccessible process.

Visibility is likewise edge state rather than fold state. The
`process_observers(session_id, process_id)` relation is query truth, while
`process.observer_added` and `process.observer_removed` are replay-keyed audit
events. Observer removal never changes lifecycle or retention. Session ids are
single-use under [ADR 0049](0049-session-ids-are-used-once.md), so an observer
edge cannot suffer delete-and-reuse ABA ambiguity.

Forks select observer inheritance explicitly: `All`, `None`, or
`Only(process_ids)`. The selected ids are stored in the durable fork relation
as pending apply intent. Publishing the fork precedes idempotent observer-event
application, and session open replays any uncleared intent before clearing it.
This gives hosts customizable branch visibility without coupling observation
to wake routing.

Session deletion is the deliberate exception to per-edge observer audit events:
it removes all observer rows and wake routing owned by that session without
appending `observer_removed` or subscription-retarget events. The deletion is
one bulk session-lifecycle fact; the audit lane records addressability changes
while both endpoints remain addressable, rather than fan-out events for a
session that no longer exists. Process pruning likewise removes its child edge
rows with the process.

Wake retargeting and session deletion discard only deliveries that have not
entered the durable `enqueuing` claim state. A claimed delivery settles
truthfully against its original target; retargeting bounds work not yet in
flight. If a driver crashes after claiming, bounded stale-claim recovery mints
a new ownership token. The old claimant can no longer settle or defer that
delivery. Recovery may still truthfully deliver the reclaimed wake to the
retargeted-away target: retargeting bounds deliveries that were not already in
flight, and receiver high-water deduplication absorbs any retry.

A discarded group head remains an ordering barrier: skipping it would let the
receiver high-water mark absorb a gap. `wake_delivery_report` therefore names
each blocked `(target_session_id, process_id)` group, the discarded head and
reason, and the delivery id to pass to `redrive_wake_delivery`. A
retargeted-away group receives no new deliveries, so any block there is moot.
The live operational case is an `Expired` head on a current target; the host
redrives that named head explicitly.

After pruning, observer rows no longer exist. Consequently, a caller that
guesses any retained tombstone id can receive its terminal label and prune
timestamp through the typed no-longer-retained result; that informational
probe cannot prove the caller was formerly an observer. Lash's single-host
trust-domain ruling accepts this limited disclosure.

## Shipped storage boundary

The reject-and-recreate schema stores lifecycle JSON beside extracted,
indexed query columns: originator id, wake session, identity kind and
label, waiting, timestamps, status, and change sequence. It adds
`process_observers` with a composite session/process key and reverse index, and
payload-free `process_tombstones` carrying the deletion change sequence.
Segment handovers remain in their existing tables but are exposed only through
the substrate-scoped `ProcessContinuationStore`.

`ProcessStatus` is the sole label-only lifecycle enum. Terminal payloads live
in `ProcessRecord::outcome`; list and projection queries can filter lifecycle
without decoding those outputs. SQL backends push every `ProcessListFilter`
predicate into the query.

## Consequences

Lifecycle retries deduplicate through deterministic replay keys. A failed
append cannot change the record, and an exact replay cannot create another
transition. First-writer-wins and write-once rules remain model constraints
enforced before projection.

Every process-registry backend must pass the same conformance test: after each
registration, lifecycle transition, signal, cancellation request, terminal
outcome, replay, and failed append, folding the complete event log from the
registration base must equal the stored record field-for-field.

External consumers of `events_after` observe additive reserved event kinds,
including lifecycle transitions, observer audit events, and subscription
retargets. Consumers must ignore unknown event kinds so future runtime facts
remain additive. The best-effort `ProcessEventSink` emits these events as well;
the durable event log remains the reconcile source.
