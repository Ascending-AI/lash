# Trigger deliveries are first-class and recoverable

## Decision

A Trigger Delivery is the reserved occurrence/subscription pair that starts a process. Provenance carries both identities. Emit reports each delivery's settled outcome, `Started` with its bound process or `Failed` with a reason. Reattachment returns the committed attempt's processes and outputs rather than a fresh reservation status. `TriggerIngressReceipt::realization` records whether the call coalesced onto an existing occurrence. TriggerStore exposes occurrence and delivery reads. A downstream start failure leaves the reservation unbound and is reported per delivery rather than failing the whole emit; a later emission or host redelivery of the occurrence starts it.

## Captured contracts and lifecycle

Admission captures the source constructor path, schema and resident/provider route on the subscription. Delivery validates against that captured contract and restores its route rather than consulting the current catalog. A reservation keeps the subscription snapshot it captures. Temporary route unavailability leaves the same delivery unbound for a later start; revocation refuses permanently. Engine targets are admitted through `ProcessEngineRegistry`, which supplies the authoritative signature and rejects unknown kinds.

Subscription lifecycle is `Enabled`, `Disabled` or `Tombstoned`, with deletion time carried by the tombstone. SQL constraints enforce the same shape. One `routable` predicate and one tombstone transition own routing and deletion. `target_label` is host presentation independent of the identity label.

## The reservation starts its process

An emission is a store-local effect. Inside an actor it commits in the
transaction that records its tool outcome; a host emission is its own
transaction ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md)
§5). That transaction ingests the occurrence, reserves each matching delivery,
registers its process and binds the delivery to the minted id, so the start is
exactly once and no reservation waits on a relay. Reservation, registry and
trigger tables share one database on both backends (ADR 0132 §12). A permanent
delivery refusal records the delivery `refused` visibly.

Start-key idempotence lasts while the process is retained under ADR 0107.
Registration and binding commit together, so retention never sees a registered,
unbound delivery and needs no pin between them.

A binding outlives its process, so the binding, not the start key, answers a
duplicate emission. The emission's ingest reads the occurrence and each
reservation with its binding in its own transaction, and acts on that answer
alone. A fresh emission that meets a bound delivery reports `Started` with that
process, even one retention has pruned, and registers, schedules and binds
nothing. An emission that meets an unbound delivery starts and binds it in the
same transaction. A resumed actor reads the committed outcome and writes
nothing to the trigger store: an occurrence and a delivery retention has
reclaimed stay reclaimed.

Concurrent emissions of one occurrence race on the delivery row. The registrar
reads the delivery's binding in the transaction that checks the start key,
after the key found nothing. A bound delivery registers nothing and refuses as
`TriggerDeliveryBound`, and the emission reports `Started` with the bound
process (FIG-4369).

## Reclaimed occurrences stay reclaimed

A committed outcome answers a resumed emission. A host source that redelivers
runs the emission's ingest again on every redelivery: the tool-intent ingress,
and any host that redelivers an occurrence. Once retention has reclaimed the
occurrence, that ingest finds no row under the idempotency key, and nothing at
the caller can tell an emission that never ran from one that ran and was
reclaimed.

The trigger store answers it. Every delete of an occurrence (delivery reconciliation, the occurrence reclaim pass and the non-fired audit prune) writes a payload-free tombstone under the occurrence id in the same transaction. An ingest that finds no occurrence and a tombstone writes nothing and refuses as `TriggerOccurrenceReclaimed`: no occurrence, no reservation and no start. The refusal stays typed to the host. A turn's intent reports it as the intent's refusal code, and the tool-intent ingress as `ToolIntentIngressRefusal::TriggerOccurrenceReclaimed`.

Occurrence tombstones are never deleted automatically (FIG-4610). Lash cannot
observe a host's last redelivery: a host source can resubmit an occurrence
indefinitely.

`reclaim_trigger_occurrences(cutoff)` reclaims eligible terminal occurrences and writes their tombstones. It never deletes a tombstone, including at `u64::MAX`. The factory owns each tombstone under ADR 0067.

The host deletes tombstones explicitly through `Processes::forget_trigger_tombstones(written_before_epoch_ms)`. The store deletes exactly the tombstones written strictly before that store-clock timestamp and returns the number removed. By invoking it, the host vouches that its trigger source stops redelivering those occurrences. A later redelivery of a forgotten identity runs as a new occurrence; an identity whose tombstone remains is still refused as `TriggerOccurrenceReclaimed`. There is no fixed expiry horizon.

A store fault in the emission's transaction rolls it back and is never
recorded; the attempt recomputes from committed state. Only a typed refusal is
an emission's recorded outcome, so an outage never settles a started delivery
as failed.

## Cross-store retention protocol

Process retention commits a tombstone before delivery cleanup. Reconciliation snapshots exact `(occurrence_id, subscription_id, process_id)` identities, classifies tombstoned processes, revalidates at the action boundary, then deletes only matching observed delivery rows. A live row takes precedence over a stale tombstone. Each trigger-store delete is atomic and idempotent.

Tombstone compaction surveys outstanding deliveries and excludes their process ids. The facade always supplies the configured TriggerStore and fails toward retention if it cannot survey it. The raw registry caller must supply that store too. Retained tombstones keep pending cleanup inert to recovery.

Wake sender floors survive process pruning and allocate events above the retained floor. A rewound sender without a surviving receiver row produces `ProcessWakeSequenceRewound` and a typed discard; valid retained retries remain idempotent. Trigger mutation receipts follow their separate retention contract under ADR 0023.

## Alternatives and consequences

Host repair of a reservation crash window is rejected because Lash owns delivery identity. Relaying a reservation to a later start is rejected: the reservation, registry and trigger tables share one database, so one transaction leaves no window to repair. Start preparation that performs external work, such as artifact staging, happens before that transaction under ADR 0113. Product mappings remain host-owned. Artifact, lifetime and execution recovery contracts follow ADRs 0113, 0108 and 0110.

[Captured subscription types](../../crates/lash-core-execution/src/triggers.rs), [delivery start and recovery](../../crates/lash-core-execution/src/triggers/router.rs), [retention reconciliation](../../crates/lash-core-execution/src/runtime/process/registry.rs) and [registry pin contract](../../crates/lash-core-execution/src/runtime/process/registry_concerns.rs) implement the protocol.
