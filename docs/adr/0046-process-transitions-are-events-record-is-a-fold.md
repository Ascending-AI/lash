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

## Consequences

Lifecycle retries deduplicate through deterministic replay keys. A failed
append cannot change the record, and an exact replay cannot create another
transition. First-writer-wins and write-once rules remain model constraints
enforced before projection.

Every process-registry backend must pass the same conformance test: after each
registration, lifecycle transition, signal, cancellation request, terminal
outcome, replay, and failed append, folding the complete event log from the
registration base must equal the stored record field-for-field.

Append concurrency controls, wake delivery, observers, grants, remote
protocols, and durable-core graph commits are separate decisions. This ADR does
not redesign the append API or storage schema.
