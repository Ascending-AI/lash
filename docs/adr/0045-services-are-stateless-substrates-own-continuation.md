# Services are stateless; substrates own continuation

Amended 2026-09-24 (FIG-3669), **not yet implemented**:
[ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md)
makes Restate the only effect engine and the SQL stores storage only. This ADR
specifies SQL-engine behaviour in *The reference substrate*, which makes lash
the substrate over the SQL stores, with its own redrive and worker retry
budget. *Conformance is the contract* stands, and now binds the one engine and
any later one. Those passages stay as written until the PR that deletes the
code (FIG-3667, FIG-3668, or FIG-3600 for the session lease) rewrites them.

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

This includes the same-session commit-admission FIFO, for both final turn
commits and queued session-command commits. The controller explicitly declares
whether an engine owns commit backpressure. Durable journal participation is
not that discriminator: store-backed replay also journals effects and still
uses the native FIFO. Turn-owned command drains carry the invocation controller;
standalone command drains select the configured host controller. Storeless
commits continue to bypass local admission.

## The reference substrate

The in-process `DurableProcessWorker`, the SQLite and Postgres stores, and
the native drivers together form the reference substrate lash ships so the
batteries-included path works without an engine. There, lash *is* the
substrate, so it redrives after restart — and its execution budget (the
native process execution concurrency bound) is that driver's scheduling
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
same contract natively. When a bound, a wait, or a redrive path is proposed
inside lash-core, the first question is whether it belongs to the substrate —
the answer decided FIG-526, and it will decide the next one.

- lash-core never asks which tier it runs on. A behaviour difference is either
  an operation on the effect seam or the single `EffectJournaling { Local,
  Journaled }` fact on `RuntimeEffectController` (FIG-2226, PR #1946). The
  only documented exception is `owns_commit_backpressure`: it is a property of
  the engine, not a tier flag (FIG-3397 deleted `supports_concurrent_effects`). `scripts/check-substrate-boundary.sh` guards the retired names.
- How a failed turn settles is not a tier question either (FIG-3575). The
  failure's code has a cause class, `RuntimeErrorCode::turn_failure_cause`:
  a code is terminal exactly when it is an outcome. An outcome, and any
  failure the journal already holds, is recorded as a failed turn and settles
  a queued run once. A live fault aborts: an aborted direct turn returns its
  acceptance receipt, and a queued run stays pending for its retry budget.
  A replay refusal is a third class, `Parked` (FIG-3586, FIG-3587): the
  lashlang divergence and cutover codes, `lashlang_cell_binding_drift`, and
  any recorded effect's replay hash conflict on every SQL host. It is neither
  terminal nor retryable: the turn aborts with its claims held, a typed park
  is recorded, and no retry budget is spent, because every redrive by the same
  build refuses again with zero dispatch. A hash conflict was recorded as a
  failed turn before FIG-3587; it now parks on every SQL host. Restate's
  envelope mismatch (`WorkerReplacementAbort`) still aborts as a live fault;
  parking it is deferred to S7. Otherwise every host settles the same failure
  the same way.
- Decided 2026-09-24, not yet implemented (FIG-3600, [ADR 0101's
  amendment](0101-one-session-ingress-carries-every-admitted-item.md#amendment-fig-3600-2026-09-24-one-send-ingress-the-driver-runs-every-turn)):
  continuation is the substrate's for **every** turn, because no caller-driven
  turn exists after that cutover. The backend's work driver runs each turn.
  A live fault is re-driven under the substrate's policy with the same turn id,
  and an exhausted budget parks the turn. The aborted direct turn in the
  bullet above goes away on landing.

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

## Amendment (FIG-3588, 2026-09-24): a Restate segment never restarts started work

"Lash never re-drives engine-owned work" is absolute on the Restate tier. A
Restate workflow's invocation id is a function of its key (Restate v1.7.0,
`InvocationUuid::generate`), so a retry of the invocation that started a
segment and a fresh invocation of the same key after its journal was lost
carry the same id. The id cannot tell them apart; a durable start marker does.
Every `LashProcessWorkflow/run` invocation admits its segment before any
effect, in this order, each step its own journaled command:

1. **Verdict** (read-only). Read the segment's start marker: segment 0's is
   the process's `first_started`, a later segment's is the marker on its
   retained handover. Present: the process ends `Abandoned` with
   `ResumeRefused { SubstrateLost }`, naming the lost execution. Absent: admit,
   with a nonce drawn from OS randomness inside the step so it is journaled
   with the verdict. A segment whose successor handover exists already
   completed; it is ignored, never refused.
2. **Start**. Write the marker with that nonce, set-if-absent. The recorded
   nonce equals ours: this execution's own marker, possibly from its earlier
   try. A different nonce: a lost execution started the segment, so
   `SubstrateLost`.
3. **Effects**, only under the proof step 2 returns (`SegmentStarted`, which
   has no public constructor and which the segment's controller and runner
   require).

The nonce and the marker are two journaled steps because a retry re-runs a
step whose completion was not journaled; one step that drew and wrote would
draw a second nonce and refuse its own marker. Neither the context RNG nor the
invocation id may supply the nonce: both repeat after a purge.

The resulting cases, each a law against Restate's own identity and retry
semantics (`lash-restate` `tests::substrate_lost`):

- (a) a crash between the steps retries over the journaled verdict and
  proceeds;
- (b) a journal lost before the marker commits admits a fresh run, and no
  effect had run;
- (c) a journal lost after the marker, before the first effect, ends
  `SubstrateLost` with zero effects. This double fault is the accepted
  direction: a false Abandoned, never a duplicate effect;
- (d) a journal lost after effects ends `SubstrateLost` with no re-dispatch.

Restate v1.7.0 refuses to purge an invocation or its journal while the
invocation is not completed, so (b) and (c) arise from endpoint crashes and
lost journal storage, not from ordinary purges.

The ingress sweep therefore submits every live row under its latest segment's
key, whatever its external reference says: Restate coalesces a submission onto
a live or retained workflow, and a key it no longer holds runs the admission,
which starts a segment that never started and refuses one that did. The
external reference is observational. A boundary still writes its successor's
reference before the handover and the send, and a store fault there is
retried by Restate, never logged and dropped.

`RESTATE_PROCESS_JOURNAL_VERSION` owns the handler's leading journaled
commands; any change to them bumps it. Every submitter stamps it on the
workflow input, and the handler refuses another generation before it
journals anything, ending the process `ResumeRefused { RetiredGeneration }`.
Refusing chains an earlier build submitted, rather than migrating them, is the
current, temporary cutover policy, not a permanent law.
