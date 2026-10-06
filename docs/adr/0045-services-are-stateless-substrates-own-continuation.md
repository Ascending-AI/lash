# Services are stateless; the store owns continuation

A service instance is stateless with respect to correctness across a committed
boundary. In-memory turns, watch hubs, caches and the owner's actor state
exist, but committed phases live in the lash store. The owner's cache is keyed
by `(actor, epoch)` and is never a grant: on any failed fence it is discarded
and state reloads from rows
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §3). Sticky
sessions are an optimization, not correctness authority. The session-head CAS
and the session actor's epoch fence govern session mutations and admission
(ADR 0101).

Streaming is in memory until the next committed phase or checkpoint. A crash
can cost recomputation of uncommitted work from the last committed state. It
cannot authorize a fresh execution of work whose started row is committed:
recovery follows that row's execution policy.

## Once in flight, the store owns it

Lash's durable engine runs over the lash store (ADR 0132 §1). Each session and
each process is an actor with one scheduling row; a node owns it under an
epoch, and a reaped node's actors are claimed by another node with an epoch
bump. Lash does not restart a started process from scratch: resume loads its
committed state and continues. Waiting work commits its last phase and releases
its actor; it holds no node.

Retry policy is data. A `Repeatable` execution records its `BoundedRetry`, and a
retry is a record with a due time (ADR 0132 §5 and §7). An actor whose claims
make no phase progress counts failed activations and parks with
`ActivationLoop` at its activation budget. No second runtime attempt budget or
scheduler duplicates these.

## Conformance is the contract

Store, work-driver, process-registry and crash-matrix laws define what a store
implementor must supply. A correctness property absent from conformance is a
gap in Lash's contract. Conformance remains maintained product code rather
than an informal example.

The store matrix is SQLite file, SQLite memory and PostgreSQL. Laws run the
production runtime over a fault-injecting store with labelled commits, a
virtual clock and `SimNodes` (ADR 0132 §14).

## Consequences

Correctness state held only in instance memory across a committed boundary is
a defect. A committed outcome settles as recorded; a live fault recomputes
from committed state. Stored data that cannot be decoded parks rather than
publishing a fabricated terminal outcome. Recovery follows ADR 0110.

## Considered and rejected: durable partial assistant streams

Persisting every assistant delta in session continuation storage adds a durable
write per provider event to buy reconnect display. It is rejected because
observation history is not session continuation authority. A host that needs
crash-surviving preview activity supplies durable observation retention rather
than adding token-by-token session commits.

## Started work never restarts

A process's started execution is a committed row. A `Once` execution started
without an outcome records `Interrupted` and never runs again; a `Repeatable`
one runs again at its same ordinal (ADR 0132 §5). Nothing rebuilds a lost
history by running work fresh, because no history is re-run: the rows are the
state. A false `Interrupted` is the accepted direction; a duplicate `Once`
execution is not.

## Implementation

- [Failure classification](../../crates/lash-core-store/src/runtime_error/classification.rs).
- The substrate lanes implement actors, epochs and phase rows under ADR 0132.
