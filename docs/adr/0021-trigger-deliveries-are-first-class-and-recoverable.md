# Trigger deliveries are first-class and recoverable

## Decision

A Trigger Delivery is the reserved occurrence/subscription pair that starts a process. Provenance carries both identities. Emit reports each delivery's settled outcome, `Started` with its bound process or `Failed` with a reason. Reattachment returns the committed attempt's processes and outputs rather than a fresh reservation status. `TriggerIngressReceipt::realization` records whether the call coalesced onto an existing occurrence. TriggerStore exposes occurrence and delivery reads. A downstream start failure leaves the reservation available for recovery and is reported per delivery rather than failing the whole emit.

## Captured contracts and lifecycle

Admission captures the source constructor path, schema and resident/provider route on the subscription. Delivery validates against that captured contract and restores its route rather than consulting the current catalog. A reservation keeps the subscription snapshot it captures. Temporary route unavailability retries the same delivery; revocation refuses permanently. Engine targets are admitted through `ProcessEngineRegistry`, which supplies the authoritative signature and rejects unknown kinds.

Subscription lifecycle is `Enabled`, `Disabled` or `Tombstoned`, with deletion time carried by the tombstone. SQL constraints enforce the same shape. One `routable` predicate and one tombstone transition own routing and deletion. `target_label` is host presentation independent of the identity label.

## Recovery through obligations

The reservation transaction arms a `TriggerDelivery` obligation keyed by the occurrence/subscription pair under ADR 0109. Producer start/bind is the immediate attempt; the relay's due pass reads and recovers the retained reservation. It does not ingest the occurrence again. Binding records the minted process id and delivers the obligation in the same trigger-store write.

Start-key idempotence lasts while the process is retained under ADR 0107. Registration therefore pins the process to its delivery in the registration transaction. Prune cannot delete it between registration and bind. After bind commits, producer and recovery release the pin. A lost release conservatively retains the row; retention releases pins whose delivery is bound or absent before pruning. A permanent delivery refusal stalls `refused` visibly.

A binding outlives its process, so the binding, not the start key, answers a duplicate emission. Emit journals its admission of each delivery before it prepares a start: `Start` for a delivery the store answers unbound, `Bound` with the process for one it answers bound. Emit acts on the recorded admission alone. A fresh emission that meets a bound delivery reports `Started` with that process, even one retention has pruned, and registers, schedules and pins nothing. A replay of an emission that admitted a start serves that admission and then its recorded start, so its journal replays in shape although the store now answers the delivery bound. Relay recovery journals nothing and needs no admission.

## Cross-store retention protocol

Process retention commits a tombstone before delivery cleanup. Reconciliation snapshots exact `(occurrence_id, subscription_id, process_id)` identities, classifies tombstoned processes, revalidates at the action boundary, then deletes only matching observed delivery rows. A live row takes precedence over a stale tombstone. Each trigger-store delete is atomic and idempotent; the two stores need no distributed transaction.

Tombstone compaction surveys outstanding deliveries and excludes their process ids. The facade always supplies the configured TriggerStore and fails toward retention if it cannot survey it. The raw registry caller must supply that store too. Retained tombstones keep pending cleanup inert to recovery.

Wake sender floors survive process pruning and allocate events above the retained floor. A rewound sender without a surviving receiver row produces `ProcessWakeSequenceRewound` and a typed discard; valid retained retries remain idempotent. Trigger mutation receipts follow their separate retention contract under ADR 0023.

## Alternatives and consequences

Host repair of the reservation crash window is rejected because Lash owns delivery identity and the storage obligation. Reserving and starting in one transaction is rejected because process and trigger stores can be separate databases and start preparation performs external work. Product mappings remain host-owned. Artifact, lifetime and execution recovery contracts follow ADRs 0113, 0108 and 0110.

[Captured subscription types](../../crates/lash-core-execution/src/triggers.rs), [delivery start and recovery](../../crates/lash-core-execution/src/triggers/router.rs), [retention reconciliation](../../crates/lash-core-execution/src/runtime/process/registry.rs) and [registry pin contract](../../crates/lash-core-execution/src/runtime/process/registry_concerns.rs) implement the protocol.
