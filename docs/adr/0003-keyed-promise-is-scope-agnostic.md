# Durable Waits are scoped and resolved by the EffectHost

Amended 2026-09-29 (FIG-4123): a wait key is a deterministic identity, not a
capability; resolution authority belongs to host ingress (ADR 0014, ADR 0046
§3).

A turn and a Runtime Process suspend on the same primitive: a one-shot durable
keyed promise (`AwaitEvent { key }` plus a resolve seam), resolved purely by
key. The key is derived from the Execution Scope, while the requirements needed
to run come from that scope's bound Execution Environment. Restate already
treats waits this way; the inline path currently welds resolution to the Process
Event log, so Durable Wait resolution moves onto the EffectHost. The Process
Event log becomes pure observability layered over that promise, not the
resolution mechanism.

The current code-level `EffectScope` name is promoted to `ExecutionScope`,
because the scope now owns more than effect-host routing: it is the identity for
replay, wait keys, cancellation, tracing, and environment binding.

## Considered Options

- **Make top-level a degenerate process** so every wait has a process to own it.
  Rejected: a Runtime Process is globally addressable and session-independent; a
  suspended turn is session-owned and must not inherit process
  lifecycle/addressability. Unify the mechanism, not the scope.
- **Keep suspension process-only** and require the agent to `start` a process
  before any long tool call. Rejected: pushes a runtime optimization into
  agent-authored structure for the most common case — a long tool call inside a
  turn — and the agent is not supposed to know or care.

## Consequences

- A foreground turn gains a durable suspend point: it may park on a detached
  tool completion and resume as the same turn, committing only on completion.
  Bounded, worker-resident turns are no longer guaranteed on the durable tier.
- A Suspended Turn remains the active session turn. It is observable through
  session observation, but it is not a Runtime Process and does not commit
  partial session history while waiting.
- Native substrate: the EffectHost wait capability is an in-process park (no durable
  suspension); the optimization is a deliberate no-op there.
- Execution Scope owns effect/replay/wait/cancel/trace identity. It is not the
  Execution Environment: processes still run from captured environment refs, and
  turns run from the session's current environment.
- Key identity splits across the two ends: the await side keys with a Replay Key
  (re-derived deterministically on replay); inbound `resolve` is an idempotent
  ingress governed by an Idempotency Key.
- Resolving a wait is terminal-state idempotent: the first resolution is
  accepted, duplicate delivery reports the already-recorded terminal result, and
  unknown or revoked keys are distinguishable without being treated as runtime
  failures.
- The process-event/signal resolver ports onto EffectHost `AwaitEvent` as the
  only path; the process-scoped `AwaitEvent` resolver is deleted, not kept
  alongside.

## Amendment (FIG-4125, 2026-09-29)

Item 5: The native in-process park described above is historical.
[ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md)
places durable execution on Restate. The separate wait-key authority and seed
ruling belongs to FIG-4123.
