# Operational Policy Stays With the Host; Lash Exposes Levers

## Status

accepted. The *Lease Timings* bullet is superseded in part by
[ADR 0029](0029-claims-are-generation-fenced-under-the-session-lease.md):
queued-work and turn-input claims are no longer TTL leases with a renewal API —
they are generation-fenced under the session execution lease. `LeaseTimings`
governs only the true lease lanes.

## Decision

Lash ships no shutdown or drain orchestrator. Operational policy — when to stop
admitting work, how long to drain, when to fail over, what to do with
stragglers — belongs to the embedding host, which owns the process, its
signals, and its deployment model. Lash's obligation is that every reasonable
host policy is implementable through explicit, lash-owned capabilities, because
the state those capabilities act on (leases, claims, waits, cached transports,
trace buffers) lives inside lash. The capability set:

- **Lease Timings**: every runtime *lease* (session execution, durable effect
  replay, process leases) derives its TTL and renewal cadence from one
  host-configurable `LeaseTimings` on the core builder, validated against the
  survive-two-missed-renewals invariant (`ttl >= 3 * renew_interval`). The former
  hardcoded 30s constants are gone. Queued-work and turn-input **claims** are not
  leases: they carry no TTL and are generation-fenced under the session execution
  lease (ADR 0029), so they inherit their liveness from that lane rather than
  from `LeaseTimings`.
- **Quiesce and handoff**: `LashSession::park(self)` flushes dirty state
  through a fresh-lease commit and returns a resumable `ParkedSession`;
  `LashCore::resume` rebuilds it. `LashSession::close(self)` is park-without-
  a-handle (flush + discard). Both consume the session and fail with
  `SessionStillInUse` when other live handles exist, making mid-turn quiesce an
  explicit contract rather than a silent partial flush.
- **Transport and sink release**: `Provider::close()` (default no-op; Codex
  drains its websocket session cache with real close frames) and
  `TraceSink::flush()` (default no-op; the OTel sink documents that span-export
  durability is the host provider's duty).
- **Claim and wait handback**: host-facing `abandon_queued_work_claim` /
  `abandon_turn_input_claim` return claimed work immediately instead of waiting
  out the claim TTL, and `revoke_durable_waits` resolves a session's
  outstanding Durable Waits as `Cancelled` without deleting the session.
- **Failover parity**: process leases carry `LeaseOwnerIdentity` and support
  the same fenced, TTL-gated acquisition as session execution leases. Neither
  lane infers holder liveness from the local process table.

## Why

A capability audit showed the machinery (fencing, per-turn leases, cancellation,
observation cursors) was first-class while the host-facing lever layer was not:
TTLs were compile-time `pub(crate)` constants, the park/resume quiesce primitive
existed only inside `lash-core`, `close()` silently did less than its name, and
TTL-gated takeover is the only portable rule for the opaque identities every
distributed deployment uses. The tempting fix — a `LashCore::shutdown()` drain loop — would
have moved host policy into the runtime and set the precedent for `health()`,
`readiness()`, and the rest of framework-hood. Lash's thesis is the opposite:
the app owns the outer boundaries; lash owns the turn. Lash owns the
effect-journal contract while the configured substrate owns the journal; the
same boundary discipline keeps drain policy inside the host.

## Consequences

- Hosts compose their own drain: stop admitting turns, cancel or await actives,
  `park()`/`close()` sessions, `close()` providers, `flush()` sinks, and exit —
  each step an explicit call, no hidden ordering.
- Failover latency is a host decision (`LeaseTimings`), traded explicitly
  against false-takeover risk, instead of a constant chosen by lash.
- `AwaitEventResolver` gained `cancel_await_events_for_session` with a
  loud-failing default, so durable effect hosts (Restate/Temporal adapters)
  must decide how wait revocation maps onto their engine rather than silently
  ignoring it. The inline registry (and the SQLite/Postgres boundaries that
  reuse it) resolves every outstanding wait for the session as `Cancelled`
  while leaving the session usable. Restate deployments bind
  `LashDurableWaitWorkflow` for exact-address promise resolution and
  `LashDurableWaitIndex` for the durable session-to-wait index. `cancel_all`
  drains the current index while permitting later registration; `revoke_all`
  drains it and persists a tombstone so session deletion also rejects future
  waits. All execution scopes use the exact workflow address, while scopes
  carrying a session id additionally participate in the session index.
- Anything lash cannot expose as a lever without becoming an orchestrator
  (signal handling, drain deadlines, readiness endpoints) is documented as host
  territory in the production guide instead of API surface.

## Considered Alternatives

- **`LashCore::shutdown()` orchestrator.** Rejected: drain ordering and
  deadlines are policy; the runtime absorbing them starts the framework slide
  and still could not know the host's grace budget.
- **`Provider::shutdown()` hook alone.** Rejected: without the rest of the
  lever set nothing in core would call it, making it a footgun-by-convention.
- **Accept the status quo (drop everything, TTL recovers).** Rejected after the
  audit: TTL-only release put a fixed 30s floor under every drain and failover
  path and was not a policy the host chose.
