# Claim supersession is reclaim-mediated under the session lease

The historical filename is retained to preserve inbound links; the title above
is authoritative.

## Status

accepted

Supersedes the claim-renewal language in
[ADR 0014](0014-operational-policy-stays-with-the-host.md)'s *Lease Timings*
bullet: queued-work and turn-input claims are no longer leases with a TTL and a
renewal API. `LeaseTimings` now governs only the three true lease lanes.

## Context

Queued-work claims and turn-input claims carried a per-claim TTL (30s by default,
from `LeaseTimings`) exactly like the leases they lived beside — but the runtime
never renewed them. `renew_queued_work_claim` was in the store trait and every
backend, yet no call site in `crates/lash-core/src/runtime` ever invoked it, and
there was no `renew_turn_input_claim` at all. The two unrenewed lanes were
precisely the two that are already subordinate to the session execution lease:
every claim call takes and validates a live `SessionExecutionLeaseAuthority` in the
same transaction.

Because nothing renewed them, a claim outlived its usefulness by wall-clock
alone. A turn that claimed a wake batch at one active-turn checkpoint, then
stalled past the TTL (a slow provider call), re-ran `claim_ready_queued_work` at
the next checkpoint; the claim filter treated the turn's own now-expired rows as
free and re-claimed them, bumping `claim_fencing_token`. At commit the original,
now-stale claim could no longer complete its rows and the turn failed with
`QueuedWorkClaimExpired` — a user-visible turn failure caused by nothing more
than the owner being slow. Turn-input `DeferredNextTurn` claims had the same
latent shape via the failed-turn-then-idle-retry path.

## Decision

Claims pin the **session-execution-lease generation** they were taken under. The
generation is the lease's `fencing_token`: renewal and same-incarnation extension
preserve it, while every fresh acquisition after release or TTL expiry bumps it
(`previous + 1`). The pinned generation controls claimability and lease-less host
views; it is not an eager settlement fence.

**Core rule: claim supersession is reclaim-mediated.** The session lease provides
advisory serialization, liveness, and handoff: while a generation holds it, that
generation cannot re-claim its own rows; after release, expiry, or takeover, a
successor generation may re-claim them. Commit authority remains the session-head
CAS plus the batch-ownership check described below. Concretely:

- Claim rows record `claim_session_lease_generation` (the caller's validated-live
  `fence.fencing_token` at claim time) instead of a `claim_expires_at_ms`.
- On the claim path the caller holds a validated-live fence, so a row is
  claimable iff `claim_token IS NULL` **or**
  `claim_session_lease_generation != caller_fence.fencing_token`. Same-generation
  self-steal is unrepresentable. The old holder-liveness probe
  (`is_definitely_dead_for_claimant`) drops out of the claim filters entirely — a
  dead owner's generation can never equal the caller's validated-live one, so
  generation mismatch subsumes it.
- Host-facing paths that hold no lease (cancelling a queued batch, hiding
  live-claimed rows from pending snapshots, cancelling a pending turn input)
  derive liveness from the lease row itself in the same transaction: a claim is
  live iff the session's `session_execution_leases` row is live (lease token
  present and `lease_expires_at_ms > now`) **and** its `lease_fencing_token`
  equals the row's `claim_session_lease_generation`. The SQL backends evaluate
  this with a correlated subquery against the lease row; the in-memory and perf
  stores read the live generation once and compare.
- Completion validation is unchanged. Completions validate `claim_id` +
  `claim_token` row ownership, not the session-lease generation; the per-batch
  `claim_fencing_token` bump per re-claim and the `qwc:{seq}:{fencing}` claim-id
  format stay. Once a successor re-claims a batch, the predecessor completion no
  longer owns it and is rejected with `QueuedWorkClaimSuperseded` /
  `TurnInputClaimSuperseded`.
- The host handback levers `abandon_queued_work_claim` /
  `abandon_turn_input_claim` remain (per ADR 0014), now without expiry columns.
- The turn-input state machine (ADR 0010) is untouched: claim paths still claim
  only pending states and keep the `Accepted` lockout as admission evidence.
  Generation-pinned reclaim eligibility is added on top for both wanted-state
  scopes.

`renew_queued_work_claim`, the shared `renewed_claim` helper, and the
`lease_ttl_ms` parameter on every claim call are deleted. `LeaseTimings` and its
survive-two-missed-renewals invariant are unchanged, but now govern only the
session-execution, process, and durable effect-replay lease lanes, which still
renew on cadence.

The conformance contract distinguishes law from demonstrated current behavior:

- **LAW (all backends):** after a successor generation re-claims a batch, the
  superseded predecessor commit **must fail and must mutate nothing**. When its
  head CAS is otherwise current, it fails with `QueuedWorkClaimSuperseded` (or
  the turn-input counterpart). When the successor has also advanced the head, a
  backend may reject it earlier with `HeadRevisionConflict`; either error
  satisfies the law.
- **NON-LAW (current backends only):** before the successor re-claims, a
  predecessor claim-carrying commit is accepted by every current backend. The
  demonstration cases pin PRESENT behavior as executable documentation and are
  required for today's backends; a future deliberate tightening REVISES those
  demonstration cases alongside the behavior change — that is a suite revision,
  not a LAW breach.

This distinction is deliberate: the current suite rejects an eagerly fencing
backend because its executable demonstration would be stale, while demoting the
reclaim-mediated rejection LAW would be a contract break.

## Consequences

- The self-steal-at-checkpoint failure is unrepresentable: a live owner re-runs a
  claim under its own generation and can never re-claim its own rows, so a stalled
  but healthy turn keeps its claim for the whole turn and commits.
- The failed-turn idle-retry reclaims naturally: a failed turn's lease is released
  or taken over, the next acquisition mints a new generation, and the previously
  claimed rows become claimable. Re-claiming them supersedes the old completion.
- Claim liveness for lease-less callers is derived from the lease row join, so a
  released or superseded generation immediately makes its claims pending and
  cancellable again — a claim is never shown as live to a lease-less reader under
  a lease its owner no longer holds.
- The abandon levers stay for immediate handback but are no longer load-bearing
  for correctness: once an owner loses the lease its claims are eligible for
  successor re-claim, and that re-claim supersedes the old completion.

## Cross-version consequences

The claim schema changes, so — per lash's reject-and-recreate doctrine (there is
no migration chain) — durable state from before this release is rejected loudly
and recreated, not migrated. On `queued_work_batches` and `pending_turn_inputs`
the `claim_claimed_at_ms` and `claim_expires_at_ms` columns are replaced by a
single `claim_session_lease_generation`. The store schema version bumps
accordingly: SQLite session databases move to `user_version = 11` and the single
Postgres schema component to version 12. A pre-cutover database is rejected at
open with a "delete and start fresh" error.
