# Services are stateless; engines own continuation

A service instance is stateless with respect to correctness across an effect
boundary. In-memory turns, watch hubs and caches exist, but committed steps
live in SQL state or the engine journal. Sticky sessions are an optimization,
not correctness authority. The session-head CAS and sealed drive fence govern
session mutations and admission (ADR 0101).

Streaming is in memory until the next recorded effect or checkpoint. A crash
can cost re-execution of unrecorded work. It cannot authorize a fresh execution
of work whose retained start proves that its journal is lost.

## Once in flight, the substrate owns it

Restate is the production effect engine; SQL stores are storage (ADR 0104).
The engine owns invocation continuation, crash redrive, retry policy and
backpressure. Lash does not restart a started process from scratch. The
`ProcessStart` obligation relay submits work to the engine and executes no
process body (ADR 0109). Waiting work suspends through engine operations.

Engine-owned commit backpressure is an operation-level controller fact,
`owns_commit_backpressure`. Core uses that contract rather than guessing from
a backend name. An engine's retries and concurrency policy are not duplicated
by a second runtime attempt budget or scheduler.

## Conformance is the contract

Effect-host, work-driver, process-registry and differential replay laws define
what an engine implementor must supply. A correctness property absent from
conformance is a gap in Lash's contract. Conformance remains maintained product
code rather than an informal example.

The current store matrix is SQLite file, SQLite memory and PostgreSQL. Hosts
are the Restate server double, live Restate and lash-sim's in-process effect
host. The test doubles exercise the journal contract over SQL storage.

## Consequences

Correctness state held only in instance memory across a committed boundary is
a defect. A recorded outcome settles as recorded; a live fault remains engine
retry work. Replay divergence parks rather than publishing a fabricated
terminal outcome. Recovery is engine-owned under ADR 0110.

## Considered and rejected: durable partial assistant streams

Persisting every assistant delta in session continuation storage adds a durable
write per provider event to buy reconnect display. It is rejected because
observation history is not session continuation authority. A host that needs
crash-surviving preview activity supplies durable observation retention rather
than adding token-by-token session commits.

## A Restate segment never restarts started work

Every process segment establishes admission before any effect:

1. A read-only journaled verdict inspects the retained start marker. A marker
   for a lost execution refuses fresh execution with `SubstrateLost`. With no
   marker, the verdict records an OS-random nonce. An already-completed
   segment is ignored.
2. A separate journaled start writes the marker set-if-absent. The same nonce
   identifies this execution's own retry; a different nonce proves another
   execution started it and refuses with `SubstrateLost`.
3. Effects require the sealed `SegmentStarted` proof returned by start.

Drawing and writing in one retryable step is rejected because a retry could
mint a different nonce and refuse its own committed marker. An invocation id
or engine-context RNG cannot prove journal continuity after lost engine state.

A journaled verdict survives a crash before start. Loss of journal before a
marker permits fresh admission because no effect can precede it. Loss after
the marker refuses with zero redispatch, even if no effect actually ran. A
false abandonment is the accepted direction; a duplicate execution is not.

Recovery resubmits a live row under its current segment key. The engine
coalesces a live invocation or retained journal; a missing journal encounters
the admission proof. External invocation references are observation. The
handler's generation sentinel and successor windows govern which build may
continue it (ADRs 0043 and 0106).

## Implementation

- [Process segment admission](../../crates/lash-restate/src/process/admission.rs) and [engine submissions](../../crates/lash-restate/src/process/mod.rs).
- [Controller backpressure contract](../../crates/lash-core-execution/src/runtime/effect/executor/control.rs).
- [Failure classification](../../crates/lash-core-store/src/runtime_error/classification.rs).
