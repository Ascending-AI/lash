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
remain distinct storage classes within that ingress. Lash's durable engine
executes over the lash store
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md)).

## Decision

Every facade turn enters through durable acceptance before execution. The
session actor's owner runs admitted runs. The caller sends and observes; its
future is not the execution authority.

### 1. Single ingress, unconditional

`LashSession::send(input)` and `DurableSession::send(input)` return a
`SendBuilder`. Awaiting it durably accepts the input and returns a `SendHandle`
with a `TurnInputAcceptanceReceipt`. `output` and activity-forwarding helpers
compose acceptance with observation of the run that takes the input. They do
not execute a caller-owned turn.

The row is a Pending Turn Input admission. `NextTurn` is the default; a typed
ingress can address an active turn's checkpoint. The input's run spec is part
of the submitted revision. A nondefault spec that disagrees with the active
turn refuses before acceptance.

Facade sessions bind an explicit store, so the acceptance rule has no
store-capability fast path. The kernel can run a child session within its
parent's execution, but accepts that child's durable input first. A lower-level
store-less runtime can execute an input directly; it is outside the facade's
durable acceptance guarantee.

### 2. The cost is stated, not gated

Acceptance performs a storage write before execution. Run admission and turn
commits are separate writes with separate authority. Their cost informs host
sizing and batching, not whether a turn can omit admission evidence. A cheap
no-record path would change recovery semantics for the same operation.

### 3. Recovery is fully unified

The accepted row exists independently of the send handle, and its accepting
transaction wakes the session actor (ADR 0132 §12). The owner admits a run
under the actor's epoch fence, and the run owns the execution and recorded
admission. Any node that claims the actor can resume that run without the
submitting process. The caller can reattach by input id or its stated id.

Dropping an observation future is not cancellation. A host cancels the accepted
input or the running run through the typed cancellation operations. Before
admission, withdrawal has no executed run; after admission, the run's
cancellation policy governs accepted inputs and committed work.

A run that faults resumes from its committed state on the actor's next
activation, or parks once its activation budget is spent (ADR 0132 §3).
`Parked` and `Stalled` are durable observations without an invented completed
turn. They do not transfer execution authority to the caller. Explicit repair
and cancellation use their addressed durable identities.

### 4. No new idempotency machinery

`SendBuilder::id` states the host's idempotency key and names the run the input
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

### 5. A turn settles the acceptance admitted to its run

The run's recorded admission binds the rows it executes to that run. Settlement
names the run and turn and runs under the actor's epoch fence and head commit
authority. The store predicates completion on the row's `admitted_run`. A turn
cannot settle a row that another run owns, and an unmatched settlement cannot
be reported as a successful write.

Terminal settlement clears admission bindings. A run's terminal write releases
remaining bound inputs according to their delivery rules; an active-turn input
whose turn ends is made next-turn work. No row remains bound to a run with
terminal evidence.

Admission and commit identity make a repeated commit idempotent. An owner whose
epoch went stale cannot commit at all: its transaction fails the fence, rolls
back and drops the actor, and the next owner resumes from committed state
(ADR 0132 §3). The head compare-and-set and the commit receipt decide whether
work is already committed.

### 6. Child acceptance is a mailbox write, and admission is recorded

A host send is an explicit keyed storage admission. A child-session acceptance
inside a parent's execution is a store-local effect: the parent's transaction
that records it inserts the child's input, keyed by the acceptance address and
with the turn id as its source key, and wakes the child session actor
(ADR 0132 §5 and §12). A recomputed parent stretch that accepts again adopts
the same row.

The child session is its own actor. The node that accepted the input may claim
it at once and run the child's run beside its parent, but only the child
actor's epoch decides who runs that run: a claim bumps the epoch, so two
executors never share a run, and a stale one fails its next fenced write. The
parent awaits the child's answer through a bounded wait on the child's run
terminal, and reads the outcome the child's owner committed from the run's
terminal evidence on the durable head; a refused run answers its refusal, as
the typed `SendOutcome::Refused` answer rather than an error of the read
(FIG-5092).

Run admission is keyed by the run. Its stored admission includes the exact
inputs and queued work, base head, turn index and executable binding. The store
binds those rows and records the composition in one transaction, so a crash
leaves either no admission or a complete one; resume reads it back instead of
selecting again.

A resumed run uses that recorded admission and base head. It does not read
live pending rows to reconstruct the run, include later arrivals, or default
to the head its own first commit moved. Retention must preserve any admission
and base still needed by the run's recovery.

A first admission that cannot reach its head takes nothing. If the row is still
present, the owner retries the admission without recording a permanent
refusal. If its head is absent because it is settled, cancelled or pruned, the
run cedes with `accepted_turn_input_ceded`. An infrastructure failure before
an admission answer commits nothing and is recomputed from committed state.

### The residual duplicate-execution window

Acceptance identity and fenced commits prevent duplicate durable admission or
commit for the same recorded operation. Physical execution between a started
row and its outcome follows the execution policy: a `Once` body never runs
twice and a `Repeatable` body may (ADR 0132 §5). Tool and provider work follow
their own effect and usage contracts; the epoch fence protects the writes it
owns rather than proving that every remote body stopped.

## Alternatives considered

A second turn-intent marker omits the existing input's admission, cancellation,
identity and terminal evidence or duplicates them in another row class. The
Pending Turn Input row already has an owner and reclaim trigger under ADR 0067.

A no-record path chosen by backend capability makes recovery depend on storage
selection and leaves some effects without enumerable admission. The facade's
bound store supplies one acceptance contract.

Giving the caller's future a privileged reservation introduces a second
execution authority and a caller-liveness question. The work driver owns the
run; the caller observes it.

Putting acceptance and admission into one body makes a crash between their
writes repeat a partially completed body. Separate idempotent writes and
recorded run composition keep each step explicit.

Reconstructing a run from current pending rows changes its words, ordering
and authority after later arrivals or retention. The recorded admission is the
run's resume input.

## Consequences

Ingress, idempotency, cancellation and recovery share one set of semantics.
Acceptance costs a write, and an unacknowledged send without a reusable host id
can duplicate a submission. A caller disappearing cannot make an accepted run
undiscoverable. Observation handles can be replaced without changing the
execution's identity or recording another input.

## Executable evidence

- [Send acceptance](../../crates/lash/src/send.rs),
  [host id](../../crates/lash/src/send.rs) and
  [durable-session send](../../crates/lash/src/durable_session.rs)
  implement facade ingress.
- [Digest and id verdict](../../crates/lash-core-store/src/store_backend_support/turn_input_batch.rs)
  implements existing/new admission and typed conflicts.
- [Child-session acceptance](../../crates/lash-core/src/runtime/session_manager/session_init.rs)
  provisions its admission.
- [Recorded run admission and restore](../../crates/lash-core/src/runtime/durable/session.rs)
  preserve the admitted set across recovery; [store composition](../../crates/lash-core-store/src/store/admission_plan.rs)
  selects that set before it is recorded.
- [Settlement and release SQL](../../crates/lash-store-sql/src/turn_ingress/pending_inputs.rs)
  binds settlement to the run and releases bindings at terminality.
- [Ingress integrity laws](../../crates/lash-conformance/src/conformance/runtime_persistence/ingress_integrity.rs)
  cover retained submission identity and settlement at the store boundary.
  The former direct-acceptance and turn-crash registrations are retired;
  their presence in earlier revisions is not current runtime proof.
- Store laws run on SQLite file, SQLite memory and PostgreSQL. Laws run the
  production runtime over a fault-injecting store with labelled commits, a
  virtual clock and `SimNodes` (ADR 0132 §14). Upgrade proofs use the
  synthetic-next tier.
