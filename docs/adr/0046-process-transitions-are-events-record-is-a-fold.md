# Process transitions are events; the record is a fold

The process event log is durable transition history. Registration supplies the
immutable base; the stored `ProcessRecord` is its transactionally maintained
read projection. Every lifecycle mutation appends an event and saves the fold
in the same transaction. Replaying ordered events from the registration base
reproduces the record field-for-field.

## Context

First-started facts, wait transitions, cancellation,
terminal outcomes and parks need one auditable transition path. Reads use the
stored projection instead of refolding history. A batch folds each event in
order as individual appends would, then saves the record once.

## Rendering and visibility

Observers are edge state, not lifecycle fields. Observer-added and
observer-removed events audit the `process_observers(session_id, process_id)`
relation without changing lifecycle or extending retention. The closed
lifecycle vocabulary is defined by ADR 0136.

Session deletion removes its observer edges while retaining process execution.
It is a bulk session-lifecycle fact, not a fan-out of per-edge events for a
deleted endpoint. Process pruning removes child edges with the process.
Single-use session ids prevent delete-and-reuse ambiguity (ADR 0049).

Rendering distinguishes a retained row, a host projection with its mandatory
prune watermark and a typed `NoLongerRetained` outcome. An absent retained
payload is not an empty process. Process identity is the minted id under
ADR 0107; scope lifetime is a registration fact under ADR 0108.

## 3. Host policy surface

Lash has one host trust domain and implements no authentication or
authorization policy. Observer-edge visibility and tool filters are query and
model-presentation rules.

Hosts explicitly choose initial process observers, session-creation and fork
observer selections, and later idempotency-keyed observer mutations. Session and
fork creation retain an observer intent before cross-store publication and
consume it after idempotent application. Opening reconciles an interrupted
publication. Typed unavailable, missing and pruned outcomes describe each
selection; history position and writer provenance do not select observers.

A factory-scoped `ProcessToolVisibilityFilter` applies only after observer
visibility, to session process tools. It is synchronous, infallible and
narrow-only; core intersects its returned ids with visible candidates. It
does not govern admin reads, projections, host routing, cleanup or pruning.
A run-local handle can address its process independently of the session tool
filter.

The host chooses destinations and sends process notices only after their
source facts commit. It deduplicates each send with a stable turn identity;
[ADR 0136](0136-the-host-owns-events-routing-and-scheduling.md) owns delivery and retention. Observer relationships do not
route a notice or start a turn.

Pruning and tombstone compaction require an explicit
`ProjectionWatermark::{UpTo, NoProjector}` choice. Session deletion does not
implicitly cancel processes; hosts compose cancellation policy explicitly.

## Shipped storage boundary

SQLite and PostgreSQL store lifecycle JSON beside indexed query fields,
observer edges and payload-free process tombstones. `ProcessStatus` is a
label-only lifecycle enum; terminal payloads live in `ProcessRecord::outcome`.
VM continuation state is the process's snapshot under [ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §8.

## Consequences

A failed append cannot change the fold, and an idempotency key cannot create
a second transition. First-writer and write-once constraints are checked before
projection. Every registry must pass the record-refolding conformance law.

The best-effort event sink is observation; the durable event log is the
reconciliation source. Stored lifecycle facts are a closed typed vocabulary; malformed facts are
refused rather than interpreted as producer-defined events. Counted observer receipts are
rejected because visibility does not own process retention.

## Implementation

- [Event projection and refolding](../../crates/lash-core-execution/src/runtime/process/validation.rs).
- [SQLite event transaction](../../crates/lash-sqlite-store/src/process_registry/support.rs) and [PostgreSQL event transaction](../../crates/lash-postgres-store/src/postgres/process_helpers.rs).
- [Registry concerns, visibility and retention](../../crates/lash-core-execution/src/runtime/process/registry_concerns.rs).
- [Record-fold law](../../crates/lash-conformance/src/conformance/process_registry.rs).

[ADR 0136](0136-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
