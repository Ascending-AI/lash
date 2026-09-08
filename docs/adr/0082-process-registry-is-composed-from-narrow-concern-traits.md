# 0082 — The process registry is composed from narrow concern traits

Date: 2026-09-08

## Status

Accepted

## Context

`ProcessRegistry` had grown into one 47-plus-method trait bundling every
process-store concern: registration, observer edges, the event log, lifecycle
transitions, tool-intent durability, the wake-delivery outbox, leases,
query/scan reads, and retention. A wide trait cannot be composed, decorated, or
partially reused: every backend had to re-implement all of it by hand, and a
decorator that only wanted to observe mutations (`WatchedProcessRegistry`)
still had to hand-forward every read, lease, wake, and retention method.

## Decision

The registry contract is a set of narrow, single-concern traits that a backend
composes (`crates/lash-core/src/runtime/process/registry_concerns.rs`):

- `ProcessQuery` — point reads, listings, the change feed, the recovery
  worklist, aggregates.
- `ProcessRegistrar` — registration and the durable external backend
  reference.
- `ProcessObserverRegistry` — observer edges, subscription targeting,
  session-scoped routing cleanup.
- `ProcessEventLog` — the per-process append-only event log.
- `ProcessLifecycle` — started fact, waits, abandon/departure markers,
  terminal completion, parent-end teardown plans.
- `ProcessToolIntents` — durable tool-intent submission admission and
  settlement.
- `ProcessWakeOutbox` — wake-delivery retention policy and the
  claim/settle/redrive protocol.
- `ProcessLeases` — the single-owner process lease protocol.
- `ProcessRetention` — physical reclamation of terminal rows and tombstones.
- `ProcessClockRebind` — rebinding a backend to the runtime clock at facade
  construction.

`ProcessRegistry` remains the composed contract: a supertrait bundle over all
ten concerns, blanket-implemented for any type implementing every one of them.
`Arc<dyn ProcessRegistry>` stays the uniform runtime handle; supertrait methods
resolve on the trait object unchanged, so consumers are unaffected.

Two deliberate read-dependency supertraits exist:
`ProcessObserverRegistry: ProcessQuery` (observer defaults resolve identity
and liveness through point reads) and `ProcessEventLog: ProcessQuery` (the
incarnation-pinned `*_ref` defaults resolve one exact incarnation before
touching the log). No other concern implies another.

Backends implement each concern in its own `impl` block. Decorators implement
only the concern(s) they intercept and delegate the remaining concerns
wholesale instead of hand-forwarding method-by-method.

## Consequences

- Out-of-tree backends implement the narrow traits; `impl ProcessRegistry for
  X` blocks no longer exist (the blanket impl owns that name). This is a
  source-level break for backend implementors and invisible to registry
  consumers.
- A wrapper or test double can now cover exactly one concern and be proven —
  at compile time — not to carry any other concern's obligations.
- The conformance suites keep taking `ConformanceProcessRegistry`
  (`ProcessRegistry + ProcessRegistryTestSupport`), unchanged.
- Registry semantics are unchanged: this decomposition moved method
  declarations and impl-block boundaries only.
