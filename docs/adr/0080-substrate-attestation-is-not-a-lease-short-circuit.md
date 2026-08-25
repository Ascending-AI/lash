# Substrate attestation is not a lease short-circuit; failover waits the lease TTL

## Status

accepted. Ruled on FIG-1825. Applies the delegate-to-substrate boundary of
[ADR 0045](0045-services-are-stateless-substrates-own-continuation.md) to the
fencing model of
[ADR 0029](0029-claims-are-generation-fenced-under-the-session-lease.md), and
leaves the failover-latency lever of
[ADR 0014](0014-operational-policy-stays-with-the-host.md) where it is.

## Context

Under durable acceptance ([ADR 0069](0069-durable-acceptance-is-the-sole-turn-ingress.md))
a turn that has been accepted survives the death of the worker driving it: the
acceptance is journaled, the loser of the head CAS cedes without writing
anything durable, and the engine re-invokes against the journaled acceptance.
Liveness therefore comes from the substrate, not from any worker identity.

What the substrate cannot supply is *when* the successor may execute. The dead
holder's session-execution lease has to age out first. Acquisition is
TTL-gated and fence-minting: a claim against a session whose
`session_execution_leases` row is still live returns `Busy`, and the successful
acquisition after expiry sets `lease_fencing_token = previous + 1`. That new
generation is what makes the dead holder's claims re-claimable (ADR 0029) and
what makes any late write from the dead holder unable to complete rows a
successor has taken. Lash exposes no host-supplied liveness assertion that
would let a caller declare a previous incarnation dead and take the lease
early; the `examples/slack-clone` deferral documents the same property from the
host side, where a rebooting bot must simply wait out the TTL before its
lease-fenced admissions succeed.

The visible residual is failover latency. The
`Restate + Postgres + MinIO Workers` runbook records it twice — in the
owner-crash-recovery turn-control gate and in the failover convergence gate in
`runbooks/restate-postgres-workers/src/bin/runner.rs` — as roughly one lease TTL
(~38s observed against the 30s default `LeaseTimings` TTL, plus the engine's
re-invocation delay) between the crash and the recovered turn settling. Nothing
is wrong in those gates: the turn completes exactly once, against exactly one
acceptance, with no duplicate or conflicting settlement. The turn is simply idle
for a lease TTL first.

That residual invites an obvious shortcut. Restate already guarantees that at
most one invocation of a given key is in flight; Temporal offers the equivalent
for a workflow execution. If a substrate can *attest* that the previous holder
is no longer executing, why should the successor wait out a timer whose only job
is to infer the same fact? This ADR answers that question.

## Decision

**A durable substrate's exclusivity or liveness attestation is not accepted as a
short-circuit for the session-execution-lease TTL.** Failover of an accepted
turn waits for the lease to expire, and the successor's authority comes from the
generation minted by that expiry-gated acquisition. There is no attestation
parameter on lease acquisition, no substrate-supplied "previous holder is dead"
predicate in the claim filters, and no engine-specific fast path around the TTL
gate in any store backend.

Three things follow directly.

1. **Fencing is never delegated.** ADR 0045 gives substrates continuation and
   redrive policy — when to re-invoke, how to retry, how to apply backpressure.
   It does not give them lease authority. The session-execution lease, its
   generation counter, and the claim predicates derived from it are lash-owned,
   and they stay decidable from durable lash state alone. The
   delegate-to-substrate principle is explicitly asymmetric: substrate-unique
   concerns may be delegated, fencing never may.

2. **Failover latency is a host lever, not a lash constant.** ADR 0014 already
   places this exact trade with the host: `LeaseTimings` is host-configurable on
   the core builder, validated only against the survive-two-missed-renewals
   invariant (`ttl >= 3 * renew_interval`). A deployment that wants sub-10s
   failover sets a shorter TTL and a proportionally shorter renew interval, and
   pays for it in renewal traffic and in false-takeover risk under a slow store.
   That is the intended dial. The runbook's ~38s is the *default* dial position,
   not a floor lash imposes.

3. **The residual is documented, not tracked.** The two runbook comments cite
   this ADR as the settled decision rather than an open ticket to shorten the
   wait.

## Why

An attestation short-circuit would make correctness depend on a claim made by
the component the lease exists to be independent of.

The lease's job is not to guess whether a process is running. It is to be the
single durable fact from which every fencing decision is derived, in the same
transaction as the decision. Claim filters compare the caller's validated-live
fencing token against the token recorded on the row; lease-less host views join
against the lease row to decide whether a claim is live. Both are exact
comparisons over rows in one store. An attestation is a different kind of
object: a statement made outside that transaction, by a system whose own
guarantee is scoped to *its* invocations, about a window that must cover
lash's. A worker can be outside the engine's exclusivity envelope and still be
inside lash's danger window — mid-flight in a provider call it started before
the engine gave up on it, holding buffers it is about to write. The engine's
guarantee is "I will not run a second invocation"; the guarantee fencing needs
is "the first one cannot write". Those are not the same statement, and only the
second one is safe to build a CAS on.

Accepting the attestation would also spread the fencing model across the
substrate boundary in a way nothing else in lash does. Every store backend
would need an attestation-aware acquisition path; every conformance suite would
need to define what a *false* attestation must not be able to cause — and the
honest answer is that a false attestation from a trusted attester can cause a
duplicate live generation, which is precisely the state the whole fencing design
exists to make unrepresentable. Buying failover latency with that is a bad
trade, especially when the latency already has a supported lever.

Finally, the property being purchased is smaller than it looks. Durable
acceptance means the failover is already *correct* while it waits: nothing is
lost, nothing is duplicated, and the turn settles once. The wait costs latency
on an uncommon path. Lash's answer to latency on an uncommon path is a host
knob, not a weakened invariant.

## Consequences

- Failover of an accepted turn is bounded below by the configured
  session-execution-lease TTL, in every deployment and on every substrate. The
  runbook gates record this as expected behaviour with the reason attached.
- Hosts that need faster failover configure `LeaseTimings` and accept the
  renewal cost and false-takeover exposure explicitly. The production guide's
  framing of this as host policy is unchanged.
- Store backends gain no engine-specific lease path, and the conformance surface
  gains no attestation vocabulary. TTL-gated, fence-minting acquisition remains
  the only way a generation changes hands.
- Proposals to let any external system assert holder liveness — a substrate
  attestation, a container-orchestrator liveness probe, a peer's local process
  table — are answered by this ADR and by ADR 0014's *Failover parity* bullet,
  which already states that neither lease lane infers holder liveness from a
  local process table.
- If a future substrate contract genuinely bounds the previous holder's *write*
  window rather than its invocation window, that is a new fact and would deserve
  a new decision. It would still have to arrive as a durable, lash-verifiable
  fact in the acquisition transaction, not as a trusted claim.

## Alternatives considered

- **Accept the attestation as a liveness short-circuit (the rejected design).**
  Sketched honestly: `SessionExecutionLeaseClaimIdentity` gains an optional
  `HolderExclusivityAttestation` supplied by the driving substrate, asserting
  that the identified previous holder has no in-flight invocation for this
  session. Acquisition treats a valid attestation as equivalent to expiry —
  it still mints `previous + 1`, so downstream claim and view logic is
  untouched, and only the expiry predicate is relaxed. Failover latency drops
  from a lease TTL to the engine's own re-invocation delay. Rejected: the
  attestation is a statement about invocations, not about writes, so it does not
  establish the property the CAS needs; it makes a lash invariant depend on a
  substrate's self-report; it obliges every backend and every conformance suite
  to model a trusted external assertion and to define the blast radius of a
  false one; and the latency it buys is already available through
  `LeaseTimings` without touching the fencing model.
- **Short-circuit only on the reference substrate, where lash is the
  substrate.** Rejected. ADR 0045 states that design pressure on the reference
  substrate must not leak into the contracts, and the inverse holds too: a
  correctness shortcut available only inline would make the reference substrate
  semantically privileged and would diverge the conformance suites from the
  behaviour hosts actually get.
- **Shorten the default TTL so the residual is smaller everywhere.** Rejected as
  a decision for this ADR: it does not answer the attestation question, and
  choosing a default failover latency for every deployment is exactly the host
  policy ADR 0014 refuses to absorb. A host that wants a shorter TTL sets one.
- **Have the successor probe the previous holder directly before acquiring.**
  Rejected. This is the holder-liveness probe ADR 0029 removed from the claim
  filters: it re-introduces a liveness inference alongside the generation
  comparison that already subsumes it, and it is unavailable for the opaque
  identities most distributed deployments use.
