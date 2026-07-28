# Bounded journals are an effect-controller obligation

A Runtime Process may run arbitrarily many effects (authored loops, large batch drivers) and
for arbitrary duration. In the durable tier that currently means one Restate invocation per
process whose journal grows with every effect — the classic unbounded-history cliff other
durable-execution systems cap outright (Temporal kills at ~51k history events; Inngest caps at
1000 steps) and solve with an author-visible reset (continue-as-new, new function runs). We
decided the reset is not the author's or the host's problem: **the `RuntimeEffectController`
seam guarantees that executing a process never requires an unbounded single-invocation
journal**, and each controller meets the guarantee with its backend's native mechanism. The
inline controller already satisfies it (effects re-drive against lash persistence; there is no
journal replay). `lash-restate` will segment a long process across chained invocations keyed
by (process id, segment), carrying an effect-result checkpoint across the boundary. A future
engine with native continue-as-new maps to that directly.

Above the seam nothing changes: process identity, leases, durable waits, provenance, replay
keys, and observation are segment-invariant, and no authoring construct or host projection
ever sees a segment. The seam itself needs exactly two expansions: a non-terminal
segment-boundary outcome on the effect-controller/run path that the process worker treats as
an ordinary reschedule (the process stays running; never terminal), and promotion of the
inline tier's effect-replay persistence to a controller-accessible seam so cross-segment
replay reads durable outcomes instead of carrying an ever-growing checkpoint. Boundary
thresholds, next-segment self-submission, and checkpoint mechanics stay inside the controller
crate. Open for the implementation pass: once outcomes write through to the replay store, the
engine journal stops being the replay source of record — deciding how far to lean into that
(engine as scheduler over lash-persisted replay) is the first design question, and durable-wait
re-arming across a boundary needs deterministic-simulation and fault-matrix evidence either
way. We rejected author-visible chaining (a `continue`-as-successor terminal
that hosts stitch into lineages) because it exports a backend limitation into every authoring
surface, host projection, and UI forever — and rejected a substrate-level incarnation
primitive as duplicating what each engine already does natively. Consequence: the lash-restate
segmentation is real, phased work — hosts must not ship unboundedly-looping processes on the
Restate tier before it lands — and segment handover (including handover while parked on a
Durable Wait) needs Deterministic Simulation and fault-matrix coverage.

## Resolved in implementation design (2026-07-12)

The implementation pass settled the questions this ADR left open, and corrected one framing
above. Nothing in the original decision is reversed; the guarantee, the segment-invariance,
and the two rejected alternatives all stand. The refinements:

### 1. The engine stays authoritative for durable execution within a segment

We do not lean into "engine as scheduler over a lash-owned replay store." lash is
engine-pluggable by design; taking on effect durability to dodge a journal that a proven
engine already maintains would be re-implementing durable execution for a backend-specific
symptom, with no reason that generalizes across engines. So: **the engine (Restate journal,
Temporal history, or the inline tier's own persistence) remains the source of record for
effect durability inside a segment.** The only state that crosses a boundary is a bounded
resumption snapshot (below), not an effect-replay ledger.

This shrinks the second seam-expansion named above. "Promote the inline effect-replay
persistence to a controller-accessible seam" is not needed for the engine tiers: cross-boundary
resume rides the bounded continuation, and within-segment replay stays each engine's own
concern (the engine journal; the inline tier's `runtime_effect_replay` store for the
engine-less case). We do not expose one tier's replay store to another.

### 2. The boundary trigger is step-count, decided by the controller — never a durable wait

The original text framed the cliff as "journal grows with every effect … for arbitrary
duration." Duration is the wrong half. On the target engines a wait is journal-neutral:
Restate *suspends and frees* on any await (a month-long sleep is one journal entry, holds no
compute, lasts indefinitely by design), and Temporal history grows on events, not on elapsed
wait time. What actually grows an invocation's journal/replay cost is the **count of durable
steps executed inside one incarnation** — a tight authored loop. So:

- **Segment on accumulated step/journal cost, taken at a quiescent post-effect point.** Every
  journal-growing authored operation already passes through the effect seam, so post-effect
  points are both frequent enough and naturally quiescent.
- **Never segment at a durable wait — suspend there.** Suspending is the engine's native, free,
  indefinite, and crash-safe behavior, and it keeps the durable-wait resolution inside the
  engine's own mailbox (Restate awakeable/promise resolution is journal-idempotent; a Temporal
  signal lands in history) instead of a hand-rolled buffer. This design *removes* the two
  hardest open problems above — "durable-wait re-arming across a boundary" and "handover while
  parked on a Durable Wait" — because that state no longer exists: you are never parked inside
  an incarnation across a wait.

The trigger is a single controller-owned predicate the run loop consults at each quiescent
post-effect point: **`wants_segment_boundary(progress) -> Option<BoundaryReason>`**. The
controller alone knows its backend's real limit, so the engine decides:

- inline tier → always `None` (no journal; runs to completion in one incarnation, unchanged);
- `lash-restate` → `Some(..)` as it approaches its replay-payload budget;
- an engine with native continue-as-new → maps its own suggestion (e.g. Temporal
  `GetContinueAsNewSuggested`) onto the predicate;
- a future engine with generous limits → always `None`, and segmentation simply never fires.

There is no host-facing segmentation-policy enum threaded through the run loop. Any tuning
(a replay-budget fraction; a wall-clock cap, see §4) is **controller construction-time input**,
folded into the same predicate; tests and DST force a boundary by constructing a controller
whose predicate fires on a schedule. Correctness is invariant to the predicate: a process must
compute identical results whether it segments every N steps or never — the predicate tunes
liveness, never semantics. Disabling it on an engine with a hard cliff is an operational
misconfiguration (the incarnation may die on an unbounded loop), not a wrong answer.

### 3. Cross-boundary state is a bounded VM continuation (the real lash-side work)

Investigation confirmed lash has no existing resumable-mid-computation snapshot. A lashlang
process is a bytecode VM inside one pinned future, re-initialized at `ip: 0` from its original
arguments each run; the only serializable snapshot is globals, and the session/turn
`execution_state_snapshot` is owned by the RLM code-executor, not the workflow VM. The former
`DurableStep` recorded one JSON effect result and was not a continuation. So a journal-reset
incarnation cannot resume at loop iteration K+1 today.

The bounded cross-boundary state is therefore a new **`VmContinuation`** — instruction pointer,
slots/globals, operand stack, iterator stack/cursors, occurrence counters, and process-host
effect ordinals — serialized at a quiescent post-effect boundary and used to reconstruct the
successor VM. This is smaller than serializing a Rust future because lashlang already
centralizes execution state in an explicit VM. Honest limit: the continuation is bounded by the
program's live data, not by effect count — an author accumulating an unbounded value in a slot
grows the continuation, exactly as continue-as-new does; no engine solves that. What we fix is
the journal-of-all-effects growth, which is the actual cliff.

### 4. Two orthogonal reasons to bound an incarnation; do not conflate

Journal/replay growth (§2) is one reason. The other is **code-version pinning**: a long-lived
incarnation stays bound to the code version it started on and blocks clean rolling upgrades
(Restate's documented motivation for "end and reschedule," which is *not* journal size). This is
an engine-specific operational lever (Restate version pinning; Temporal worker versioning), a
host preference ("I want to deploy within T"), and it folds into the same predicate as an
optional construction-time wall-clock cap returning `Some(DurationCap)`. It is closer to the
ADR-0023 host-lever pattern than to the journal obligation and must not be baked into it.

### 5. The handover, and the requirements that survive

The successor is the engine's native primitive carrying the continuation — Restate's delayed or
immediate self-`send` (its documented continue-as-new equivalent), Temporal `continue-as-new`.
No bespoke `(process, segment)` chaining machinery beyond handing over the bounded continuation.

Where a boundary can coincide with buffered external work, prior art (Temporal's drain-before-
continue-as-new and its livelock failure mode; Azure's still-buggy preserve-unprocessed-events;
AWS's silence on in-flight callbacks across a split) converges on three requirements:

1. the boundary transition — commit `VmContinuation`, retire the incarnation, schedule the
   successor — is **one atomic unit**, so nothing arrives in an unobserved gap;
2. successor start is **idempotent, keyed to the stable process id, not the incarnation**
   (per-run dedup is proven insufficient across a reset);
3. **no second uncaptured pending operation** at the cut — the continuation is the only
   uncommitted thing, or any other pending op is carried into it.

Because §2 keeps us from segmenting at waits, the common boundary is a tight compute loop with
no pending external event, so these bind narrowly; but they are mandatory for the crash-in-
handover window and for the rare boundary that meets buffered work. Segment handover — including
any interaction with a pending durable wait or a crash mid-transition — needs Deterministic
Simulation and fault-matrix coverage before hosts may run unbounded loops on an engine tier.

## Durable-core retention ruling (implementation pending)

ADR 0047 applies the bounded-journal obligation to the substrate-owned replay
tables and commit receipts as well as to one engine invocation. The ruling
requires canonical `ExecutionScope` replay keys attributable to their owning
turn, process, queue drain, deletion, or runtime operation, plus bounded pruning
for effect-replay rows and commit receipts.

This is not shipped behavior: replay rows still use the lossy
`(scope_id, replay_key)` primary key, and substrates expose neither pruning
surface. The L7 retention layer of the FIG-636 durable-core cutover owns the
canonical-key and pruning implementation. When it lands, the Host Application
will supply the `RetentionBound` and schedule the work under ADR 0023.

Age alone never proves that an operation cannot be re-driven after an outage,
so the future pruning surface must be terminal-gated. The controller remains
responsible for making bounded cleanup possible; the host remains responsible
for choosing the operational horizon.
