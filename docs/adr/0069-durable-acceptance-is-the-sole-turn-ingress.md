# Durable acceptance is the sole turn ingress

## Status

Accepted. Ratified on FIG-1661; the four sections of the decision are the four
rulings recorded there. Implemented separately by FIG-1671, which added
section 5 — how an accepted input is settled by the turn that drove it.

Strengthened 2026-09-23 (FIG-3540), **not yet implemented**: [ADR 0101](0101-one-session-ingress-carries-every-admitted-item.md) makes the Pending
Turn Input row class the `input` kind of one Session Ingress item, beside
process wakes and session commands, so "sole ingress" is literal: host, process
and command admissions share one table, sequence and lifecycle (commands in a
class-level lane applied at turn boundaries), and no commit writes ingress rows
(the frame follow-on is a session-head fact). A row's delivery is immutable
intent: final commits no longer re-defer addressed input, and an ended turn's
items are next-turn items by rule. Section 6 stands; section 5's unclaimed
regime is deleted (see the note there). References below to ADR 0010 read as
ADR 0101.

Amended 2026-09-23 (FIG-3589): section 7 adds the one ingress row state the
acceptance timeline lacked. A direct turn that aborts with `Err` binds its drive
claim to its turn, so the input no longer lapses to whichever lease generation
comes next; only that turn's redrive or a cancel by its receipt consumes it. A
crashed turn is untouched: section 3's recovery still reclaims it.

Amended 2026-09-24 (FIG-3600), **not yet implemented**: [ADR 0101's FIG-3600
amendment](0101-one-session-ingress-carries-every-admitted-item.md#amendment-fig-3600-2026-09-24-one-send-ingress-the-driver-runs-every-turn)
makes `session.send(input)` the only way a turn starts and the backend's work
driver the only thing that runs one. Durable acceptance stays the sole ingress.
On landing of FIG-3600, the caller-driven parts of this ADR are superseded:
section 1's `run` / `stream_to` sugar, section 3's caller future as first
driver, the direct-turn `Queued { ahead }` outcome in section 6, and section 7
entirely. The text below still describes current code.

Amended 2026-09-24 (FIG-3552): section 6's cede rule covers every claim a
redrive restores, not only its journaled drive. A checkpoint claim of turn input
or queued work, and work withheld at a terminal checkpoint, all carry the first
execution's authority. A resumed queued run first retakes the open rows its
checkpoints were assigned under its own generation (ADR 0029), so a restored
claim that is still superseded at commit was taken by another driver: the
redrive cedes with `accepted_turn_input_ceded` and commits nothing. ADR 0029's
recovered-settlement drop, which section 5(d) and section 6 refer to, is
deleted.

## Context

Lash has two ways to start a turn, and they disagree about what durably exists.

`DurableSession::enqueue(input).send()` (reached as `session.durable()` or
`core.session(id).durable()`; ADR 0097) writes a Pending Turn Input row and returns
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

### 5. A turn may settle the acceptance it drove, with or without a claim

> Amended 2026-09-23 (FIG-3540): every direct turn claims its row since FIG-3532,
> or returns the success outcome `Queued { ahead }` when queued behind the claim
> bound. Store-level unclaimed settlement is dead and is deleted on all three
> stores in the [ADR 0101](0101-one-session-ingress-carries-every-admitted-item.md)
> cutover; the claimed regime below remains the only one.

Section 3 says the caller's future is the first driver with no special status.
That is a statement about *recovery*, and it was read once as a statement about
*authority*: that a direct turn must first take the session-execution lane, claim
its own accepted row under ADR 0029's generation fencing, and only then drive it.
That reading is wrong, and it breaks a property that predates this ADR — the
session-execution lane is **advisory for a direct turn**
([ADR 0029](0029-claims-are-generation-fenced-under-the-session-lease.md): the
commit CAS is the authority, the lease is an advisory serialiser). Making the
lane load-bearing would refuse foreground turns that lash has always run.

**So: a turn may settle the acceptance it drove without holding a claim on it,
fenced by the head commit CAS.** There are exactly two settlement regimes —
*claimed* and *unclaimed* — and one authority for both: the head CAS at commit.
A driver that holds the lane claims its row and settles under the claim
predicate; a driver that could not take the advisory lane drives the row it
itself accepted and settles it under the bare row predicate. Nothing else
changes: the ingress split (direct push versus claimed drain) is unchanged, and
the two regimes are **one code path per backend**, not two parallel routines —
the claim fields are an optional predicate strengthener on the same conditional
write.

Four conditions bind this, and each is enforced in code.

**(a) Predicate plus affected-rows contract.** Unclaimed settlement is a
conditional write, not a blind one. Every backend verifies that the settlement
UPDATE (or, for the in-memory and perf stores, the equivalent state check) is
matched by **exactly one row**. The claimed predicate additionally requires the
claim id and claim token; the unclaimed predicate requires the row to carry no
claim and to be in neither `Completed` nor `Cancelled`. Zero matched rows is a
typed supersession error, never a silent success — a settlement that changed
nothing must never be reported as a settlement that changed something.

**(b) Ordering.** A lane-less direct turn is **exempt from queue-head ordering**,
and this is the stated exemption rather than a head-only restriction. It drives
exactly the row it accepted and no other, so it never reorders itself ahead of
pending work it might otherwise have drained: what it does is precisely what an
advisory-lane direct turn has always done, now with a durable row behind it. A
turn that *wants* the queue's ordering takes the lane and claims, which is the
claimed regime. The exemption is enforced by the settlement validator: an
unclaimed completion carries no claim id to match on, so it validates only
against an originating settlement naming exactly the rows this turn accepted. A
turn can never settle a row it neither claimed nor accepted.

**(c) Commit identity is unchanged.** Unclaimed settlement rides the existing
`completed_turn_inputs` hash projection. Claim-authority fields stay excluded
from `turn_commit_hash`, and **no new intent field is introduced** — the identity
of a settlement is the session and the rows, which is what it always was.
Introducing an "unclaimed" marker into the intent would change replay hashing for
every commit, including the ones that never had a choice of regime.

**(d) Supersession, and what the loser does about it.** ADR 0029's
recovered-settlement drop rule keys on lease generation: a settlement whose claim
generation has been superseded is dropped and retried under the current one. An
unclaimed settlement has **no generation**, so it has no generation to be
superseded at, and the rule does not apply to it. Its semantics are therefore
*lose, do not retry*: it is a distinct typed error, and the losing driver retires
its turn at its **first** commit attempt rather than looping. This is what decides
the recovery-claim-versus-live-driver race — if a recovery claims the row while a
lane-less driver is mid turn, the CAS decides at the loser's first commit.
Liveness is never guessed.

Losing is not failing. The loser **cedes**, and ceding has a defined shape:

* it does **not** retry the settlement, and it does **not** re-commit the turn's
  content under any other authority — the settlement is attempted once, and once
  only, per drive attempt;
* nothing durable was written, so the row is left exactly as its holder expects
  to find it and the session's record is untouched;
* the whole drive attempt is retired as **superseded**, surfaced as one typed
  runtime outcome — `turn_input_settlement_superseded` — and never as a generic
  commit fault.

That outcome is **retryable by contract**, and the two words together are the
point. The drive attempt is over; the *admission* is not, because acceptance is
durable and journaled (section 6). So re-running the identical turn is explicitly
safe and is how a durable engine obtains the result: replay re-derives the same
acceptance and the same journaled drive set (section 6), and the commit either
settles those rows or finds the original commit's receipt and replays it. A host that treats this outcome as a
terminal failure turns ordinary failover — where the accepted row is momentarily
held by a driver that has gone away — into a user-visible fault, which is exactly
what this section exists to prevent. The `Restate + Postgres + MinIO Workers`
failover scenario is the behavioural witness.

### 6. Acceptance is journaled, so engine replay re-derives it

The acceptance commit happens before the turn runs, which puts it inside the
replay window of a durable-execution engine driving lash
([ADR 0045](0045-services-are-stateless-substrates-own-continuation.md)). If it
is performed as an ordinary store write, every replay of the handler performs it
again and admits the same turn twice.

So acceptance is issued as a **journaled runtime effect**, not as a direct store
call. On first execution the engine journals the acceptance and its result; on
replay the engine returns the journaled result and lash re-derives the admission
instead of re-performing it. The algebra is the same one the effect host already
uses for provisioned effects: the acceptance identity is provisioned and stable
across replays, the committed successor result is the predicate that prevents a
second redemption, and an intent is fulfilled if and only if its result exists.

"An already-settled identity is skipped" is the *effect* the replay delivers, not
the mechanism, and the difference matters. Lash has no way to hand a caller back
a turn it never ran, so a replay whose turn already committed **redrives** the
turn and lets the commit-identity receipt recognise it: the re-derived commit
hashes to the same `turn_commit_hash`, and the store replays the original result
instead of writing a second one. The redrive therefore has to re-derive the same
turn, which means the same rows with the same words — a first execution that held
the lane may have absorbed every earlier claimed row into one turn.

**The initial drive set is journaled with its settlement authority, exactly as
checkpoint claims are.** Right after `AcceptTurnInput`, the turn issues a second
journaled effect, `ClaimAcceptedTurnInput`, whose outcome is a self-contained
snapshot of what the turn drives: the claimed rows with their content and claim
token; `Queued { ahead }` when the accepted row is open but sits behind more
earlier admissions than one claim absorbs; or a typed refusal. It is a separate effect rather than part of the acceptance, because the
acceptance body must stay one write: a crash between two writes in one body
re-runs both under at-least-once semantics. The envelope names only the accepted
input; the lease fence and owner that perform the claim are captured by the local
executor, so the entry replays identically under every lease generation.

Replay returns that journaled drive and never reads pending rows. Rows are
admission evidence, not replay state
([ADR 0010](0010-pending-turn-input-is-admission-evidence.md)), so `vacuum()`
pruning terminal rows cannot affect a replay: a redrive after vacuum drives the
journaled rows and replays the receipt, an input the host cancelled is never
re-admitted, and a row admitted after the drive was journaled never joins the
redriven turn. Only a first execution reads rows, and when it finds its own row
held by another claim of the live lease generation, or already settled,
cancelled, or pruned, it cedes with the terminal typed error
`accepted_turn_input_ceded`. It never re-admits a row. Replaying that refusal
refuses again, which is why it is terminal.

A replayed drive settles with the first execution's claim token. If the worker
died after the drive was journaled and a recovery drain reclaimed and answered
those rows meanwhile, the settlement is superseded, and the replay cedes at
commit with the same `accepted_turn_input_ceded`: nothing is written. The
journaled drive never joins ADR 0029's recovered-settlement drop, because
dropping the superseded rows and committing anyway would answer the same words
a second time.

A queued drive drops nothing and withdraws nothing. The accepted row stays in
the next-turn queue in arrival order, the call succeeds with one turn whose
outcome is `TurnOutcome::Queued { ahead }` and which carries the acceptance
receipt, and the queued-work drain answers the input in order, exactly once. The claim bound is host policy
(`QueuedWorkBatchingConfig::with_max_turn_input_claim`, default 64), shared by
the direct-turn drive and the drain because they claim from the same queue.
Retrying the call would admit the words a second time; replaying the turn
reports the same queue position.

This is the second regime's replay story and it introduces no third one.
Acceptance replay adds **no third authority** beside the claim predicate and the
head CAS: the journaled drive replays the claim predicate's own token. To be
precise about what that does and does not say: a direct turn that takes the
advisory lane claims its row through the same generation-fenced
`claim_next_turn_inputs` a drain uses, so it *is* lease-generation fenced —
that is the claimed regime, unchanged. What section 6 rules out is a *separate*
generation fence attached to acceptance replay itself, on top of the two
regimes section 5 defines.

### 7. An aborted direct turn's input is bound to that turn

> Superseded on landing of FIG-3600 by [ADR 0101's FIG-3600
> amendment](0101-one-session-ingress-carries-every-admitted-item.md#amendment-fig-3600-2026-09-24-one-send-ingress-the-driver-runs-every-turn).
> No caller runs a turn after that cutover, so no caller owns an aborted turn's
> continuation. The substrate re-drives it or parks it (A3, A4), and the
> binding, the `TurnBound` read status and cancel refusal, and the receipt
> re-drive are deleted (A8). This section still describes current code.

Section 3 makes a crashed direct turn recoverable by anyone: its claim is pinned
to a lease generation that stops holding the lane, and the next generation
reclaims the rows under ADR 0029's fence. That is right for a crash, because
nobody is left to decide. It is wrong for a turn that *aborts*: a live fault
returns `Err` to a caller that is still there, carrying the acceptance receipt
(FIG-3575), and the caller now owns the decision. Left to the generation fence,
the next turn under a new lease generation reclaims the aborted turn's rows and
folds them into its own message block. The model sees the words in a turn they
were never sent to, a host that retries with the same words shows them twice,
and the aborted turn's journal is orphaned: its redrive finds the rows answered
and cedes.

**So an aborted direct turn binds its drive claim to its turn id.** The binding
is a nullable pair on the row beside the claim it binds: `claim_bound_turn_id`
and `claim_bound_receipt_input_id`, the input the turn's acceptance receipt
names. It is not a new lifecycle state: the row stays an open next-turn row
holding its claim, and a CHECK holds the pair all-or-none and to exactly that
shape. Its rules:

- **Written at the abort, before the lease is released.** The direct-turn drive
  writes it on the `Err` path, conditional on the claim still being the one the
  turn drove, so a row another driver settled or reclaimed meanwhile is left
  alone. When the drive effect itself fails after its body claimed rows (its
  journal cannot finalize, say), the turn never learns its claim; it binds the
  claim the accepted row carries under the lease generation the drive ran
  under instead. Writing either before the release closes the window in which a
  successor could reclaim the rows first. It is best-effort like every
  abort-path repair: a store that cannot take the write leaves the rows exactly
  as a crash does. It is also no stronger than its fence: if the live fault was
  a lost lease and a successor generation had already re-claimed the rows, the
  binding is a no-op, and that successor may fold the input into its turn.
- **Never binds a pending queued run's inputs.** A direct turn's drive can
  absorb a run's lapsed frozen members; those rows are skipped, so they lapse to
  the run and its retry re-claims them as before.
- **Tells an abort from a crash by construction.** Only a turn that reaches its
  abort path writes the binding. A worker that dies, or a future that is
  dropped, never does, so its claim lapses with its generation and section 3's
  recovery is unchanged. Neither the lease nor the journal has to be consulted:
  the effect journal lives in another database on SQLite, and the lease cannot
  say why its holder stopped.
- **Excluded from every claim and from claimable work.** The next-turn and
  checkpoint candidate scans, the claim statement's backstop, the
  has-claimable-work probe and the pending-work ordering all skip a bound row,
  under every lease generation.
- **Consumed by exactly two things.** The aborted turn's redrive settles the
  rows with the claim token its journaled drive recorded (section 6), which
  clears the binding with the claim. A redrive whose effect host journaled no
  drive re-takes the rows bound to its own turn id under its own generation
  (`reclaim_turn_bound_inputs`) and drives exactly that set. Or the host cancels
  the input by the receipt, which also returns the claim's other rows, the
  earlier admissions the aborted turn had absorbed, to the queue unbound,
  because the aborted turn can no longer settle them. A cancel of one of those
  other rows on its own is refused (`PendingTurnInputCancelOutcome::TurnBound`,
  naming the turn and the receipt's input): it would change the drive set the
  turn's journal replays. A cancel that covers the receipt's input as well, in
  one target list or one suffix, may take them.
- **Visible.** The pending-input read reports a bound row as
  `PendingTurnInputReadStatus::TurnBound { turn_id, receipt_input_id }`, naming
  the turn to redrive and the input to cancel.

The redrive has one precondition the binding does not remove: the aborted turn's
journal was recorded against the session head it ran on. Once a later turn
commits, the redrive can no longer replay that journal: its acceptance fails
with `SqliteEffectReplayHashConflict` or `PostgresEffectReplayHashConflict`, and
the row stays bound. The host must then cancel the input by the receipt. A typed
refusal for this case, and a bound on how long a bound row may wait, are left to
a follow-up.

Two sweeps meet bound rows. Deleting a session deletes them with every other
row. `vacuum()` reclaims terminal rows only, and a bound row is open, so it
leaves them; so does the FIG-1573 orphan backstop, which ranges over active-turn
rows. There is no sweep that releases a binding: a bound row waits for its
redrive or its cancel, and an operator finds it by its read status.

### The residual duplicate-execution window

Sections 5 and 6 do not close every duplicate-execution window, and the one that
remains is stated here rather than implied.

**The window.** Duplicate execution requires a session-lease handover while the
previous holder is alive but partitioned, mid-turn. In steady state it is zero.
Its rate is approximately the lease-handover rate multiplied by the probability
that a turn is in flight at handover.

**Its granularity.** The loser is fenced at its **first commit attempt**, so the
duplicated work is bounded to a single uncommitted segment — the provider call
and any un-journaled work since its last commit point. The durable record is
never duplicated: two drivers may execute, but only one commits, because the head
CAS admits exactly one.

That holds for admission too. The acceptance's input id is provisioned from its
effect address before the acceptance body runs, so a body re-run because its
outcome was never recorded names the row the first run wrote, and the store
adopts it instead of admitting the same words a second time (FIG-3513).

**Why it is not closed.** Closing it means proving a remote process is not
running, and lease-based systems do not offer that proof. etcd states plainly
that lease-based mutual exclusion cannot guarantee a previous holder has stopped
executing; Orleans documents duplicate-activation windows during silo failure
detection; Temporal specifies activities as at-least-once and pushes idempotency
to the activity. Lash makes the same trade for the same reason, and pays for it
in bounded re-execution rather than in a liveness oracle it cannot build.

**Remediations, documented and not implemented.** Three options exist if the
window ever costs something measurable: a pre-flight fence read before expensive
provider work (narrows, does not close, and adds a read to every turn);
double-run-sensitive tools routed through the journaled effect path so their
results are deduplicated by identity; and faster lease-liveness detection, which
shortens the window at the cost of more spurious handovers. None is implemented
today.

**Trigger for revisiting.** Measured duplicate provider spend correlated with
handover events. Not a hypothetical, and not a code review finding — a number.

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
* The pending-turn-input lane that now carries every turn keeps its "no pin, by
  design" reasoning unchanged, because a direct turn's batch also does not exist
  before its claim.

### Sequencing

FIG-1671 implements this ADR, sequenced after the FIG-1660 and FIG-1663 wave so
it lands on the settled ingress and ordering surfaces rather than racing them.
Its acceptance evidence is the killed-worker recovery runbook extended to a
direct turn: kill the caller mid-turn, let an unrelated worker rediscover and
drive the accepted input, and show the session committing a complete turn the
original caller never saw.
