# Durable acceptance is the sole turn ingress

## Status

Accepted.

## Context

Durable effects need discoverable admission and an owner that can resume them
without the original caller. An execution started only by a caller's future
cannot provide that evidence after the caller disappears. Acceptance, execution
and observation therefore have separate responsibilities.

[ADR 0101](0101-one-session-ingress-carries-every-admitted-item.md) owns the
session's logical ingress and ordering. Pending turn inputs and queued work
remain distinct storage classes within that ingress. Restate owns production
execution; SQLite and PostgreSQL own storage.

## Decision

Every facade turn enters through durable acceptance before execution. The
backend's work driver runs admitted roots. The caller sends and observes; its
future is not the execution authority.

### 1. Single ingress, unconditional

`LashSession::send(input)` and `DurableSession::send(input)` return a
`SendBuilder`. Awaiting it durably accepts the input and returns a `SendHandle`
with a `TurnInputAcceptanceReceipt`. `output` and activity-forwarding helpers
compose acceptance with observation of the root that takes the input. They do
not execute a caller-owned turn.

The row is a Pending Turn Input admission. `NextTurn` is the default; a typed
ingress can address an active turn's checkpoint. The input's run spec is part
of the submitted revision. A nondefault spec that disagrees with the active
turn refuses before acceptance.

Facade sessions bind an explicit store, so the acceptance rule has no
store-capability fast path. The kernel can run a child session within its
parent's execution, but accepts that child's durable input first. A lower-level
store-less runtime can drive an input directly; it is outside the facade's
durable acceptance guarantee.

### 2. The cost is stated, not gated

Acceptance performs a storage write before execution. Root admission and turn
commits are separate writes with separate authority. Their cost informs host
sizing and batching, not whether a turn can omit admission evidence. A cheap
no-record path would change recovery semantics for the same operation.

### 3. Recovery is fully unified

The accepted row and its delivery obligation exist independently of the send
handle. The work driver admits a root under its drive fence, and the root owns
the execution and recorded admission. A worker can resume that root without
the submitting process. The caller can reattach by input id or its stated id.

Dropping an observation future is not cancellation. A host cancels the accepted
input or the running root through the typed cancellation operations. Before
admission, withdrawal has no executed root; after admission, the root's
cancellation policy governs accepted inputs and committed work.

A root that faults remains the engine's work to redrive or park. `Parked` and
`Stalled` are durable observations without an invented completed turn. They do
not transfer execution authority to the caller. Explicit repair and cancellation
use their addressed durable identities.

### 4. No new idempotency machinery

`SendBuilder::id` states the host's idempotency key and names the root the input
starts. It is stored verbatim as `source_key`. The store compares the admission's
submission digest: identical content returns the existing acceptance, and
changed content refuses with `PendingTurnInputSourceKeyConflict`.

An explicitly provisioned input id likewise names one admission. The same
submission in the same session adopts it; different content or another owning
session refuses with `PendingTurnInputIdConflict`. One source key or input id
cannot silently name two submitted revisions.

A send without a stated identity gets a fresh id. If acceptance commits but the
caller does not learn that identity, a fresh retry can admit a second input.
A host requiring at-most-once submission names its id before sending and reuses
it for that exact revision. This uses the input's existing identity and digest,
not another key-to-acceptance table. Retention of that evidence follows the
owning session's lifecycle and host retention policy.

### 5. A turn settles the acceptance admitted to its root

The root's recorded admission binds the rows it drives to that root. Settlement
names the root and turn and runs under the drive fence and head commit authority.
The store predicates completion on the row's `admitted_root`. A turn cannot
settle a row that another root owns, and an unmatched settlement cannot be
reported as a successful write.

Terminal settlement clears admission bindings. A root's terminal write releases
remaining bound inputs according to their delivery rules; an active-turn input
whose turn ends is made next-turn work. No row remains bound to a root with
terminal evidence.

Admission and commit identity provide replay acknowledgement. A redrive does not
commit the same content under fresh authority merely because its first
execution lost a fence. Supersession cedes without writing; the effect and commit
receipts determine whether recorded work can be replayed.

### 6. Acceptance is journaled, so engine replay re-derives it

A host send is an explicit keyed storage admission. A child-session acceptance
executed inside a durable handler is instead `AcceptTurnInput` on the journaled
runtime-effect seam. Its input id is provisioned from the acceptance address
before the body runs and its source key is the turn id. If the body commits but
the journal has not recorded its result, retry adopts the same row. Engine
replay returns the recorded acceptance rather than minting another one.

The acceptor drives the accepted row itself, inline in the parent's execution.
That drive is the ask the row's ingress obligation owes, so the acceptance
takes the obligation's claim in the transaction that admits the row, held for
the relay's claim TTL. A relay pass asks the session for the row only once the
claim has lapsed, which presumes the acceptor lost; until then no second drive
runs the same root beside its acceptor.

The claim is a liveness hint, not the exclusion. An acceptor that is alive but
slower than the claim loses it to a relay pass, and the session's drive then
admits the row's root as an engine run of its own. From that admission on the
root's recorded executor decides who runs it: two different executors an engine
holds a run for (the root's own run, or the run of a process that drives the
root inline) never share a root, and an acceptor's inline drive, under any
scope, never takes a root such a run holds. The acceptor's drive admission finds
the unfinished root recorded under the other executor, seals nothing over its
fence, and fails retryably with `SessionRootPending`; `admit_root` itself
refuses the same mismatch with `RootHeldByAnotherExecutor`, which the root
records as the refusal `HeldByAnotherExecutor` and cedes. The acceptor's retry
finds its input answered once that run ends the root. A lost acceptor recorded
nothing, so the relay's ask drives its root once, as the session's own run; an
acceptor that recorded its root before it was lost is redriven by its own
engine, and the lost-root pass ends the root if that run is gone for good. A
inline drive no engine holds (an in-process session drive or queue drain) is
outside the rule: its root is resumed by the next drive, and it resumes an
unfinished root, under the drive fence as before.

The root then issues journaled `AdmitRoot`, keyed by the root rather than by a
particular drive attempt. Its stored admission includes the exact inputs and
queued work, base head, turn index and executable generation. The store binds
those rows and records the composition in one transaction. A crash between the
store commit and recording the effect outcome therefore leaves a recoverable
composition; retry reads it back instead of selecting again.

A root redrive uses that recorded admission and base head. It does not read live
pending rows to reconstruct the run, include later arrivals, or default to the
head its own first commit moved. Pruning admission evidence cannot change the
recorded inputs the journal replays; retention must preserve any admission and
base still needed by the root's recovery.

A first admission that cannot reach its head takes nothing. If the row is still
present, the step retries the admission race without recording a permanent
refusal. If its head is absent because it is settled, cancelled or pruned, the
root cedes with `accepted_turn_input_ceded`. An infrastructure failure before
an admission answer is a retryable uncommitted derivation, not a replayed
terminal refusal.

### The residual duplicate-execution window

Acceptance identity and fenced commits prevent duplicate durable admission or
commit for the same recorded operation. They do not promise exactly-once
physical execution. A failed worker attempt can execute work whose durable
answer is not yet recorded, so recovery can repeat it. Tool and provider work
must follow their own effect and usage contracts; a drive fence protects the
writes it owns rather than proving that every remote body stopped.

## Alternatives considered

A second turn-intent marker omits the existing input's admission, cancellation,
identity and terminal evidence or duplicates them in another row class. The
Pending Turn Input row already has an owner and reclaim trigger under ADR 0067.

A no-record path chosen by backend capability makes recovery depend on storage
selection and leaves some effects without enumerable admission. The facade's
bound store supplies one acceptance contract.

Giving the caller's future a privileged reservation introduces a second
execution authority and a caller-liveness question. The work driver owns the
root; the caller observes it.

Putting acceptance and admission into one at-least-once body makes a crash
between their writes repeat a partially completed body. Separate idempotent
writes and recorded root composition keep each replay obligation explicit.

Reconstructing a drive from current pending rows changes its words, ordering
and authority after later arrivals or retention. The recorded admission is the
run's replay input.

## Consequences

Ingress, idempotency, cancellation and recovery share one set of semantics.
Acceptance costs a write, and an unacknowledged send without a reusable host id
can duplicate a submission. A caller disappearing cannot make an accepted root
undiscoverable. Observation handles can be replaced without changing the
execution's identity or recording another input.

## Executable evidence

- [Send acceptance](../../crates/lash/src/send.rs#L351),
  [host id](../../crates/lash/src/send.rs#L254) and
  [durable-session send](../../crates/lash/src/durable_session.rs#L212)
  implement facade ingress.
- [Digest and id verdict](../../crates/lash-core-store/src/store_backend_support/turn_input_batch.rs#L35)
  implements existing/new admission and typed conflicts.
- [Child-session acceptance](../../crates/lash-core/src/runtime/turn_loop/accept.rs#L169)
  journals its provisioned admission.
- [Recorded root admission](../../crates/lash-core/src/runtime/drive/root.rs#L1),
  [journaled step](../../crates/lash-core/src/runtime/drive/root.rs#L64) and
  [store composition](../../crates/lash-core/src/runtime/drive/root.rs#L713)
  preserve the exact drive set and base across recovery.
- [Settlement and release SQL](../../crates/lash-store-sql/src/turn_ingress/pending_inputs.rs#L212)
  binds settlement to the root and releases bindings at terminality.
- [Acceptance laws](../../crates/lash-conformance/src/conformance/direct_turn_acceptance.rs#L1)
  and [crash-matrix redrive](../../crates/lash-conformance/src/conformance/turn_crash_matrix/after_commit_redrive.rs#L1)
  cover admission, journal replay and commit acknowledgement.
- Store laws run on SQLite file, SQLite memory and PostgreSQL. Effect-host laws
  run on the in-process Restate server double, live Restate and lash-sim's
  in-process effect host. Upgrade proofs use the synthetic-next tier.
