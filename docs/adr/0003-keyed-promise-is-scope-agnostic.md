# Durable Waits are scoped and resolved by the EffectHost

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
- The key doubles as a bearer capability, so it must be unguessable:
  `durable_wait_key = HMAC(scope_secret, ordinal)`. The `scope_secret` is
  host-delegable — supplied optionally at scope creation, persisted per scope —
  but is not a field on `ExecutionScope`; the scope is the lookup identity, and
  secret material is host/effect-owned state keyed by that identity. This gives
  the host issuance, validation, and revocation (drop the secret to void a
  scope's outstanding waits) without core understanding the host's auth model.
  The derivation stays in core for replay determinism; the effect host supplies
  the default secret when the host abstains. No token↔key map is introduced (one
  secret per scope, key still derived).
- The process-event/signal resolver ports onto EffectHost `AwaitEvent` as the
  only path; the process-scoped `AwaitEvent` resolver is deleted, not kept
  alongside.
