# Trigger deliveries are first-class and recoverable

A Trigger Delivery — the reserved (occurrence, subscription) pair that starts one process — was
persisted durably but invisible: `CausalRef::TriggerOccurrence` recorded only the occurrence
half of the delivery identity, `TriggerEmitReport` flattened deliveries to bare started process
ids, the `TriggerStore` trait exposed no reads over occurrences or deliveries, and a crash
between reserving a delivery and starting its process lost the delivery forever (replayed emits
skip already-reserved pairs). We decided the delivery is a first-class, observable, recoverable
substrate fact: process provenance carries `subscription_id` alongside `occurrence_id`, the
emit report returns per-delivery outcomes (started / already reserved / failed with reason),
the trigger store exposes occurrence and delivery reads, and recovery sweeps deliveries that
have no registered process and starts them idempotently (safe because delivery process ids are
deterministic and registration is idempotent by hash).

Two consequences are deliberate: `emit` never fails because downstream starts failed — it
reports per-delivery outcomes and a failed start leaves its delivery row for recovery to
retry; and retention eventually removes a delivery after its terminal process is represented
by a durable tombstone, because an unguarded surviving delivery row is indistinguishable from
the crash window and would let the recovery sweep resurrect completed work.

The alternative — hosts joining process → delivery at read time and running their own repair
sweeps — was rejected: every host would pay a per-process lookup to learn provenance lash
already knows, and a host repairing the substrate's own emit crash window is a layering
inversion. Host-agnostic by construction: occurrence, subscription, delivery, and process are
all lash-native concepts; product mappings built on top of `subscription_id` (e.g. a host's
release or run attribution) stay in the host.

## Amendment: cross-store retention protocol

The original same-transaction ruling assumed the process registry and trigger store shared one
transaction boundary. That is not a valid substrate contract: SQLite deliberately permits the
two stores to occupy separate databases, and other embedders may provide independently durable
implementations. Process retention therefore commits its process tombstone first. A coordinator
first snapshots each delivery row's `(occurrence_id, subscription_id, process_id)` identity, then
asks the process registry which process ids are tombstoned, revalidates that classification at the
action boundary, and asks the trigger store to delete only the exact observed rows whose complete
identity still matches. The process store remains the authority for terminal-history
classification; the trigger store remains the authority for deleting delivery rows. A stale
classification can therefore neither expand into a process-wide delete nor sweep in a replacement
delivery inserted after the survey.

The protocol guarantees that a delivery for a live process is retained, including when a legally
re-registered process row shadows a stale tombstone. All backends implement that live-row
exclusion. It also guarantees that a delivery whose process has a retained tombstone is inert to
the recovery sweep while cleanup is pending: recovery classifies tombstoned ids as registered
history. Each trigger-store batch delete is atomic and idempotent, so a failure or ambiguous
result is repaired by re-running the coordinator. There is no distributed transaction and no
claim that both commits become visible simultaneously.

Recovery by re-run depends on the tombstone outliving the delivery. Tombstone compaction therefore
takes the trigger store's complete outstanding-delivery process-id survey and structurally excludes
every matching tombstone inside the process registry's delete. The public
`Processes::compact_tombstones` facade reconciles first, then passes the configured trigger store to
the raw registry lever. The raw lever performs a fresh complete survey itself, so a process pruned
by another writer between reconciliation and compaction is protected by its still-outstanding
delivery. A tombstone may compact in the same cycle once its delivery is absent. This is a local
compaction invariant, not a call-order convention: a tombstone guarded by an outstanding delivery
is refused by the raw registry lever as well. The one exception is a caller that passes no trigger
store to the raw lever from a runtime that has one: the survey is then empty and nothing is
excluded. The `Processes::compact_tombstones` facade always passes the configured store, so the
invariant holds for every route a host reaches through the public surface.

The invariant fails toward retention. If a configured trigger store cannot be surveyed or
reconciled, the facade aborts compaction and tombstones accumulate until the trigger store recovers.
Proceeding would knowingly orphan recovery evidence, so there is no proceed-and-log escape hatch.

Process-id reuse after terminal pruning is safe. Process-event sequence allocation is
`max(MAX(events) + 1, sender_floor + 1)`. The sender floor is one durable row per
`(target_session_id, process_id)`, advances in the same transaction as an event append, survives
process pruning and tombstone compaction, and is deleted with the target session. Sequences remain
small and ordered within an incarnation, while reuse always starts above the surviving floor. A
live process row still shadows its stale tombstone so retention never destroys recovery evidence,
and reconciliation revalidates that state immediately before action. Delivery deletion remains
bound to the row identity captured before classification rather than process id alone.

The receiver allocation fence is defense in depth for restoring or rewinding a sender store behind
the receiver. A wake with no live receiver row at or below the retained floor returns
`ProcessWakeSequenceRewound`; `WakeDeliveryDriver` terminalizes it as a typed
`sequence_rewound` discard with sequence and floor evidence, then continues the ordering group.
Receiver retries with a surviving live source row settle idempotently.

FIG-2502 implements `RetentionBound` for runtime commit receipts and usage
evidence, with terminal-session gating and atomic dependent-root reconciliation
(ADR 0023). It does not authorize age-only reclamation of host/platform trigger
mutation receipts or change trigger-delivery lifecycle ownership.

## Amendment: a subscription carries its own source contract and route

A subscription used to name its source only by `source_type` plus a free-form `source`
document, and resolved the meaning of that name against whatever the live catalog said at
delivery time. That is a recovery hole. The registrant's session is gone by the time a
delivery runs; a catalog edit, a re-linked module, or a provider whose grant moved silently
changed what the subscription meant, and the change was invisible — delivery either validated
against a contract nobody registered against, or "succeeded" into a route that no longer
existed.

A registration therefore captures, at admission time and durably on the subscription row, the
source contract it was admitted against and the provider route it was admitted through:
the constructor path, the config schema, and either the resident marker or the provider id
plus its opaque route document. Delivery validates the occurrence against the *captured*
contract, never the current catalog, and restores the *captured* route rather than
re-resolving the source name. A reserved delivery keeps the subscription snapshot it reserved
with, so a later catalog edit or explicit `Update` cannot rewrite work already in flight.

Route restoration distinguishes two failures, and the distinction is the whole point: a
temporarily unavailable provider is retryable — the delivery stays reserved, and a retry
resumes the same delivery identity and the same deterministic process id — while a revoked
route refuses visibly and permanently, with no re-resolution and no false success.

The same admission boundary runs the engine target through `ProcessEngineRegistry`
(FIG-1522): a subscription naming an engine kind this host never registered is refused at
registration with the registry's typed `UnknownEngine` refusal, and a registration naming a
registered engine stores the engine's authoritative signature rather than the claim that
arrived. A dead target is now a registration-time error rather than a first-delivery mystery.

The capture is new durable data, not a reshaping of existing data, so it lands as a
reject-and-recreate cutover: the store schema versions, the remote protocol window, the module
artifact host requirements, the durable-read fixture and the trigger definition fingerprint
family all move together, and a pre-capture row is refused at decode rather than defaulted
into a contract nobody ever admitted. There is no shim and no automatic store reset.
