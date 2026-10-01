# Process transitions are events; the record is a fold

The process event log is durable transition history. Registration supplies the
immutable base; the stored `ProcessRecord` is its transactionally maintained
read projection. Every lifecycle mutation appends an event and saves the fold
in the same transaction. Replaying ordered events from the registration base
reproduces the record field-for-field.

## Context

First-started facts, wait transitions, external references, cancellation,
terminal outcomes and parks need one auditable transition path. Reads use the
stored projection instead of refolding history. A batch folds each event in
order as individual appends would, then saves the record once.

## Rendering and visibility

Wake subscription and observers are edge state, not lifecycle fields.
`wake_session_id` is indexed routing truth.
`process.subscription_retargeted` records its audit event, while retargeting
updates the edge and discards eligible pending deliveries transactionally.
Observer-added and observer-removed events audit the
`process_observers(session_id, process_id)` relation without changing process
lifecycle or extending retention.

Session deletion removes its observer edges and wake routing while retaining
process execution. It is a bulk session-lifecycle fact, not a fan-out of
per-edge events for a deleted endpoint. Process pruning removes child edges
with the process. Single-use session ids prevent delete-and-reuse ambiguity
(ADR 0049).

Rendering distinguishes a retained row, a host projection with its mandatory
prune watermark and a typed `NoLongerRetained` outcome. An absent retained
payload is not an empty process. Process identity is the minted id under
ADR 0107; scope lifetime is a registration fact under ADR 0108.

## 3. Host policy surface

Lash has one host trust domain and implements no authentication or
authorization policy. Observer-edge visibility and tool filters are query and
model-presentation rules.

Hosts explicitly choose initial process observers, session-creation and fork
observer selections, and later replay-keyed observer mutations. Session and
fork creation retain an observer intent before cross-store publication and
consume it after idempotent application. Opening reconciles an interrupted
publication. Typed unavailable, missing and pruned outcomes describe each
selection; history position and writer provenance do not select observers.

A factory-scoped `ProcessToolVisibilityFilter` applies only after observer
visibility, to session process tools. It is synchronous, infallible and
narrow-only; core intersects its returned ids with visible candidates. It
does not govern admin reads, projections, wake delivery, cleanup or pruning.
A run-local handle can address its process independently of the session tool
filter.

Input and process wakes compose under ADR 0101's FIFO admission policy.
Per-item merge metadata is not a composition gate. Wake delivery has its own
ownership token; retargeting discards work that has not entered delivery,
while a reclaimed in-flight delivery retains its original target. A stale
claimant cannot settle a successor's delivery. Typed delivery reports and
explicit redrive preserve host decisions about discarded ordering barriers.

Each claimed wake is attempted independently. Expiry is checked before reading
its source process, so an unreadable source cannot prevent expiry or a sibling's
delivery. A permanent source-read failure records the typed `source_unreadable`
discard. A transient failure releases its claim with bounded backoff; the next
attempt never moves beyond the original expiry. If a settlement write fails,
only that row waits for claim lapse, and the rest of the claimed page continues.
These delivery outcomes do not change the source process's lifecycle.

A `SourceUnreadable` head remains an ordering barrier. After repairing the
source, the host calls `redrive_wake_delivery` with the delivery id named by
`wake_delivery_report`.

A delivery whose process fact differs from the wake its receiver already holds
under the same process and sequence records the typed `content_conflict`
discard, once the receiver has refused it and raised its redelivery floor
(ADR 0101 §9). Like `sequence_rewound`, it is not an ordering barrier: later
deliveries in its group proceed, and it is never retried. Transient receiver
faults still release the claim with backoff.

Pruning and tombstone compaction require an explicit
`ProjectionWatermark::{UpTo, NoProjector}` choice. Session deletion does not
implicitly cancel processes; hosts compose cancellation policy explicitly.

## Shipped storage boundary

SQLite and PostgreSQL store lifecycle JSON beside indexed query fields,
observer edges and payload-free process tombstones. `ProcessStatus` is a
label-only lifecycle enum; terminal payloads live in `ProcessRecord::outcome`.
Continuation state uses the engine's scoped continuation contract.

## Consequences

A failed append cannot change the fold, and a replay key cannot create a
second transition. First-writer and write-once constraints are checked before
projection. Every registry must pass the record-refolding conformance law.

The best-effort event sink is observation; the durable event log is the
reconciliation source. Event-page consumers ignore unknown event kinds so
additional runtime facts remain additive. Counted observer receipts are
rejected because visibility does not own process retention.

## Implementation

- [Event projection and refolding](../../crates/lash-core-execution/src/runtime/process/validation.rs).
- [SQLite event transaction](../../crates/lash-sqlite-store/src/process_registry/support.rs) and [PostgreSQL event transaction](../../crates/lash-postgres-store/src/postgres/process_helpers.rs).
- [Registry concerns, visibility and retention](../../crates/lash-core-execution/src/runtime/process/registry_concerns.rs).
- [Record-fold law](../../crates/lash-conformance/src/conformance/process_registry.rs).
