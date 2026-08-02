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
then asks the process registry which process ids are tombstoned and asks the trigger store to
delete only those delivery rows. The process store remains the authority for terminal-history
classification; the trigger store remains the authority for deleting delivery rows.

The protocol guarantees that a delivery for a live process is retained, including when a legally
re-registered process row shadows a stale tombstone. All backends implement that live-row
exclusion. It also guarantees that a delivery whose process has a retained tombstone is inert to
the recovery sweep while cleanup is pending: recovery classifies tombstoned ids as registered
history. Each trigger-store batch delete is atomic and idempotent, so a failure or ambiguous
result is repaired by re-running the coordinator. There is no distributed transaction and no
claim that both commits become visible simultaneously.

Recovery by re-run depends on the tombstone outliving the delivery. The public
`Processes::compact_tombstones` facade therefore runs delivery reconciliation before tombstone
compaction and aborts compaction if reconciliation fails. Hosts should use that facade rather
than compacting the raw registry when a trigger store is configured. This ordering ensures a
compacted tombstone cannot leave a delivery that recovery would later mistake for never-started
work.

Process ids are legally reusable after pruning. Once an id is reused, a delivery from the prior
incarnation and one associated with the new incarnation are indistinguishable by process id
alone. The contract consequently fails toward retention: the live process row shadows the stale
tombstone, reconciliation deletes nothing for that id, and any stale delivery may remain until
the live incarnation itself reaches a later terminal-retention cycle. Leaving that row is safe;
deleting a live incarnation's recovery evidence is not. No schema or identity change is required
for this conservative contract.
