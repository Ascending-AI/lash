# 0082: The process registry is composed from narrow concern traits

## Context

Process persistence has independent query, mutation, delivery, and retention
concerns. A backend or decorator should implement and test one concern without
hand-forwarding every unrelated operation.

## Decision

The registry contract has nine concern traits:

- `ProcessQuery` supplies point reads, listings, change feeds, bounded
  non-terminal pages, and aggregates.
- `ProcessRegistrar` registers a process and records its external backend reference.
- `ProcessObserverRegistry` owns observer edges and session routing cleanup.
- `ProcessEventLog` owns the append-only process event log.
- `ProcessLifecycle` records starts, waits, departures, completion, and parent-end teardown.
- `ProcessToolIntents` admits and settles durable tool-intent submissions.
- `ProcessWakeOutbox` owns wake obligations and their claim, settle, and redrive protocol.
- `ProcessRetention` reclaims terminal rows and tombstones.
- `ProcessClockRebind` binds the registry to the runtime clock.

`ProcessRegistry` composes all nine and `FleetFormatStore` as supertraits. A
blanket implementation covers types satisfying the complete bundle.
`Arc<dyn ProcessRegistry>` is the common runtime handle. Methods remain
available through the concern traits on that object.

`ProcessObserverRegistry` and `ProcessEventLog` depend on `ProcessQuery` for
identity and incarnation checks. Other concerns do not acquire unrelated
concern obligations. Backend implementations group methods by concern, and a
decorator delegates unintercepted concerns wholesale.

Process execution and recovery belong to the engine under ADR 0110; registry
concerns are persistence reads and writes. Wake-outbox claim tokens belong to
the obligation relay under ADR 0109.

The runtime-store decorator follows the same ownership rule. Its default
forwarder and component implementations derive from one `runtime_store_operations!`
list. Provided convenience operations compose the decorator's own required
primitives, so interception also applies through those operations.

## Alternatives considered

One wide trait requires every wrapper to forward unrelated methods and makes
concern isolation hard to express. Independent contracts plus a composed bundle
provide both narrow implementation units and one runtime handle. Separate
handwritten forwarding lists allow provided methods to drift between them.

## Consequences

Backend implementors satisfy the concern traits and `FleetFormatStore`; the
blanket implementation owns `ProcessRegistry`. Conformance support is a
separate testing contract. A concern-only wrapper cannot satisfy another
concern's bound accidentally. Composition does not grant process-execution
ownership to storage.

## Code references

- `crates/lash-core-execution/src/runtime/process/registry_concerns.rs:40-994` declares the concern traits.
- `crates/lash-core-execution/src/runtime/process/registry.rs:703-729` composes the registry and fleet format.
- `crates/lash-core-store/src/store/runtime_store_decorator.rs` defines the generated forwarding contract.
