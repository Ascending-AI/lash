# Services are stateless; substrates own continuation

A lash service instance is stateless with respect to correctness. In-memory
state exists — a turn mid-stream, watch hubs, caches — but none of it may be
load-bearing across an effect boundary. Every committed step lives in the
store or the engine journal, and any instance can resume from committed state.
Sticky sessions are an affinity optimisation, never a correctness requirement:
the session-head CAS rejects stale history while commit-time claim ownership
rejects work a successor already re-claimed (ADR 0029). Those authorities make
resumption-anywhere safe rather than merely hopeful; the advisory lease itself
does not reject a current-head tail from an owner that lost it.

Statelessness has a floor. A turn actively streaming from a provider is
irreducibly in memory until its next commit point. Crashing there costs
re-execution from the last committed effect, never correctness. That is the
trade the checkpoint-committed ingress work already assumed, and it is the
right one.

## Once in flight, the substrate owns it

When an invocation is in flight, the durable substrate — Restate, Temporal,
or whatever a host implements against the contracts — owns continuation:
redrive after a crash, retry policy, and backpressure. Lash never re-drives
engine-owned work. The Restate tier conforms today: live starts submit engine
invocations, the ingress sweep only *submits* `run/send` per row and executes
nothing, execution happens inside engine invocations where parked waits
suspend natively, and a 10,000-row recovery is throttled by the engine's
invoker, not by lash. This is the same rule that already governs durability
(effect-host gaps close on the engine side) and work-item waits (await lives
on the work-driver seam); this document names the general principle those
rulings were instances of.

Two placement consequences follow. The shared segment executor
(`run_process_segment_with_scoped_effect_controller`) carries no bound,
because bounding it would double-bound work an engine already schedules. And
no lash-side stampede control exists on engine tiers, because that is the
engine's contractual job.

## The reference substrate

The in-process `DurableProcessWorker`, the SQLite and Postgres stores, and
the inline drivers together form the reference substrate lash ships so the
batteries-included path works without an engine. There, lash *is* the
substrate, so it redrives after restart — and its execution budget (the
inline process execution concurrency bound) is that driver's scheduling
policy, not a lash-level recovery semantic. Design pressure on the reference
substrate must not leak into the contracts.

## Conformance is the contract

A third-party substrate's redrive quality is its implementor's problem. What
lash owes them is an airtight conformance surface: the effect-host contract,
the work-driver seam, the process-registry conformance suite, and the
differential replay tests of ADR 0044. Passing conformance must mean the
substrate drives lash correctly; anything correctness-relevant that
conformance does not exercise is a gap in lash, not in the substrate. The
conformance kit is therefore a product surface, maintained and versioned like
one.

## Consequences

Anything found to keep correctness state only in instance memory across an
effect boundary is a defect against this document. New coordination features
land on the engine seam first, with the reference substrate implementing the
same contract inline. When a bound, a wait, or a redrive path is proposed
inside lash-core, the first question is whether it belongs to the substrate —
the answer decided FIG-526, and it will decide the next one.

## Considered and rejected: durable partial assistant streams (2026-08-20)

A 2026-08 review of a peer harness design examined the alternative this
document's floor forgoes: persist each streamed assistant delta as a compact
durable frame under the in-flight effect's reserved identity, delete the
frame list atomically at settlement, and reduce the committed prefix into an
explicit unknown-outcome response after a crash. The frames are auxiliary by
construction (never completion authority, never a restart point), so the
model is coherent. It is still rejected for lash: one durable transaction
per provider event prices per-token writes into Postgres- and journal-backed
stores, against the commit-budget rule; a partial-stream archive in the
session store contradicts the rule that the store records continuation
state, not observation history; and the property it buys is reconnect
display, not correctness. Hosts that want crash-surviving partials own that
choice at the observation plane: the live-replay store is host-supplied, the
streamed deltas already flow through it, and a durable implementation of it
recovers the same evidence without touching the session store or this
document's floor.
