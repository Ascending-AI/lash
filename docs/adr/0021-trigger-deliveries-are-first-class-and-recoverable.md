# Trigger deliveries are first-class and recoverable

## Decision

A Trigger Delivery is the reserved occurrence/subscription pair that starts a process. Provenance carries both identities. Emit reports each delivery's settled outcome, `Started` with its bound process or `Failed` with a reason. Reattachment returns the committed attempt's processes and outputs rather than a fresh reservation status. `TriggerIngressReceipt::realization` records whether the call coalesced onto an existing occurrence. TriggerStore exposes occurrence and delivery reads. A downstream start failure leaves the reservation available for recovery and is reported per delivery rather than failing the whole emit.

## Captured contracts and lifecycle

Admission captures the source constructor path, schema and resident/provider route on the subscription. Delivery validates against that captured contract and restores its route rather than consulting the current catalog. A reservation keeps the subscription snapshot it captures. Temporary route unavailability retries the same delivery; revocation refuses permanently. Engine targets are admitted through `ProcessEngineRegistry`, which supplies the authoritative signature and rejects unknown kinds.

Subscription lifecycle is `Enabled`, `Disabled` or `Tombstoned`, with deletion time carried by the tombstone. SQL constraints enforce the same shape. One `routable` predicate and one tombstone transition own routing and deletion. `target_label` is host presentation independent of the identity label.

## Recovery through obligations

The reservation transaction arms a `TriggerDelivery` obligation keyed by the occurrence/subscription pair under ADR 0109. Producer start/bind is the immediate attempt; the relay's due pass reads and recovers the retained reservation. It does not ingest the occurrence again. Binding records the minted process id and delivers the obligation in the same trigger-store write.

Start-key idempotence lasts while the process is retained under ADR 0107. Registration therefore pins the process to its delivery in the registration transaction. Prune cannot delete it between registration and bind. After bind commits, producer and recovery release the pin. A lost release conservatively retains the row; retention releases pins whose delivery is bound or absent before pruning. A permanent delivery refusal stalls `refused` visibly.

A binding outlives its process, so the binding, not the start key, answers a duplicate emission. Emit journals its ingest as one recorded step, whose outcome is the store's receipt: the occurrence, and each reservation with its binding. Emit acts on the recorded receipt alone. A fresh emission that meets a bound delivery reports `Started` with that process, even one retention has pruned, and registers, schedules and pins nothing. An emission that meets an unbound delivery starts it, then journals the bind, with its pin release, as a second recorded step whose outcome is the bound process. A replay serves the recorded receipt, then the recorded start and bind, so its journal replays in shape although the store now answers the delivery bound, and it writes nothing to the trigger store: an occurrence and a delivery retention has reclaimed stay reclaimed (FIG-4503). Relay recovery journals nothing.

The receipt holds each binding as the emission's ingest answered it. A start that another emission's bind, and a prune of the bound process, overtake between that ingest and its registration finds nothing under its key. The registrar therefore reads the delivery's binding in the transaction that checks the start key, after the key found nothing. A bound delivery registers nothing and refuses as `TriggerDeliveryBound`. The emission then journals an admission, which reads the binding and records `Bound` with its process, and reports `Started` with that process. Its replay serves the recorded refusal and then that admission. Relay recovery reads the binding again and binds nothing new. A SQLite store set attaches its trigger store to its registry's connection for this read; PostgreSQL reads the table in the same transaction (FIG-4369).

## Reclaimed occurrences stay reclaimed

A journal answers a replay. A host with no journal to answer from runs the emission's ingest again on every redelivery: the tool-intent ingress, whose redelivery arrives on a new invocation with an empty journal, and any host that journals nothing. Once retention has reclaimed the occurrence, that ingest finds no row under the idempotency key, and nothing at the caller can tell an emission that never ran from one that ran and was reclaimed.

The trigger store answers it. Every delete of an occurrence (delivery reconciliation, the occurrence reclaim pass and the non-fired audit prune) writes a payload-free tombstone under the occurrence id in the same transaction. An ingest that finds no occurrence and a tombstone writes nothing and refuses as `TriggerOccurrenceReclaimed`: no occurrence, no reservation and no start. The refusal stays typed to the host. A turn's intent reports it as the intent's refusal code, and the tool-intent ingress as `ToolIntentIngressRefusal::TriggerOccurrenceReclaimed`.

Occurrence tombstones are never deleted automatically (FIG-4610, superseding FIG-4573). Lash cannot observe a host's last redelivery: a journal-less retry can resubmit an occurrence indefinitely. Restate's journal and idempotency retention (ADR 0025) bound when a duplicate stops attaching to its first invocation, not when duplicates stop arriving.

`reclaim_trigger_occurrences(cutoff)` reclaims eligible terminal occurrences and writes their tombstones. It never deletes a tombstone, including at `u64::MAX`. The factory owns each tombstone under ADR 0067.

The host deletes tombstones explicitly through `Processes::forget_trigger_tombstones(written_before_epoch_ms)`. The store deletes exactly the tombstones written strictly before that store-clock timestamp and returns the number removed. By invoking it, the host vouches that its trigger source will no longer redeliver those occurrences. A later redelivery of a forgotten identity runs as a new occurrence; an identity whose tombstone remains is still refused as `TriggerOccurrenceReclaimed`. There is no fixed expiry horizon.

A store that does not answer an emission's ingest, its bind or its binding read is that attempt's fault and is never recorded: the engine runs the attempt again, which serves the steps already journaled and binds the process the first attempt registered. The pin holds that process until the bind commits, as it does when the attempt dies instead. Only a typed refusal is a step's recorded outcome, so an outage never settles a started delivery as failed.

An emission journals a step whether or not a subscription matches, so on Restate it runs inside a handler. A scope the effect host lends outside one refuses the emission at its ingest step, before the trigger store is reached, and writes nothing. Every production emitter passes its handler's controller.

## Cross-store retention protocol

Process retention commits a tombstone before delivery cleanup. Reconciliation snapshots exact `(occurrence_id, subscription_id, process_id)` identities, classifies tombstoned processes, revalidates at the action boundary, then deletes only matching observed delivery rows. A live row takes precedence over a stale tombstone. Each trigger-store delete is atomic and idempotent; the two stores need no distributed transaction.

Tombstone compaction surveys outstanding deliveries and excludes their process ids. The facade always supplies the configured TriggerStore and fails toward retention if it cannot survey it. The raw registry caller must supply that store too. Retained tombstones keep pending cleanup inert to recovery.

Wake sender floors survive process pruning and allocate events above the retained floor. A rewound sender without a surviving receiver row produces `ProcessWakeSequenceRewound` and a typed discard; valid retained retries remain idempotent. Trigger mutation receipts follow their separate retention contract under ADR 0023.

## Alternatives and consequences

Host repair of the reservation crash window is rejected because Lash owns delivery identity and the storage obligation. Reserving and starting in one transaction is rejected because process and trigger stores can be separate databases and start preparation performs external work. Product mappings remain host-owned. Artifact, lifetime and execution recovery contracts follow ADRs 0113, 0108 and 0110.

[Captured subscription types](../../crates/lash-core-execution/src/triggers.rs), [delivery start and recovery](../../crates/lash-core-execution/src/triggers/router.rs), [retention reconciliation](../../crates/lash-core-execution/src/runtime/process/registry.rs) and [registry pin contract](../../crates/lash-core-execution/src/runtime/process/registry_concerns.rs) implement the protocol.
