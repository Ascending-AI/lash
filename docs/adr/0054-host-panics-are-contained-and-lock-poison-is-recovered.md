# Host panics are contained and standard-lock poison is recovered

## Status

Accepted.

## Context

Provider and tool implementations are host-supplied dynamic code. A panic at
either call seam must not unwind through production turn orchestration, but the
same panic must remain loud in simulation and confidence harnesses. Separately,
standard-library lock poisoning only proves that a guard was held during an
unwind; the workspace had accumulated incompatible panic, typed-error, and
recovery policies for that condition.

Containment also creates child-task join boundaries. If those joins collapse a
panic into a generic task failure, production and simulation commit different
typed outcomes for the same host defect. In-memory store transactions add a
related constraint: recovering a poisoned guard is safe only while a critical
section cannot call arbitrary host code and unwind again through partially
updated state.

## Decision

- Lash has exactly two dynamic containment seams: `Provider::complete` and
  `ToolProvider::execute`. They convert a panic into the non-retryable typed
  outcomes `provider_panicked` and `tool_panicked`.
- Child/effect task joins inspect `JoinError::is_panic`, preserve those typed
  panic outcomes rather than generic join failures, and apply loudness only
  after the typed result has been formed. Cancellation remains a distinct join
  failure.
- Loudness is one process-scoped runtime flag. Production leaves it disabled;
  test harnesses, lash-sim, and confidence/runbook binaries enable it explicitly
  at startup. Cargo features do not select panic behavior, so one resolved
  artifact has identical typed semantics in every workspace feature graph.
- Every poisoned `std::sync::Mutex` or `RwLock` acquisition recovers the guard
  with `PoisonError::into_inner`. Poison is not a typed error tier. The shared
  `lash_sansio::sync` traits, re-exported through `lash_core::sync` and
  `lash::sync`, are the canonical acquisition vocabulary.
- In-memory store write transactions deliberately recover poison. Their
  critical sections must remain free of host-supplied code. Host clock reads and
  any other dynamic calls happen before the transaction lock is acquired, and
  only inert values cross into the closure.

## Consequences

- Workspace feature unification cannot make production-style containment tests
  fail or silently change committed failure codes.
- Harnesses still observe panics when they opt into loud mode, while durable and
  in-memory records retain the same typed outcome as quiet mode.
- A contained panic says nothing about the host object's own mutable invariants;
  provider and tool hosts own replacement or repair before reuse.
- Lock recovery stays uniform and greppable. Domain invariants are repaired by
  the operation that owns them rather than by a generic poison-error taxonomy.
- Adding host calls to an in-memory write transaction violates this ADR and
  requires hoisting those calls out of the critical section.
