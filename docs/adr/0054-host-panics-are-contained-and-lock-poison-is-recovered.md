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
typed outcomes for the same host defect. The former in-memory store transaction
constraint is historical: ADR 0104 removed that effect-engine implementation.
Standard-lock poison recovery remains the shared lock acquisition contract.

## Decision

- Lash has two attempt containment seams: `Provider::complete` and
  `ToolProvider::execute`, which convert panics into non-retryable typed
  `provider_panicked` and `tool_panicked` outcomes. Auxiliary provider callbacks
  `close` and `reconcile_usage` also contain panics as non-retryable
  `ProviderPanicked`, with the same process-scoped loudness policy.
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
- Historical under [ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md):
  the removed in-memory store write transactions recovered poison and kept
  host calls outside their critical sections. This is not a current store API.

## Consequences

- Workspace feature unification cannot make production-style containment tests
  fail or silently change committed failure codes.
- Harnesses still observe panics when they opt into loud mode, while durable and
  in-memory records retain the same typed outcome as quiet mode.
- A contained panic says nothing about the host object's own mutable invariants;
  provider and tool hosts own replacement or repair before reuse.
- Lock recovery stays uniform and greppable. Domain invariants are repaired by
  the operation that owns them rather than by a generic poison-error taxonomy.
- The removed in-memory transaction rule imposes no current store API requirement.

## Amendment (FIG-4125, 2026-09-29)

Item 23:
[ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md)
supersedes obsolete SQL effect-engine assumptions. The host panic-containment
rule survives.

## Amendment (FIG-4163, 2026-09-30)

The two attempt seams coexist with auxiliary provider callback containment and child/effect task joins; the removed memory-transaction paragraphs are historical under ADR 0104.
[`ProviderHandle::close`, `reconcile_usage`, and `provider_close_panicked`](../../crates/lash-core-llm/src/provider/handle.rs)
form the typed callback failure before applying process-scoped loudness.
