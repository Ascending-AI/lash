# 0099: Tool children of effect groups have one lifecycle — live, closing, settled

## Status

Accepted and implemented. Restate is the only effect engine
([ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md)):
the group authority this ADR names is the Restate `EffectGroupIndex` object,
which also answers the group's own notifications (§2), and the SQL stores hold
storage only. A process opener is
its minted `ProcessId`
([ADR 0107](0107-a-process-is-named-by-a-minted-id-a-start-by-its-key.md)).
Queued-work admission and recovery are
[ADR 0101](0101-one-session-ingress-carries-every-admitted-item.md)'s, and
process recovery is [ADR 0110](0110-the-engine-owns-process-recovery.md)'s.
Every tool child is an attempt ([ADR 0116](0116-tools-are-opaque.md)).

This ADR refines [ADR 0025](0025-bounded-journals-are-an-effect-controller-obligation.md),
[ADR 0042](0042-tool-attempts-are-atomic.md),
[ADR 0062](0062-the-typescript-dialect-is-an-exact-ecma-262-subset.md),
[ADR 0065](0065-concurrent-settlement-is-a-durable-group-at-the-effect-host-seam.md),
[ADR 0094](0094-child-lifecycle-is-a-registration-fact-settled-by-scope-end.md)
and [ADR 0095](0095-processes-are-values-and-process-controls-are-tools.md) for
tool children of effect groups; each of them links here. A loser of an
aggregate belongs to its opener and does not run past the opener's end as
implicit durable background work; work that must outlive the opener is an
explicit process.

## Context

[ADR 0065](0065-concurrent-settlement-is-a-durable-group-at-the-effect-host-seam.md)
made concurrent settlement a durable group at the effect-host seam and left one
question open: what happens to a tool child that lost a race when its opener
ends or dies. Two endpoints are rejected below.

**A (reduced)** made a loser durably host-owned, finishing after its opener
ended. It buys unobserved post-turn side effects: a `send_email` that loses to a
timer realizes its delivery intent with nobody watching, and a losing
`spawn_indexer` starts a second process that no program holds a handle to.

**B-prime** gave the loser to its opener and abandoned it, with a recorded
decision, on a crash past the winner. It destroys recorded declarations that
[ADR 0042](0042-tool-attempts-are-atomic.md) protects — the phase between "final
attempt recorded" and "declarations drained" — and it mis-identifies two events:
ADR 0094 already calls an uncommitted crashed turn "interrupted, not ended", and
a reference system that drops a late activity result does so at *workflow
close*, not whenever a live workflow's worker crashes.

The **hybrid** below is the smallest endpoint that breaks no standing law. It
costs more than B-prime by exactly the recovery ownership B-prime could not
actually delete.

### Anchors

Anchors quote a path and source text, never a line number: a line pin rots on
the next edit above it and then points confidently at the wrong construct, while
a quoted phrase either still exists or visibly does not.

## Decision

### 0. The lifecycle

**live.** Aggregate selection cancels nothing; a losing child keeps running,
exactly as a losing promise does in ECMA-262. Worker loss does not end an
opener: an accepted child is *recovered* while its opener lives, from durable
input. Every host journals, so no child is held only in memory (§14).

**closing.** Entered by a durable transition (§7), not by a worker dying. New
tool work stops, cancel-eligible attempts become cancel-decided, and every
attempt whose final result already committed drains its declarations and records
its projection.

**settled.** Finalization has committed its outcome and accounting, parent end
is recorded, and retirement has completed.

Two latencies, and they are separate numbers: **winner latency** (when `race`
resumes — at the first settlement) and **finalization latency** (when the opener
reports success — after closing drains its protected obligations). No successful
turn latency bound exists: every protected committed obligation must finish.

**Background work that must survive `finish` is a process the program named.**

---

### 1. Logical opener identity

**An opener is `Turn { session_id, turn_id }`, `SessionOperation {
session_id, operation_id }` or `Process { process_id }`.** `EffectOpener`
(`crates/lash-core-store/src/effect_opener.rs`) is that identity. It is stable
across worker attempts and segments. A process opener is the process's minted
id, which is never reused (ADR 0107), so a process registered later can never
share it and alias another process's groups, closes or cancellation fences.

**Every logical turn a shift runs is opened by `Turn(logical run)`.** Its
later physical turns share the opener: a frame or terminal-checkpoint
follow-on, and a follow-on that a later shift recovers under a recovery run
of its own. The run's terminal evidence closes that scope once. Admission,
selection, retry and abandonment of queued work are ADR 0101's.

**A session operation is a durable opener that runs no turn.** A host command
runs under `SessionOperation(session, batch)`, so a redrive of the unsettled
command under another run replays its effects, and a shift request's
admissions are recorded under an operation named by the request. An operation
has no logical run, and a logical turn started under one is refused. Its
session's close ends its scope.

**`SessionDelete` and `RuntimeOperation` scopes are not openers and run no
cells.** A scope that is none of the three is refused with a typed
`EffectOpenerError` rather than given an invented identity: widening what an
opener is changes this section, never a fallback. Group identity, retained
authority, cancellation, usage attribution and retirement all bind that exact
opener.

**A process-backed session turn runs its cells under the process opener.** A
`ProcessInput::SessionTurn` row — what every `agents.spawn` child is — creates a
child session and runs one turn of it under the *process's* scope: the process
session runner
(`crates/lash-core/src/runtime/session_manager/process_runners/session.rs`)
stamps the create request `caused_by` the process and admits the child's first
turn under the process scope. So that child turn's cells are opened by the
process and not by the child turn: a worker retry keeps the process id and
reuses the journal.

**One derivation, and no name lookup.** `EffectOpener` is the single owner
vocabulary, and it is derived exactly once — `EffectOpener::for_scope` — from
the `AdmittedScope`. Every surface that must name the owner calls it: the
lifecycle parent a child start declares, the recorded attempt a tool body runs
inside, and the host identities the Lashlang bridges mint. There is no second
derivation, and none of it asks a registry. It is an enum with a `kind` tag
rather than a rendered string because a turn's scope identity is free-form text
that can spell a process opener's rendering exactly; untagged, the two openers
would mint one identity. Where the derived opener is minted into a key preimage
or an embedded id, `EffectOpener::identity_encoding` — the length-prefixed
canonical form — is the only encoding; `render` is the diagnostic projection
and is free to collide.

**A dead worker is neither live-ended nor closed.** Recovery classifies an
opener by the durable closing fact of §7, never by the liveness of a lease or
by what one process can see: a group open in another process is not
distinguishable from a closed one by a local guard, so closedness is the
group authority's fact.

**Ordinal carriage.** A lashlang aggregate is addressed by the issue ordinal of
the command that formed it (ADR 0103), which rides the VM continuation: a
process body's ordinals live in `ReplayOrdinalsState::commands` inside the
`LashlangSegmentState` its runtime serializes at a boundary, so two identical
`race` calls straddling a segment boundary take different ordinals. No key
carries the aggregate's instruction pointer or an occurrence;
`occurrence_counters` stays on the continuation as trace and graph metadata
only.

**The aggregate group key is positional.** A lashlang aggregate's group key is
the opener's group prefix followed by the issuing command's key:
`{scope_id}:group:P:{k:010}`, `P` the run's replay namespace (ADR 0103). It
carries no content, compiler output or parent effect id, and it keeps the
opener's `{scope_id}:group:` prefix, so the opener's end finishes the groups a
crashed cell formed exactly as it finishes any other it owns. A
recorded-frontier read asks for the group rows under that prefix and reads them
back as the commands that formed them. Its children are `P:{k:010}:child:{i}`,
`i` the leaf's first-appearance index; its timers' admission sample is
`P:{k:010}:timers-admitted`. Because the key is positional, an aggregate whose
arguments, grant or timer layout changed still meets its journal.

`batch_id` stays on the group as a **checked fact**: a content digest (identity
family `lash.aggregate-content`) over the tool calls and the timers' positions
and durations, with no instruction pointer or call site. The lashlang caller
opens its group with `GroupReopen::RetainedContent`: a reopen must match the
recorded shape (§3, W1) **and** every offered child must be the retained child
at its position, compared as canonical envelopes, or the open refuses before
any child is claimed — re-typed by the run to `lashlang_cell_replay_divergence`.
Other callers use `RetainedShape` and the
`{scope_id}:group:{parent_effect_id}:{batch_id}` key, whose batch id is
`TOOL_BATCH_FAMILY_VERSION` 3. Both key shapes carry the opener's scope, so no
two openers share a group although the group authority is keyed by the group
key alone.

---

### 2. Invocation driver

**A tool child is a replayable invocation driver.** Retry, completion-key
derivation and deferred await are *coordination* and run at handler level on the
child's own admitted controller. Only atomic attempts run inside recorded bodies.
Every tool child runs one recorded attempt under its driver
([ADR 0116](0116-tools-are-opaque.md)).

This is ADR 0042's structural rule unchanged — "A recorded body must not emit
commands into an ordinal-addressed journal" — and it is the reason coordination
cannot live inside a `ctx.run` closure: the closure may emit no journal command.
`crates/lash-restate/src/lib.rs` states the constraint ("not to call the Restate
context from inside the closure").

A second, weaker reason is often overstated and is stated here precisely. In the
pinned `restate-sdk` 0.11.1, a run future in its `ClosureRunning` state polls the
closure without observing SDK cancellation at that point; cancellation is checked
before the closure starts. That is a property of this SDK on this path, not a
universal claim about every SDK, and it does not prevent **explicit cooperative
cancellation inside opaque work**. The running attempt is cancelled through the
separately specified cooperative path (§4), not by the engine reaching into it.

**One resolver answers "what runs this child" for first dispatch and for
recovery.** ADR 0065 already made the registered `GroupExecutors` resolver
normative, and the code has it:
`crates/lash-core-execution/src/runtime/effect/group_executors.rs` resolves
through `fn executor_for`. **No caller-closure
route is reintroduced**: a caller vector answers only first dispatch, while
retry, drain, a resuming process and a fresh handler execution all run with no
caller in scope.

**On Restate, children are `call` children of the dispatch with the child's
replay key as the idempotency key.** The pinned VM's implicit cancellation
covers tracked `call` children, and it is never the sole close protocol (§4).

**The dispatch registers its children in one index step.** It issues every child
call, then records the full position-to-invocation map and moves the group to
ready in one exclusive index handler, which answers every READY subscriber —
the opener's and each waiting child's — together; nothing is awaited before
that step. READY is notification only for a child: a child is admitted by a
fresh read that finds its own recorded id. The adopted dispatcher's id is kept,
so a retirement before registration cancels the dispatch and the child calls it
tracks, and a retired group is never made ready.

**The group index answers the group's own notices.** READY (the group left
preparing, or was refused), RANK (a rank is inside the seated prefix, §5), the
§5 barrier (`Drained`: every committed sibling ranked below a rank has seated)
and a child's cancel fact (its cancel was decided, or it seated) are facts of
the index record, so the index owns their delivery; none of them goes through
the generic durable-wait services. A waiter creates an awakeable in its own
journal and subscribes it under its notice with the index's exclusive
`subscribe`. The index answers from its record at once when the notice already
holds, and otherwise records the subscriber before any later transition can
run. Every handler whose state change can make a notice true — the
registration, a refusal, a seat, a close, a retirement — stores its record,
then the subscribers it keeps, then completes the rest from its own journal: a
completion is a command of that invocation, never a call to another service,
so a seat invokes nothing. A crash between the stored record and the
completions replays the same stored list and completes the same subscribers.
Every answer is monotonic under the index's transitions, and retirement answers
every outstanding subscriber `Retired` and every later one from the tombstone.
A subscription is idempotent by awakeable; a waiter whose other race arm won —
the turn-cancel gate or the process cancel promise — withdraws it with a
one-way `unsubscribe`, and a group holds a bounded number of subscribers, past
which it refuses a subscription typed. A notification is a hint to re-read the
authority, never a permission of its own. A child's step-boundary cancel read
is the index's shared `child_cancel` read; the turn's cancel gate and the
waits the group does not own — a tool's completion key, a wait child's event —
stay on the durable-wait services.

---

### 3. Execution authority and accepted work

**The durable child request reuses `ProcessExecutionEnvRef` and existing artifact
ownership, and adds only what environment capture lacks.** Capture already
publishes under an owner —
`crates/lash-core-execution/src/session/execution_context.rs` exposes
`captured_process_execution_env_ref(&self, owner: &crate::ArtifactOwner)` — and
what it captures is the session's plugin options, policy and recorded render:
`crates/lash-core-store/src/process_identity.rs` defines
`ProcessExecutionEnvSpec { plugin_options, policy, render }`.

That is not a tool execution descriptor. The child request adds the admitted
grant (kept separate: `crates/lash-core-execution/src/tool_provider.rs` carries
`pub struct PreparedToolBatchCall { pub call, pub execution_grant:
Option<Box<ToolExecutionGrant>> }`, and the grant holds `source_id` and
`execution_binding`), the admitted scope, lineage, and cancellation authority.

**Before acknowledging open or dispatching any child, retain the complete
accepted membership and a reconstructible request for every unique child,
including unclaimed children**: input, replay identity, admitted grant, the exact
opener and scope, lineage, cancellation authority and environment reference. A
persisted accepted group may never exist without discoverable complete input.
The group record alone cannot supply it: `EffectGroupRecord` in
`crates/lash-core-execution/src/runtime/effect/group.rs` stores "How many
children the group has", a count, and nothing that reconstructs one.

**Environment bytes are protected under `ArtifactOwner::Execution` through their
last retained dependency**, including successor segments and close recovery.

**A reopen uses the recorded facts, not current session policy or fresh
admission.** Claims suppress duplicate *authoritative completion*; they do not
make unrecorded opaque I/O exactly-once, which ADR 0042 already concedes. **A
crash after dispatch but before the child's invocation id is published recovers
that dispatch's original identity and never issues a fresh unrelated call.**

**`RuntimeExecutionContext` is never serialized and there is no second
environment store.** The context is borrowed live state carrying senders, plugin
handles and a turn's authority, none of which survives a handler boundary.
Semantic completion facts travel; live channels do not.

**A durable-owner publish keeps its strict retirement behaviour.**
`captured_process_execution_env_ref` says why in source: "Every caller publishes
under a durable owner and then persists the reference … so a retirement must
surface at publish time rather than hand back a reference to bytes the fence
already reclaimed." A child request is on the durable side of that split.

#### What a reconstructible request contains

The clause above names the facts a child request carries. The shape settles
four further questions.

**A tool child needs a command of its own.** ADR 0065 recorded that "groups
introduce no new command variant, because what is new is the *composition
above* attempts". That holds for every child the journal could already name and
fails for a tool child. `crates/lash-core-execution/src/runtime/effect/envelope.rs`
carries `ToolAttempt { call, execution_grant, attempt, max_attempts }` — the
atomic body of *one* attempt, so a driver expressed as one could not retry,
because a second attempt is a second envelope with a second hash. §2's
invocation driver is not an attempt. The retained request is therefore the
payload of its own `ToolInvocation` command rather than a second record beside
an existing one:
one shape, so the recorded authority and the hashed envelope cannot disagree.

**1. An ungranted call pins its admitted manifest.** A reopen may not consult
the live Tool Catalog. A granted call already satisfies this, because
`ToolExecutionGrant` exists to "validate granted call arguments without
consulting the current Tool Catalog" and carries its own manifest and contract.
An ungranted call is admitted by catalog membership, and the catalog is live
state. The smallest fact that closes the gap is the admitted `ToolManifest`,
because the manifest is what the catalog is consulted *for*:
`resolve_callable_manifest_by_id` and its siblings in
`tool_dispatch/preparation.rs` return a manifest and nothing else, and
`ToolManifest` carries `retry_policy` and `argument_projection` inline. The two
cases are one field with two arms, not two optional fields, because "neither"
and "both" are not states a child can be in. No retry policy is stored beside
the manifest, since the manifest already holds it.

**1b. The opener is a typed identity, not a scope.** The request records §1's
`EffectOpener`, the identity shared by this request, by recovery and by the
Lashlang host bridges. The child's *claim address* stays an `ExecutionScope`,
which is what the journal fences a row on.

**2. Completion routing is recorded, not re-derived.** Completion-key
preparation answers `Issued | NotNeeded | Unsupported` from two live inputs —
whether the tool may defer, which consults the live registry or provider, and
whether the host routes completions durably. Both are deployment facts at
recovery time and admission facts at formation time. The request records which
of `inline` or `durable` the child was admitted under, so a recovered child
never derives a key nothing will resolve. The request's cancellation authority
is required, so every child names the binding that may cancel it.
`ToolChildCompletionRouting` has exactly those two arms.

**2b. The cancellation authority is a validated identity.** It is the value
`turn_control_binding_id_for_scope` mints and `binding_id_admits_scope` checks —
the address the cooperative cancel path signals and the one §4's cancel
disposition is fenced on — carried as `TurnControlBindingId`, a newtype with a
validated constructor, because a frozen durable shape may not hold an
unvalidated identity. It is `None` in exactly one case: an opener whose
controller participates in turn control locally rather than through a durable
journaled authority, where there is no durable address to record.

**2c. The environment reference is required.** §3 makes it part of the retained
authority, and the capture is total —
`RuntimeExecutionContext::captured_process_execution_env_ref` returns
`Result<ProcessExecutionEnvRef, _>`, inheriting or publishing, never absent. An
optional field would be a representable state with no producer, and a recovered
child that met it would have to invent an environment, which is the silent
default §3 exists to prevent.

**3. `AttachmentSourcePolicy` is deployment wiring.** It is an
`Arc<dyn AttachmentSourcePolicy>` on `ToolDispatchContext`, exactly as much
host-installed wiring as the tool implementation behind it, and is not
recorded. The same holds for the registries, the session services, the event
sender and the clock.

**4. Turn context is never recorded.** `TurnContext` holds only live runtime
correlation and has no `Serialize`. The request carries no correlation; process
runners construct their tool dispatch with `TurnContext::default()`. A send
states no prompt (the prompt is the protocol's recorded config, ADR 0030),
and a tool reads no live plugin input (ADR 0101). So the request records no turn-context payload and needs no
refusal.

---

### 4. Commit versus cancel arbitration

**Cancellation requests are not cancellation decisions.** The final-attempt
record and the cancel disposition compete at **one durable, fenced linearization
point**, and exactly one may commit. A winning final record retains settlement
ownership. A winning cancel disposition refuses any subsequent attempt-final
record. Signalling the body and the bounded local grace *follow* the cancel
decision and cannot reverse it. **A final record found after recovery is
protected even if its in-memory commit notification was never published.**

The point is the group authority's exclusive `commit_child` handler, and the
decision is journaled in its answer. An in-process signal — a watch send
followed by a gate wait — is correct in one process and is not an arbitration
primitive across a crash, so none is used.

**Every tool child is an attempt, and two parked shapes add obligations at the
point.**

- **Declared starts.** A child that parks on `PendingResolver::DeclaredStart`
  launches the start before its terminal is armed. The launch is the start's
  own journaled admission, `process:start:{start key}`, under the call's
  cancel fence, so exactly one of the launch and a cancel decision lands
  first, and a launch admitted before the decision still realizes on a
  redrive (`crates/lash-core-execution/src/tool_dispatch/pending_resolver.rs`).
  The launch reserves no rank: the child's terminal rank is reserved only when
  the result or the cancel reaches the point
  ([ADR 0116](0116-tools-are-opaque.md) §3.2).
- **Cancel obligations.** A cancelled or timed-out child parked on a
  runtime-owned resolver with `CancelHint::CancelExternalWork` records a
  cancel obligation in the cancel disposition's commit, and drains it before
  it settles `Cancelled` ([ADR 0116](0116-tools-are-opaque.md) §3.4). An
  unresolved child stays cancellable.

#### Transition table

One child. **Cancel-decided** means the cancel disposition won the linearization
point. Child disposition, rankability, and the opener's live/closing/settled
phase are three separate facts; nothing below waits on opener close.

| state | cancel decision commits | final record commits | drain completes | grace expires | worker loss |
|---|---|---|---|---|---|
| **accepted, unclaimed** | → **cancel-decided** | n/a | n/a | n/a | → *accepted* (recovered while the opener lives, §1) |
| **executing, uncommitted** | → **cancel-decided**; body signalled, grace armed | → **committed** | n/a | n/a | → *executing* (resumed from the retained request) |
| **committed** (final recorded, intents not drained) | **refused** — the point is already taken; a successor minted after a `Cancel` close is still admitted to drain (§8) | n/a | → **drained** | not armed | → **committed**; recovery finishes the drain from the retained final (§5, §8) |
| **drained** (intents recorded, projection durable) | refused | n/a | n/a | n/a | → **drained** |
| **rankable** → **settled** | refused | n/a | n/a | n/a | → unchanged |
| **cancel-decided** | no-op (idempotent) | **refused**, typed, no journal write | discharges any already-admitted protected obligation | → **logically cancelled** | → *cancel-decided* (resumed from the record) |

**Rankability follows the child's own obligations, not the opener's phase.** A
committed final proceeds through intent drain and durable projection to rankable
while the opener is live *or* closing, so a live aggregate reaches rankability
and its consumer resumes. **Grace expiry never changes the winner**; it bounds
how long the local drain waits before the child is *logically* cancelled, which
never requires proof of physical stop.

#### What the fence covers, and what it cannot undo

**Cancellation forbids new unprotected semantic admission under the cancelled
invocation.** It does **not** undo already-admitted commands and does not cancel
committed descendant obligations; those retain authority to finish under the
original opener.

Subject to that, the fence covers four sinks: child dispatch, nested semantic
writes (triggers, process registration, the live possession and usage buffers for
*new* admission), completion delivery, and rank insertion. A refusal that guards
only rank insertion arrives after the escaped coordinator's writes have landed.

**Known usage is never fenced out.** See §13: cancellation refuses semantic
results and new work; it does not discard usage already attributable to the
admitted child.

**Neither close nor cancellation acknowledgement permits deletion.** Retirement
requires discharged obligations, no retained consumer dependency, and a surviving
identity fence. A scope is quiescent only when no recorded effect is executing
and every recorded group's index reports no unsettled child
(`scope_effects_and_groups_are_quiescent` in
`crates/lash-restate/src/durable_wait.rs`); session deletion's own exclusion is
§7's.

**Restate's engine cancellation is never the sole close protocol.** The pinned
shared core cancels tracked child calls from inside `do_await` without consulting
any Lash commit fence, so dispatching children with `.call()` does not protect a
committed final by itself. The journaled cooperative path and retained
protected-work recovery must preserve this section's rules even when implicit
cancellation interrupts a child invocation.

**The completion-delivery fence closes the key with the decision.** The
substrate that owns a child's cancel decision closes the child's completion key
in the same step, so a late `resolve` of that key is refused with
`RuntimeEffectGroupChildCancelDecided`, writes nothing, and leaves the recorded
disposition `Cancel` (W17).

---

### 5. Rankability and intent order

**A child becomes rankable only after its own authoritative final intent outcomes
and its required projection.** Publishing a rank earlier would let a consumer
observe a settlement whose declared effects have not happened, and then checkpoint
past it.

#### Intents are admitted in rank order, not in source order

**The §4 linearization point reserves the child's settlement rank.** A final
record that wins the point takes the group's next rank in the same serialized
step, and a cancel decision takes the next rank the same way; a retried commit or
cancel allocates nothing. The rank is durable, monotonic per group and journaled
in the commit's answer. There is one order, the order of §4 decisions; commit
order is that order restricted to committed children. **The seat publishes the
reserved rank** — after the child's drain and projection — and allocates
nothing, so seats may land out of rank order.

**A read is served only inside the seated prefix.** A rank is served only when
every rank up to it has seated — to the consuming await and to the cursorless
read alike — and a run read serves from the asked rank to the last rank of that
prefix. A consumer therefore observes ranks in rank order and never observes a
child before it is rankable; a replay serves exactly the run its journal
recorded.

**Within a group, a child's intent drain is admitted in rank order.** A child
that declared intents admits its drain only once all lower committed siblings
have seated, or retirement releases the wait; a child never waits on a sibling
that has not committed. That is the order in which the group's children may emit
nested semantic commands, and therefore the order a replay must reproduce. The
barrier names every unseated committed sibling ranked below the child, and the
child waits at it with one `Drained` subscription at the index, whatever the
barrier's size, which the seat that lifts the barrier answers (§2); the
`drain_barrier_is_transitive` law pins it. A child whose final declared no
intent neither reads the barrier nor waits at its seat. The point retains the
final it committed, so a commit answered `AlreadyCommitted` carries the final an
earlier invocation committed: its declarations are known, and it waits at the
barrier only if they drain intents. Retirement is a release, not proof of
seating: the semantic-admission fence (§4) still refuses any intent under a
retired group. The closing wait (§7) reads the barrier past the last rank.

**Rank order, not source order, because source order cannot be carried into
groups.** A source-ordered drain gate makes every terminal leaf settle in source
order: `Promise.race([slow(), fast()])` could never resolve with `fast`, and one
hung source-first tool would stop every later sibling from ever becoming
rankable, so `race` and `any` would be accepted and useless.

**Source order is a determinism device, not a product law.** Intent realization
is journal-first and happens *after* the `ToolAttempt` is sealed, so on an
ordinal-addressed journal those commands land in the parent's journal and their
cross-sibling order must be replay-stable or the replay meets a mismatch. An
in-process completion order derived from a `FuturesUnordered` yield is not
replay-stable: a redrive re-races and can produce a different permutation.
ADR 0065 makes settlement order a durable fact, and the durable rank order is
replay-stable, so it is the order used.

Three consequences are stated rather than discovered:

- **Rank order equals commit order.** The rank is reserved at the commit, so the
  two agree by construction; only the *duration* of a drain varies, never the
  order. Cancel-decided children take a rank at their decision like any other
  terminal (ADR 0065), so rank covers more children than commit order does, and
  **rank order is decision order**: a cancel decided while a committed sibling
  still drains ranks after that sibling. For example, A commits and stalls in its
  drain (rank 1 reserved); the opener closes with `Cancel`, which decides B (rank
  2, seated at once); the group is reopened. B's cancellation is not observable
  until A seats — ranks 1 and 2 both read as not settled — and then the run is A,
  B.
- **`Promise.all` realizes intents in commit order**, not source order. This is
  observable — two children that each start a process register in commit order —
  and it is what ECMA-262 hosts do: side effects inside `a()` and `b()` happen as
  each settles, not in argument order.
- **The standalone Lashlang list-batch follows the same rule**, because it is the
  same batch path. Its *consumer* surface is §10 L7's; only the order in which its
  leaves' declarations are admitted is this section's.

**ADR 0042's "drains its declarations in source order" holds.** That clause is
about the declarations *of one attempt*, admitted in the order the provider
listed them. This section orders children, not one attempt's declarations.

#### Discharge is a recorded fact

**A child's place in rank order is durably discharged only after its required
intent outcomes are recorded, or after cancellation proves it has no remaining
protected admission obligation. Losing a task or a lease is not discharge.** A
discharge from `Drop` is right for a process-local future and **wrong** for
durable recovery, where a crash would release an earlier protected place before
its intents finish.

**A child with no remaining intent admission is admitted and discharged
immediately**: an attempt that declared nothing, and a timer or parked wait with
no remaining admission, take their place in rank order and release it at their
seat. An unseated one still holds the barrier of a later child that drains
intents.

**The committed final wins.** The §4 point retains the final a winning commit
offered: a tool child's sealed drain input — its record, its declared intents
and the attempt facts its settlement carries — a refusal, or the mark of an
outcome only the committing invocation holds (an atomic body's or a wait's).
Every later commit of the child is answered that final, and the invocation that
receives it seats it, never its own. A fallback's typed refusal — a
session-generation refusal or an expired attach (§8) — therefore seats only where
no final is committed. An invocation that cannot run the child and finds a final
committed by one that ended before its seat:

- drains a committed tool final from the retained drain input, at the barrier of
  the rank that commit reserved, and seats it: its declared intents are realized
  (W7) and the tool's attempts never run again (W15);
- seats a committed refusal as it was recorded;
- otherwise reports the committed final lost with the typed
  `RuntimeEffectGroupChildCommittedFinalLost` terminal, which names why: an
  outcome only the dead invocation held, or a tool final that a
  generation-refused invocation cannot drain under its session's state
  generation. A committed final's obligations are realized or reported by name,
  never replaced.

**A generation refusal over a committed tool final is a violated precondition,
not a routine loss.** Every invocation of a group child runs on the lane its
opener recorded (§8), and a lane's builds admit the same session-state
generations: the drain generation `G` hashes the build's session admission —
its supported range and every writer pin of the session-state surface
(`SessionAdmissionWindow`, `crates/lash-core-store/src/store/state_version.rs`;
`composed_generation`, `crates/lash/src/formats.rs`). The session's marker moves
under no production writer (ADR 0077). So a successor that drains a committed
final runs on a build that admits every session its opener's build admitted,
and drains it. `CommittedFinalLost` for a generation refusal remains only as
the typed backstop for forged or operator-forced state: a marker stamped by
hand, or a draining generation's deployment force-removed. A future marker
mover needs replay-safe exclusion of committed-undrained children, not only a
shift fence: an invocation whose marker moved between its journaled admission
and its commit would choose a different command sequence on replay.

A seat that drains nothing waits on no sibling.

**What is refused is an undefined barrier**, in particular any rule of the form
"drain every already-settled sibling", which is either circular or adds a barrier
behind an unrelated sibling.

A process-local gate — a `Mutex` plus a `Notify` — cannot be shared by two
Restate handlers and is lost by a crash past a checkpoint, so rank order and
discharge are durable facts in the group authority, not a new scheduler.

**Across the resumed continuation and running losers there is no total order, and
none is invented.** Existing target transaction order decides, and the
consumer-visible observation prefix is journaled (§6). **No turn-wide intent
scheduler.**

A deferred child's §4 point is its completion resolution; it releases its place
at discharge, after projection. The index reserves the rank in `commit_child`
(`reserve_rank`) and serves reads through `seated_prefix`
(`crates/lash-restate/src/effect_group/state_record.rs`). `commit_child` retains
the committed final beside the index record, one key per position, and the
dispatch's `settle_unrun_child` seats it
(`crates/lash-restate/src/effect_group/dispatch.rs`); the tool driver's
`drain_committed` drains a retained tool final. The
`a_committed_final_is_recovered_on_its_lane_across_a_deployment_change` and
`a_cancel_closed_groups_committed_final_is_recovered_on_its_lane` laws
(`crates/lash-conformance/src/conformance/tool_child_invocation/committed_recovery.rs`)
pin W7 across an expired attach and a deployment change.

---

### 6. Projection incorporation

**Incorporation is an opener-owned mapping from group identity to an incorporated
rank prefix, distinct from each aggregate's consumption cursor.** Ranks are
per group, and an opener may hold several groups, nested groups and already
consumed winners, so there is no opener-wide `k`.

**Before an externally effective continuation step, or a runtime observation that
reads these facts, record the chosen prefix mapping in that step's replay history,
then incorporate it.** Replay restores exactly that mapping before the step and
**must not add later-available settlements retroactively** — doing so can grant
process possession earlier than the original execution did and change
authorization. Checkpoints carry the mapping and the retained references needed to
rebuild it. Close incorporates all required remaining facts before accounting.
**Realization alone does not advance the opener's observation prefix**: recovery
after realization but before incorporation reconstructs the recorded child
outcomes and follows this protocol. This orders *observations*, not target writes,
and adds no global intent scheduler.

Possession is the authority this protects. A start declared as a tool intent is
realized in `tool_dispatch`, which holds no runtime execution context, so a
group child's settlement carries its started processes *in its outcome*
(`crates/lash-core-execution/src/runtime/effect/tool_settlement.rs`), and the
opener takes possession from the same realized outcome the bound value's
projection is taken from. `crates/lash-core-execution/src/session/execution_context.rs`
carries possession across a boundary through `restore_started_process_ids` and
`started_process_ids`.

Two consequences follow. **A Restate child mutating its own possession set
grants the parent nothing** — its settlement must carry the semantic facts. And
**already-realized starts and triggers are not erased by refusing a late
completion**: the refusal suppresses delivery of a result, never the world, and
ADR 0094 governs any process that really started.

**Losing values stay unreturned.** Incorporation concerns facts the runtime owns —
possession, trigger evidence, usage — never a value the model did not select.

**The prefix record precedes its application.** A consumer journals the prefix
it consumed as an `IncorporateGroupSettlements` record before applying it, and
an opener's phase contexts share one `IncorporationLedger`, so a loser's facts
incorporated at the opener's end never re-apply the winner's. Losers settle
while the opener lives and are incorporated at its end (§7); the ledger rides a
Lashlang segment handover, and a turn's ledger rides each of its journaled
checkpoints, so a turn resumed after its worker died — which serves its
completed cells from the journal and re-runs none of their incorporations —
restores what those cells incorporated and never incorporates a rank twice. A
handler that journals rank 1's record, crashes, and is redelivered after rank 2
settles incorporates rank 1 alone on replay.

---

### 7. Closing

**Before stopping admission or issuing any cancellation, durably transition the
exact opener from live to closing in the existing lifecycle/group authority, and
record the proposed terminal disposition there.** Recovery resumes closing
whenever that fact exists, even if no turn commit, process terminal or parent-end
row exists.

Without it recovery cannot tell closing from live. A crash after closing began and
before the terminal leaves neither of the other candidate facts — a
terminal outcome or an ADR 0094 parent-end ledger row — and ADR 0094 independently
documents a turn-commit-before-ledger-row window. Such an opener would be
classified live and would permit retries this section forbids.

**Every final turn exit and every process terminal path enters closing, including
failed and cancelled exits. Worker loss and segment handover do not.**

**Finalization is an ordered, idempotent sequence**, and a crash between steps
resumes the first incomplete step:

1. finish every protected obligation and incorporate all required usage (§13) and
   projection (§6);
2. commit the opener's outcome and accounting;
3. record parent end (ADR 0094);
4. complete retirement.

**Close seats cancel-decided children immediately without joining their attempt
bodies.** The drain barrier waits only for committed children that still owe
declarations or projection. Every protected committed obligation must finish
before finalization commits the opener's outcome and accounting, including
across recovery. Cancellation does not impose a deadline on those obligations.

The drain's queue is the group authority's own record of unsettled children,
not a second table: nothing is enqueued, and no synthetic queued-work item
exists for it.

**No fresh attempt retries after close** except what is necessary to recover a
committed obligation.

**Session deletion excludes an accepted or closing group.** Every refusal is
asked before anything is closed: an effect group that is live or closing pins
the session with `EffectGroupLifecyclePinned`
(`crates/lash-core/src/runtime/session_close.rs`), so accepted and closing groups
keep their storage and environment ownership until settlement (W16).

#### Unfinished tool execution at opener end

Losers are not assigned to the queued-work driver and do not run past opener
end on the opening scope's task set: unfinished tool execution at normal opener
end is closed by the opener (this section), and only that. Recovery ownership of
protected settlement stays, as do the drain, the disposition-at-open rule and
the work-driver seam. The owner has no implicit durable background tools; it
has the explicit process.

**Opener-close cancellation is a host lifetime contract.** Within a live opener,
letting losers run *is* Promise semantics. The divergence is at opener end, and
the honest Node comparison is a host that stays alive after an async function
returns — where a losing timer or socket is not cancelled and its later write can
land after the caller returned. Comparing against Node *process death* would hide
the difference. The statement concerns cancellation only; it claims no general
Node scheduling or lifetime equivalence, and opener close fences further
unprotected Lash semantic writes without guaranteeing that external I/O already
issued stops.

**The opener's end runs this sequence.** On Restate the group authority's
index is the closing record: `close` moves the group to `Closed` under the
recorded disposition in one exclusive step, and finalization resumes from what
the index recorded. The opener-side incorporation is one applicator call per
settled rank — `RuntimeExecutionContext::incorporate_tool_settlement` under
`SettlementSource::GroupRank { group_key, rank, child_replay_key }` — whose
`IncorporationLedger` makes a resumed re-run idempotent.
`RuntimeExecutionContext::close_opener_groups`
(`crates/lash-core-execution/src/session/opener_groups.rs`) is that end, run by
every final turn exit and every process terminal: it closes the groups the
opener still holds under `Cancel` — a group whose consumer was cancelled among
them, so a rank that lands after the cancel is still the opener's — closes the
live groups the journal holds under its scope whose keys this opener formed,
waits at each group's drain barrier until no committed child still owes its
drain, and then incorporates every group's settled ranks through the journaled
`IncorporateGroupSettlements` record. A group under the same scope that the
opener did not form is not the opener's to close or finish. A turn or process segment resumed after its worker died
recovers the accepted children of its live groups when it starts (W5),
republishing its content-addressed execution environment first so a recovered
child resolves the reference its request recorded.

---

### 8. Rank authority and consuming-bridge replay

**One rank authority per group.**

Restate's ordering guarantee is real and **invocation-local**: the protocol
requires that a replaying SDK observe the same relative notification order it
observed while processing. That is sufficient for a consumer replaying the same
selection schedule inside one invocation, and insufficient for a successor
segment, where attaching already-finished children yields new notifications in a
new order.

Therefore **SDK notification order may serve as rank only where same-invocation
replay proves the whole contract**, and **the existing group authority stays
wherever consumption, discharge or retained observation crosses invocations**.
Removing the group object requires proof of the cross-segment contract, not a
demonstration that same-invocation selection is deterministic.

**The consumed prefix and the results a successor still needs persist across
handover.** `crates/lash-core-execution/src/runtime/effect/group.rs` already
models the cursor as `EffectGroupHandle` with `group_key`, `children` and
`consumed`, and ADR 0065 makes the handle the sole cursor of record. What this
ADR adds is the *result* side: a successor must obtain the settlement payloads its
cursor still needs, not merely the count.

**Retaining the exact child invocation identity across handover is part of the
child record**, because Restate attach is by invocation id. **Expired attachment
is a typed recovery failure, never permission to rerun a side effect**, and it
never replaces a final the expired invocation committed: the successor seats that
final (§5). The Rust
SDK constructs only `AttachInvocationTarget::InvocationId`, so idempotency-key
attach is reachable from Rust only through the ingress client. Retention defaults
of 24 hours exist in the server configuration; a default is **not** an admitted
deployment guarantee, so the failure path is normative and the window is not.

Retained child ids, the typed expired-attachment failure
(`RuntimeEffectGroupChildAttachExpired`) and retained results are served from
the group authority; a Lashlang segment carries each outstanding group's key and
consumed cursor, and its successor reattaches through
`EffectGroupHandle::restored`.

**A committed child whose seat is owed is recovered by its group, on its
lane.** The group's own record is the obligation of record: the index retains
the committed final and the rank its commit reserved until the child seats. A
child's drain has one authority, the child's own driver, and nothing but the
child's invocation runs it — but when that invocation ends before its seat (an
operator's kill, a terminal protocol error) nothing else schedules the drain. So
the index re-sends every committed, unseated child whenever its opener reopens
the group, and whenever a dispatcher starts for a group already ready or closed
(`resend_owed_children`, `crates/lash-restate/src/effect_group/recovery.rs`).
The re-send is the dispatch's own child call again: the retained membership's
envelope, the recorded shape, the child's replay key as its idempotency key, on
the group's recorded lane. While the committing invocation is retained the key
attaches to it and nothing new runs; once its retention has expired the key
mints a successor with an empty journal. Restate may mint it under the very id
the index retains, so the index admits by the §4 point as well as by id: a live
admission of a committed child is always a successor's, since the invocation
that committed journaled its own admission first, and it is answered
`AttachExpired` whatever id it presents (`decide_group_child_admission`,
`crates/lash-restate/src/effect_group/state_record.rs`). The successor drains
the retained final through the child's driver and seats it at its reserved
rank; it never executes the child again. No timeout, lease or unseated rank alone
authorizes a drain.

**Every opener has a lane.** A group's dispatch route is a generation lane of
`EffectGroupDispatch`, whoever opened it: a lash handler's controller, a
controller a host builds inside its own handler, and an effect host all carry
their build's generation (`RestateRuntimeEffectController::new`,
`RestateEffectHost::new`), and the index refuses an open that declares any other
route. No group's children reach whichever build is newest through the stable
name. The dispatcher binds only its generation lane; no stable dispatcher
binding or stable dispatch route is retained. A generation's drain waits for
its lane's committed children: the
engine's retirement evidence counts the committed, unseated children of every
group on the lane (`DeploymentRegistry::undrained_group_children`), a host-built
opener's included, and `GenerationDrainStatus::drained` waits for none.

The retirement read uses a derived `EffectGroupDrainIndex` directory keyed by
`G` in each namespace (FIG-4522). A group registers durably before its final
commits; a registration failure is retryable and leaves the final uncommitted.
A poll reads that generation's directory and each listed group's authoritative
record. An absent, uncommitted or drained group contributes zero. Directory entries remain stored, including after an operator kill or a
child seats. Polls are admin-only and read-only; their cost is proportional to
all groups of the retiring generation, including drained history, and is
independent of other generations. No cleanup invocation or service timer adds
standing runtime work to a live group.

---

### 9. Segments, bounds and retirement

**Outstanding children are reattached across segments. Boundaries are never
deferred indefinitely.** "No segment boundary while tool children are unsettled"
fails ADR 0025, which requires segmentation by accumulated step cost and identical
results whichever schedule a process takes: a days-long process racing one quick
tool against one hung tool sits at width two forever and would never segment.
Declining a boundary at a *non-capturable* point stays correct —
`crates/lash-lashlang-runtime/src/process.rs` records that through
`record_segment_boundary_decline`, "lashlang segment boundary declined at
non-capturable point" — and declining because a child is unsettled does not.

**Bound retained work per exact logical opener**, including nested,
accepted-unclaimed, running, closing and **settled-but-still-required** children
and group metadata. Bounding only *outstanding* work bounds nothing: width-two
races whose losers finish promptly accumulate unlimited settled rows and retained
results while almost nothing is outstanding.

- **Count unique executions separately from operand positions** (§11 clause 1).
- **Reserve capacity atomically with acceptance; replay reuses the reservation.**
- **Release a reservation only when its recovery and consumer dependencies are
  discharged.**
- **Refusal is whole-open, before dispatch.** ADR 0065 already makes the
  whole-open refusal load-bearing for the no-executor case ("nothing of the group
  is journaled — the group row included"), because a half-admitted group whose
  operator retries is answered as a reopen, and a reopen passes the miss through
  by design.
- **Accepted work is never retroactively refused by a changed budget.**

**A completed group may retire as a whole while its opener remains live**, once no
replay or continuation needs it and an existing identity fence prevents
resurrection. Retiring the live opener's entire scope to retire one group is not
available. Retirement stays group-atomic (ADR 0065 N3).

**Backend command headroom is a per executing controller/segment bound, not one
days-long opener counter.** Admission includes a finite controller-specific upper
bound for parent-side dispatch, observation, cancellation, incorporation and
handover commands; the accounting units below bound them.

**No universal result-byte cap follows.** ADR 0025 says the segmentation guarantee
"does not claim that every effect result has a universal byte ceiling", and that a
universal byte rejection "would be a separate product contract". Intent payloads
keep their existing hard admission bounds. A `batch` wrapper takes at most
`BATCH_MEMBER_CEILING` (64) members, configurable downward
(`crates/lash-protocol-standard/src/lib.rs`), and the flattened group is
admitted whole against the session's `max_tool_calls`
([ADR 0116](0116-tools-are-opaque.md) §2.5).

A boundary is never declined for an unsettled child: the handover carries the
opener's outstanding group cursors, and with each the tool calls it counts
against the limit below. A group a process holds after an early decision stops
counting once every loser of it has settled: when a new group would pass the
limit, the process releases its oldest held groups whose last rank is recorded —
their ranks are incorporated, the groups are closed — and tries again. It never
waits for a call that is still running: a running call is held, and the limit
refuses. Settlement is durable and only moves forward, so a group released at
one point is settled on every replay of that point, and a replay releases the
same groups there. The recorded settlements are the identity fence a reopen is
served from.

**The limit and its unit (FIG-4546).** The session's `max_tool_calls` is
required host configuration with no default and no built-in ceiling: recorded at
creation, changed only by the core `set_max_tool_calls` config command, and read
from the record — a run's snapshot, a process's recorded environment — by every
replay, redrive and reopen. A changed limit therefore binds from the next run
and the next process start, and never refuses work already accepted. The unit
is the **unique tool invocation**; a timer is not a tool call and is not
counted, and operand positions are not host work (the position-to-child mapping
lives in the VM, §10 L4). What the limit counts depends on the execution:

- **A cell** — one code cell of a turn, or one step of a protocol without
  cells — counts every tool call it makes. The limit is its total; consuming a
  group gives nothing back. A redrive re-executes the cell and forms the same
  groups in the same order, so it refuses the same call.
- **A process** counts the tool calls it holds at once: accepted, running, or
  settled and still required, from its group's acceptance until the group is
  consumed to exhaustion and incorporated, released as above, or the process
  ends. It is not a total.

A group's calls are admitted before anything of the group is journaled,
announced or dispatched, an admission is reused when the same group is formed
again on replay or reattached across a segment, and a group that does not fit
is refused **whole** with `RuntimeErrorCode::MaxToolCallsExceeded` and its typed
cause, `ToolCallLimitExceeded` (scope, limit, calls counted, calls refused). The
refusal is the program's failure, never the host's, and is not retried: a
standard-protocol step's calls each answer it as a tool failure, a cell fails
as a `Program` failure carrying the typed cause, and a process fails with the
same typed failure. Its message names the limit, so the model reads it in its
feedback. Nothing is queued, paced, windowed or split: calls under the limit
run exactly as they did, and rate limits are the host's own.

The parent-side command units an aggregate costs its opener's journal are
per group: one open, one clock sample when it holds timers, at most two
`IncorporateGroupSettlements` records (at decision, and at release or the
opener's end) and one close; per child: one dispatch, one rank read per consumed or incorporated
rank, and at most one cancel decision; per handover: one carried cursor per
outstanding group. All are finite in the group's width, so an aggregate adds a
bounded number of commands between two segment-boundary checks and
**mid-aggregate VM suspension is not necessary**.

Every host reopens a closed group from its journal, so a reopen after close
serves the recorded settlements regardless of finalizer timing.

---

### 10. Aggregate laws

**L1 — the four-way consumer mode is independent of the three-way journaled wake
policy.** `GroupWakePolicy` has exactly three variants — `First`, `FirstSuccess`,
`All` — and ADR 0065 explains why `all` and `allSettled` share one: they ask the
host for the same thing and differ only in how far the caller consumes. The wake
policy is journaled identity folded into every child's envelope hash; the consumer
mode is a caller-side loop decision and is never journaled.

**L2 — the response algebra is total.**

- **`selected`** carries the first settlement for `race`, the first *successful*
  settlement for `any`, or the first *rejected* settlement for `all`.
- **`all-results`** carries all positions, for `allSettled` and for a successful
  `all`.
- **`exhausted-rejections`** carries `any`'s rejections in input-position order,
  including duplicate multiplicity.

**L3 — infrastructure failure and host cancellation propagate through a separate
host-control/error channel.** They never become tool rejections and never become
fabricated `allSettled` elements. A **retryable** infrastructure failure retains
accepted work; a **terminal** host failure enters opener closing. ADR 0065 already
takes this posture for routing: "A child with no runner is a routing fact, not an
outcome … so no terminal is ever synthesized from a miss."

**L4 — formation records the operand-position-to-unique-operation mapping.** One
child executes and ranks once; its outcome expands to every mapped position, with
ascending input position as the alias tie-break. **Unique-child exhaustion and
input-position completeness are distinct facts.** This keeps ADR 0065's refusal of
duplicate child replay keys intact: duplication is a consumer-side mapping above
unique children, never two children under one key.

**L5 — already-settled operands and preparation completions form a source-ordered
immediate prefix ahead of newly dispatched child settlements.** This is what Node
does with a plain value in the operand array, and it is what the existing batch
already does: `crates/lash-core-execution/src/session/tool_execution/batch.rs`
seeds `let mut settlement_order = settled_during_preparation;` before appending
dispatched settlements. **All pending siblings are still admitted before that
prefix can answer** (§11 clause 3). The prefix and the mapping survive replay.

**L6 — a loser's value is never synthesized.** No `undefined`, no
`{status:"cancelled"}` smuggled into an `allSettled` array, no placeholder for a
child that did not settle.

**L7 — a Lashlang-native aggregate reports its first written rejection.** Every
Lashlang-native aggregate, the standalone list-batch included, asks for every
result (`AllSettled` at the boundary) and reports its first *written* unwrapped
rejection. Only the TypeScript `Promise.*` aggregates carry an ECMA consumer
mode. ADR 0086's comprehension rules are untouched.

`AbilityOp::ResourceOperationBatch` carries the consumer mode and answers with `ResourceOperationBatchOutcome`'s
four arms — `AllResults`, `Selected`, `SettledValue`, `ExhaustedRejections`;
infrastructure failure and cancellation are the ability's `Err`, which the VM
raises as the uncatchable `AggregateHostControl` terminal — no guest `catch`
sees it — and a live controller error is also recorded as the enclosing
execution's nested effect error, so the cell aborts and is redriven rather than
committing an outcome its aggregate never answered. The VM deduplicates a handle
written twice into one leaf and expands its outcome to every position.

---

### 11. Value model

**One pending-operation handle for tools and timers.** ADR 0095 already made the
VM's single encoding `{__handle__: "lash", id}` with one mint/parse pair; no
second encoding is added for timers.

1. **Bound arrays and duplicates.** An operand may be a literal array, an
   array-valued expression or an array held in a binding, and the same pending
   operation may appear twice. **Execution deduplicates; input positions never
   do.** Formation records the position-to-unique-operation mapping (L4).
2. **Operands evaluate once, in source order**, before any settlement is consumed.
   ADR 0086 already fixes this for comprehensions.
3. **Every pending operation is admitted, even when a plain value decides the
   aggregate.** A `race` containing an already-resolved value still admits and
   dispatches its pending siblings before the immediate prefix can answer;
   skipping would make a side effect depend on an operand's arrival order.
4. **A timer's start point is its admission, and its fulfilment value is
   `undefined`.** **Admission records the timer's deadline once; replay and
   reattachment reuse that deadline, and duplicate positions share the same timer.
   Recovery never starts a fresh duration.**
5. **`Promise.race([])` never settles, faithfully.** ECMA-262 returns a
   forever-pending promise and there is no exception to catch. Because the dialect
   awaits aggregates in place, **no group is opened for zero operands** — ADR 0065
   already refuses empty groups, and nothing reaches that seam — and the host
   detects an await that nothing can resolve and **fails the cell with a typed
   host-level unsettled-await error**. That is the analogue of Node exiting with
   code 13 on an unsettled top-level await: the program's semantics are ECMA's,
   and the host's lifetime ends. It is **not** a catchable exception and **not** a
   registered ECMA deviation; it is a host lifetime contract, recorded in ADR 0062
   beside opener close. The error code is
   `RuntimeErrorCode::AggregateAwaitUnsettled` (`aggregate_await_unsettled`).
6. **`Promise.any([])` rejects with an `AggregateError` whose `errors` is
   empty**, as ECMA-262 specifies, and needs no group.
   `crates/lashlang/src/runtime/heap/validation.rs` refuses a non-aggregate error
   that "carries AggregateError errors".
7. **`Promise.all([])` and `Promise.allSettled([])` return `[]`.**
8. **`AggregateError.errors` is input-ordered**, not settlement-ordered. Settlement
   order decides *which* rejection an unwrapping aggregate reports (L2); it never
   reorders the collected errors.
9. **A raw process handle at an element position stays refused**, with the repair
   naming the tool: `crates/lashlang/src/runtime/vm/pending_tools.rs` carries
   `PROCESS_HANDLE_LEAF: &str = "a process handle cannot be awaited directly; call
   \`processes.await(handle)\` and await that call, so the durable wait settles
   with the rest of the batch"`.
10. **Async-map operands are aggregate operands.** The v1 async array driver runs
    callbacks **sequentially**, a registered deviation
    (`TS_ASYNC_MAP_SEQUENTIAL_V1`): result order matches Node, while callback
    interleaving and shared-mutation order can differ. This ADR does not change
    that driver. Its census row indexes the deviation; it is **not** executable
    evidence of callback semantics.

An unawaited `sleep(ms)`
mints a pending timer under the one handle encoding; its aggregate records the
deadline once from a journaled clock sample and admits a `Sleep { Until }`
child. A timer carries no identity of its own, so an aggregate that holds one
folds every timer's position and duration, and the instruction that formed it,
into its group identity: two timer aggregates at two sites are two groups. The clause 5 error code is `RuntimeErrorCode::AggregateAwaitUnsettled`
(`aggregate_await_unsettled`), raised by the VM as the uncatchable
`AggregateAwaitUnsettled` terminal.

---

### 12. Durable Wait as a child

**`processes.await(h)` is a resumable child on the existing Durable Wait protocol.**
A group child is an independently durable unit that need not settle inside one
resource operation, so the wait is a child of this kind — resumable, retained
across segments. Its attempt parks on the Durable Wait, and opener close
cancels and releases the wait without joining an attempt body.

**Selection never cancels the wait.** A winning timer in
`race([processes.await(job), sleep(10_000)])` leaves the losing wait **admitted
while the opener remains live**, and a live opener that crashes must recover it.
**Opener close cancels and releases that wait without requesting cancellation of
`job`.**

**The process's eventual lifetime is still ADR 0094's**, through its own Parent
Scope and Lifecycle Policy: surviving wait cancellation does not guarantee
surviving parent end.

**A process terminal and a cancellation of the wait are different facts.** A
process terminal — success, failure or process cancellation — travels as a payload
and is converted at the await site:
`crates/lash-restate/src/process/mod.rs` encodes every `ProcessAwaitOutput` through
`Resolution::Ok`, while `Resolution::Cancelled` is *wait* cancellation and becomes a
distinct `process_await_cancelled` failure. **Cancelling the wait is host
cancellation and never a fabricated process terminal.**

Routing is unchanged:
`crates/lash-core-execution/src/runtime/effect/executor/process_local.rs` keeps the
equivalence it documents, that "`await processes.await({ handle })` answers exactly
what `await handle`".

`processes.await` is admitted as a group
tool child whose attempt parks on the Durable Wait at once, so there is no
attempt body to join at close; selection leaves it admitted, and
the opener's close cancels and releases the wait without cancelling the
process.

---

### 13. Usage

**Cancellation refuses semantic results and new work; it does not discard known
usage attributable to the admitted child.** A cancelled attempt can have spent
provider tokens without an accepted success terminal, so **the child's retained
terminal or cancel disposition carries its captured usage facts, independently of
whether its value is returned**.

**Attribution binds the exact opener and tool invocation**, with each nested LLM
call identified by its `(LlmCallId, provider-attempt ordinal)` from
[ADR 0032](0032-attempt-history-rides-inside-the-result.md) — that pair is a
provider-transport attempt identity and is **not** by itself a tool driver's
lineage, which is why the opener and invocation are named beside it.

**Retained facts are incorporated idempotently before final accounting** (§7
step 1). **A late known fact stays attributed to its original opener**; it is
never silently dropped and never charged to a later turn. **No exception is
granted**: if immutable final accounting were ever intended to prohibit later
attribution, that would need its own ruling, and this ADR does not grant it.

**Facts lost before a durable observation remain unknown**, per ADR 0032, which
expressly accepts losing crash-mid-retry evidence from durable state. Unknown is a
value; zero is a false fact. ADR 0042 already concedes that an LLM call can be
billed again across an unrecorded completion, so this ADR does not claim to solve
provider billing exactly.

The ledger this attaches to exists:
`crates/lash-core/src/runtime/session_manager/direct_outcome.rs` records into a
shared `Arc` and deliberately does not commit — "Record into the shared token
ledger only … This usage is persisted exactly once by the final turn commit" and
"on effect-host replay this `apply` runs again with the cached outcome, and an
incremental persist would double-merge the usage". A remote child's ledger is not
the parent's ledger, so its usage travels as a semantic fact on its settlement.

**No generic durable trace bus.** Semantic usage rides the existing usage path;
live trace delivery stays best-effort.

Usage deltas are
charged once per `UsageDeltaIdentity`; a loser's usage is incorporated at its
opener's end, before the opener's accounting commits.

---

### 14. Every host journals

**No group state is held only in memory.** An accepted child, a settled group
record and a closing transition are durable facts in the group authority and
the journal ([ADR 0102](0102-zero-infra-is-a-sqlite-in-memory-backend.md) D1,
[ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md)),
so OS process death is worker loss (W18) and recovery reads the journal.

---

### Crash windows

Every window is a crash of the worker or handler running the opener, the child, or
both.

| # | window | required outcome |
|---|---|---|
| W1 | after the group open is journaled, before any child is dispatched | Reopen dispatches every child from the **retained accepted membership** (§3); admission is not re-decided, because the recorded group shape is the fence. Claims suppress duplicate authoritative completion; unrecorded opaque I/O stays at-least-once. |
| W2 | after dispatch, before the child's invocation id is published | Recovery reconstructs **that dispatch's original identity** and never issues a fresh unrelated call. |
| W3 | after dispatch and publication, before any settlement, opener live | Accepted children are recovered and continue. Nothing is cancelled and nothing is abandoned. |
| W4 | after a child settled and ranked, before the opener consumed it | The rank is durable; replay serves rank `consumed + 1` and yields the same settlement. |
| W5 | after the opener consumed the winner and checkpointed past the aggregate, losers in flight | Losers are recovered under the live opener. This is the window Endpoint B-prime would have abandoned. |
| W6 | after a loser's final attempt record committed, before its in-memory commit notification was published | The record is **protected** (§4). Recovery finishes the drain; the missing notification is not evidence of a lost commit. |
| W7 | after the final record committed, before its intents drained | Recovery finishes the drain and realizes the declared intents — the child's own redrive, or, once its invocation is gone, a successor the opener's reopen re-sends on the group's lane, draining the final the point retained (§5, §8). It holds across a deployment change and after a `Cancel` close. |
| W8 | after intents realized, before the opener incorporated them | Recovery reconstructs the recorded child outcomes and follows §6's observation protocol. Realization alone does not advance the prefix; no later settlement is added retroactively. |
| W9 | **closing recorded, before any cancel was issued** | Recovery resumes closing. No child is retried as though the opener were live, and the recorded terminal disposition is reused rather than re-decided. |
| W10 | **all drains complete, before the terminal/accounting commit** | Recovery resumes at finalization step 2 (§7). Obligations are not re-run and usage is not double-counted. |
| W11 | **terminal/accounting committed, before parent-end recording and retirement** | Recovery resumes at step 3, then step 4. Both are idempotent; ADR 0094's own commit-to-ledger window is the same shape. |
| W12 | closing recorded with a committed child still draining | Recovery finishes every protected committed obligation; **closing stays recorded** until they finish. Finalization waits at the drain barrier before committing the opener's outcome and accounting. |
| W13 | segment handover: continuation committed, successor not started | ADR 0025's three handover requirements apply unchanged, and outstanding children are reattached by the successor (§8). Handover does not enter closing. |
| W14 | child handler death with the opener alive | The child invocation is retried or reattached by invocation id. Abandonment is never inferred from a dead handler. A retry that finds no executor on a deployment not carrying the child now stays a retry. That includes a worker that does not hold the child's opener while another worker of the deployment does. A child no worker of its deployment can ever execute, by what it recorded and what the deployment wired, settles with the typed `RuntimeEffectGroupChildUnroutable` refusal naming the missing capability (ADR 0065), instead of retrying while its opener waits on it. |
| W15 | attach retention expired before the successor attached | Where no final is committed, the typed recovery failure. A committed final wins: the successor — answered `AttachExpired` whatever id it presents — drains and seats a committed tool final from its retained drain input, and reports any other it cannot realize lost by name (§5); a generation refusal reaches that report only through forged or operator-forced state. Never a re-execution of an opaque tool body, never a synthesized success. |
| W16 | session delete requested while the group is accepted or closing | Refused until settled (§7). |
| W17 | a late completion arrives after the cancel decision committed | Refused, typed, with **no journal write**; the refusal's evidence survives retirement. Already-admitted descendant commands are not undone (§4). |
| W18 | OS process death | No host holds group state only in memory (§14), so process death is worker loss: recovery reads the journal. |
| W19 | **final record committed and its rank reserved, crash before the drain runs or before the seat** | Recovery drains in the **recorded** rank order and seats the reserved rank; it never re-derives the order from whatever completes first on the redrive and never allocates a rank at the seat. The rank is a durable fact assigned at the §4 linearization point, not a property of the run that observed it. |
| W20 | **two finals commit concurrently, or a final and a cancel decision** | The linearization point serializes them, so exactly one takes the lower rank, and both ranks are durable before either drain begins. A tie is not resolvable by source index, by wall clock or by whichever writer returned first; if the point cannot order them it has not committed either. A cancel decided after a commit ranks after it and is observed only once that commit seats. |

---

### Worked examples

lash TypeScript dialect.

**1. Fan-out.** `await Promise.all([fetch_doc("a"), fetch_doc("b"), fetch_doc("c")])`
runs three concurrent children, and a redrive replays the same settlement order
from the ranks. No loser exists. `crates/lash-restate/src/tests/tool_batch_parallelism_on_the_double.rs` pins
the parallelism by rendezvous.

**2. Timeout, then keep working.**
`const r = await Promise.race([slow_search(q), sleep(5000)]); …; finish(answer);`
`slow_search` keeps running after the timer wins, and if it settles while the turn
still works, its completion lands in the turn. On Restate the completion is another
invocation's, so its semantic facts must travel back rather than be read out of a
shared address space (§6). The timer's deadline was recorded at admission and a
recovery reuses it (§11 clause 4).

**3. Race, then finish immediately.**
`finish(await Promise.race([search_a(q), search_b(q)]));` with `search_b` in flight
and uncommitted at `finish`. Closing records, then `search_b`'s cancel decision
commits and its body is signalled. **If cancellation wins arbitration, the child is
logically cancelled after the bounded local grace; physical I/O may continue, and
finalization still waits for every protected obligation and for accounting. No
successful-turn latency bound follows from the grace.**

**4. Side-effecting loser.**
`const sent = await Promise.race([send_email(msg), sleep(2000)]); finish(…);`
If `send_email` is uncommitted at close it is cancel-decided. If its final attempt
committed, its declarations are realized before the turn is settled and survive a
crash in that window (W7). The SMTP call may have gone out either way: ADR 0042
makes in-attempt effects at-least-once, so cancellation stops delivery of the
*result*, not the side effect. The honest message is the one the program wrote.

**5. Loser whose result is a process handle.**
`const h = await Promise.any([spawn_indexer("fast"), spawn_indexer("thorough")]);`
**An uncommitted losing spawn is cancelled at close. A losing spawn whose final
attempt committed still realizes its start and required projection before
finalization, even if it had not settled when close began.** The resulting process
follows ADR 0094 — it is not unmade by refusing a late completion — and if it
started while the opener was live, its possession reaches the parent replayably,
including on Restate (§6).

**6. Crash past the winner.** The opener consumed rank 1, checkpointed past the
aggregate, and the worker died with `search_b` mid-HTTP-call. **`search_b` is
recovered, because the opener is still live** (W5). A dead worker is not a closed
opener. **6b:** if `search_b`'s final attempt had already committed, its
declarations are realized (W7) — the case that rejected B-prime.

**7. Work that must outlive the turn.**
`const job = await processes.start({ definition: reindex, args: { scope } });
const r = await Promise.race([processes.await(job), sleep(10_000)]);`
The winning timer leaves the losing `processes.await(job)` **admitted while the
opener lives**; opener close cancels and releases the wait, and `job` keeps running
under ADR 0094 (§12).

**8. Restate mechanics.** The parent issues one idempotency-keyed `call` per child
and selects over the durable futures; the child handler runs the existing per-leaf
coordinator **at handler level**, with atomic attempts inside `ctx.run` and
coordination outside it (§2). The parent's own selection order serves as rank only
while the whole consumption stays inside that invocation (§8). No unjournaled
ingress long-poll is held open for a child's life, and implicit engine
cancellation is never the sole close protocol (§4).

## Alternatives rejected

**Endpoint A (reduced)** — durable host ownership of losers past opener end.
Rejected for unobserved post-turn side effects (examples 4 and 5), for the
late-completion delivery machinery it needs, and for the session-delete-versus-
running-loser protocol it adds.

**Endpoint B-prime** — abandon an accepted child on a crash past the winner.
Rejected because it destroys recorded declarations ADR 0042 protects, and because
it reads worker loss as opener close, which ADR 0094 forbids.

**"Never segment while a tool child is unsettled."** Rejected: it defeats ADR 0025
for a days-long width-two opener (§9).

**Bounding outstanding work only.** Rejected: it bounds nothing for an opener whose
losers finish promptly and whose settled state is still required (§9).

**Deleting group state at close.** Rejected: a committed-but-undrained child still
needs its rank, discharge and projection authority (§4).

## Consequences

- **Three facts, not one.** Child disposition, child rankability, and the
  opener's phase are separate; an implementation that derives any of them from
  another is wrong at a crash boundary.
- **`race` and `any` are useful, not merely accepted.** Rank order lets
  `Promise.race([slow(), fast()])` resolve with `fast`, and a hung sibling
  blocks no other sibling from ranking. The cost is one observable rule:
  `Promise.all` realizes intents in commit order, not source order, which is
  what an ECMA-262 host does anyway.
- **Closing is a durable fact with its own crash windows.** W9–W12 exist only
  because the transition is recorded; without it they are indistinguishable from a
  live opener.
- **A process opener is its minted id.** Group identity cannot alias across
  process registrations, because the id is never reused (ADR 0107).
- **Session deletion carries an obligation.** A live or closing group pins the
  session until it settles (§7, W16).
- **Cancellation is not a usage eraser.** Known usage survives a cancelled
  attempt, and unknown stays unknown under ADR 0032.
- **`Promise.race([])` ends the cell.** The program's semantics are ECMA's — the
  promise never settles — and the host reports a typed unsettled-await failure
  rather than parking forever.
- **The bound is a reservation protocol, not a number.** §9 names the
  accounting units and their release conditions, and mid-aggregate VM
  suspension is not necessary.

## Model usage accounting

Tool-child usage rides on no settlement and is not charged at
incorporation (§13). Each `ToolAttempt` entry's usage meter delivers the facts
of its nested calls through the accounting continuation of [ADR 0125](0125-model-usage-is-engine-owned-accounting-delivered-per-call.md), and a
nested call's unreported attempt is a fact. Settlements, captures and the
incorporation ledger carry no usage.
