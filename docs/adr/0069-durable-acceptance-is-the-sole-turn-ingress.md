# Durable acceptance is the sole turn ingress

## Status

Accepted. Ratified on FIG-1661; the four sections of the decision are the four
rulings recorded there. Implemented separately by FIG-1671.

## Context

Lash has two ways to start a turn, and they disagree about what durably exists.

`LashSession::enqueue(input).send()` writes a Pending Turn Input row and returns
a `TurnInputAcceptanceReceipt`. The input is durable admission evidence before
anything executes ([ADR 0010](0010-pending-turn-input-is-admission-evidence.md)),
so any drain holding the session-execution lease can claim it and drive it, and
the claim is generation-fenced like every other claim
([ADR 0029](0029-claims-are-generation-fenced-under-the-session-lease.md)).

`LashSession::turn(input).run()` and `.stream_to(..)` write nothing. The turn
executes straight off the caller's future, and the first durable trace of it is
whatever the effect host journals once the turn is already running.

That asymmetry is not a convenience tier. It is a hole in the property
[ADR 0045](0045-services-are-stateless-substrates-own-continuation.md) exists to
guarantee — any instance can resume from committed state — and the hole has a
precise shape:

* **Journaled effects with no discoverable owner.** A direct turn that dies
  mid-flight leaves committed durable effects behind. Those records are real,
  replayable continuation state, and no row anywhere says a turn wanted them.
  Nothing enumerable points at the work, so no drain, no worker, and no operator
  sweep can find it. The store holds the answer and cannot be asked the
  question.

* **The caller as a single point of continuation.** Only the process holding the
  future could resume such a turn, which makes that in-memory future
  correctness-bearing across an effect boundary — exactly the thing ADR 0045
  forbids. Sticky routing stops being an affinity optimisation for these turns
  and becomes a requirement nobody declared.

* **Abandonment expressed as silence.** A caller that walks away from a direct
  turn leaves no cancellation, no tombstone, and no terminal evidence. "Dropped
  the handle" and "still running somewhere" are the same observation.

* **Two ingress semantics for one operation.** Idempotent submission, admission
  outcomes, and post-crash recovery all have answers on the queued path and no
  answers on the direct one, so every feature touching ingress is specified
  twice and the second specification is always "not applicable here".

None of this is a defect in the direct path's implementation. It is the
consequence of having a second ingress at all. This ADR removes the second one.

## Decision

**Every turn enters through one durable acceptance commit, then is driven. There
is no ingress that skips it.**

### 1. Single ingress, unconditional

`TurnBuilder::run` and `TurnBuilder::stream_to` become sugar over the same
accept-then-drive machinery as `enqueue`: one acceptance commit that writes the
Pending Turn Input row, then drive that accepted input to completion, returning
the caller the turn's result as before. The convenience of the direct API is
preserved exactly — one call, one awaited turn, no queue handling — and it is
now convenience over the durable path rather than an alternative to it.

The rule is unconditional. It holds on every backend, including the in-memory
one; there is no durable-only carve-out, no fast path selected by store
capability, and no caller-owned no-record path left anywhere in the facade. A
conditional ingress would reintroduce the two-semantics problem with a runtime
predicate deciding which semantics a given turn got, which is worse than the
asymmetry it replaces.

The acceptance commit reuses the existing Pending Turn Input row class. It gains
no new states and no new columns: a direct turn's row is a `NextTurn` admission
like any other, and its cancel and vacuum lifecycle is the one FIG-1511 already
owns under [ADR 0067](0067-durable-rows-name-one-owner-and-one-reclaim-trigger.md).
A separate turn-intent marker row class was considered and rejected — see
*Alternatives considered*.

### 2. The cost is stated, not gated

Direct turns on durable backends pay **one additional store commit each**: the
acceptance commit. That is the honest number and it is not hidden behind a
qualifier. It is the same Pending Turn Input commit that queued ingress has
always paid, so the change does not invent a write — it removes an exemption.

This ADR states the cost as evidence rather than treating it as a gate.
FIG-1189's commit-budget benchmark is the work that fills in the measurement
(what a commit costs on each backend, under
[ADR 0058](0058-runtime-commit-budgets-are-explicit-host-policy.md)'s host-owned
budget), and its numbers refine this section's sizing guidance. They do not
decide whether the ruling holds.

If measurement shows a pathological result on some backend, the response is an
**amendment proposal against this ADR**, argued in the open, with a named cost
and a named alternative. It is never a silent carve-out re-adding a no-record
path for the case that measured badly. A performance exemption that reintroduces
two ingress semantics costs the property this document exists to establish, and
that trade has to be made explicitly or not at all.

### 3. Recovery is fully unified

**After acceptance, a direct turn is indistinguishable from a queued one.**

This is the point of the ruling, and its consequences are not softened here:

* Any worker or drain holding the session-execution lease may rediscover an
  orphaned accepted input, claim it under the generation-fenced claim machinery
  of ADR 0029, and drive it to completion.
* **The caller's future is the first driver, with no special status.** It is not
  the owner, it holds no reservation, and it is superseded like any other
  claimant when its generation is superseded.
* **A crashed direct turn can complete without its caller.** The process that
  called `run()` may be gone and the turn still commits, its effects replayed
  under [ADR 0010](0010-pending-turn-input-is-admission-evidence.md)'s admission
  evidence and the effect host's journal. The caller learns nothing, because
  there is nobody to tell; the session's state is nevertheless correct and
  complete.

That last consequence is deliberate and worth stating plainly to a host author:
calling `run()` and dropping the future does **not** stop the turn. **Abandonment
is expressed by cancel, never by silence.** Before the input is claimed, that is
the durable, typed, observable cancellation of the accepted input — ADR 0010's
admission outcomes, extended to accepted inputs by the FIG-1511 work. Once a
driver is executing, stopping it remains
[ADR 0039](0039-turn-cancellation-is-a-first-party-work-driver-primitive.md)'s
turn-cancellation primitive on the keyed-promise seam, unchanged and adding no
store coordination state. The two compose along the same acceptance timeline;
neither is replaced by dropping a handle. A host that wants "stop this turn"
must say so, and a host that merely goes away has handed the turn to whoever
drains next — which is the behaviour it already had for enqueued work.

### 4. No new idempotency machinery

Direct turns inherit queued ingress's identity and dedup semantics exactly.
`source_key` remains the immutable idempotency key for a submitted revision,
with ADR 0010's replay and conflict rules unchanged, and the acceptance identity
is exposed on the handle the direct call returns — the same
`TurnInputAcceptanceReceipt` identity `enqueue(..).send()` already returns, now
reachable from a turn the caller ran directly.

The window this leaves is named rather than papered over. **A retry after an
unacknowledged crash is a new turn.** If the acceptance commit lands and the
caller never learns that it landed, the caller's retry submits a fresh input
under a fresh identity, and the session may execute both. Lash does not
deduplicate them, because it has nothing to deduplicate them by: the caller
never named an identity, so no two submissions can be recognised as one.

Closing that window means a caller-supplied idempotency key, and that is
deliberately **not** ruled here. It is its own decision with its own row-class
ownership story — who owns the key-to-acceptance mapping, what its reclaim
trigger is under ADR 0067, and how long a key stays honoured — and inventing it
as a side effect of unifying ingress would be exactly the kind of unowned dedup
table ADR 0067 was written to stop. A host that needs at-most-once submission
today gets it the way it always has: by using `enqueue(..).id(..)` with a
`source_key` it chose.

## Alternatives considered

* **A new turn-intent marker row class for direct turns.** Rejected. It buys a
  record that is cheaper to write only if it records less, and everything it
  would omit — admission state, claimability, cancellation outcomes, terminal
  evidence — is exactly what recovery needs. It would also arrive owing ADR 0067
  a fresh owner and reclaim trigger, and owe the claim machinery a second row
  shape to fence. Reusing Pending Turn Input costs one commit and inherits a
  lifecycle that already works.

* **Keep the direct path for non-durable backends only.** Rejected. It makes
  ingress semantics a property of the store, so a host's recovery behaviour
  changes when it moves from the in-memory store to SQLite — and the in-memory
  store stops being a faithful conformance target for the very property under
  test.

* **Gate the ruling on FIG-1189's benchmark.** Rejected as an inversion: the
  defect is a correctness hole, and correctness rulings are not conditional on
  a cost curve. The numbers inform sizing guidance and can motivate an
  amendment; they do not decide whether journaled effects may be undiscoverable.

* **Register the caller's future as the privileged driver, with a reservation
  another worker must break.** Rejected. It preserves the caller's special
  status in durable state, which means a second fencing mechanism beside ADR
  0029's generations and a new "is the caller still alive" question with no good
  answer. The first driver having no special status is what makes recovery
  uniform.

* **A caller-supplied idempotency key, ruled here.** Rejected as scope, not as
  an idea. See section 4: it needs its own ownership ruling, and bundling it
  would ship an unowned dedup table.

## Consequences

* There is one ingress path and one set of ingress semantics. Features touching
  admission, cancellation, idempotency, or recovery are specified once.
* Direct turns cost one extra store commit on durable backends. Hosts running
  high-frequency short direct turns will see it; the commit-budget guidance in
  FIG-1189 is where the sizing advice lands.
* Dropping a direct turn's future no longer stops it. Hosts that relied on drop
  as an implicit cancel must cancel the accepted input instead, and this is a
  behaviour change worth calling out in release notes when FIG-1671 ships.
* ADR 0045's floor is unchanged and its guarantee gets stronger: a turn
  streaming from a provider is still irreducibly in memory until its next commit
  point, but the *fact that a turn was requested* is never in memory only, so
  resumption-anywhere now covers every turn rather than the enqueued ones.
* The undiscoverable-journaled-effects failure becomes unrepresentable: no turn
  can have durable effects without an enumerable accepted input naming it.
* An unacknowledged crash between the acceptance commit and the caller learning
  of it can duplicate a turn. This is a named, accepted window, not an oversight.
* `docs/architecture/queued-work-ingress.md` describes the pending-turn-input
  lane that now carries every turn; its "no pin, by design" reasoning holds
  unchanged, because a direct turn's batch also does not exist before its claim.

### Sequencing

FIG-1671 implements this ADR, sequenced after the FIG-1660 and FIG-1663 wave so
it lands on the settled ingress and ordering surfaces rather than racing them.
Its acceptance evidence is the killed-worker recovery runbook extended to a
direct turn: kill the caller mid-turn, let an unrelated worker rediscover and
drive the accepted input, and show the session committing a complete turn the
original caller never saw.
