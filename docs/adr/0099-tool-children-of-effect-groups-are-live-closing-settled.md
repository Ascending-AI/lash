# 0099: Tool children of effect groups have one lifecycle — live, closing, settled

## Status

Decided 2026-09-21 (FIG-3392). **Not yet implemented**: FIG-2266 builds the
invocation driver, FIG-3396 the accepted-work and protected-close recovery, and
FIG-3397 the integration landing. FIG-3395 authors the aggregate oracle that
freezes the pre-cutover baseline.

FIG-3396 is delivered as four ordered parts: FIG-3408 (§3, the retained child
request), FIG-3409 (§4/§5, the linearization point, commit order and
discharge), FIG-3410 (§7, closing and deletion exclusion) and FIG-3411 (§6, §8,
§12, §13, carriage and handover). §1's recovery-time incarnation validation
waits on FIG-3394. **Landed so far: FIG-3408's durable shape** — minted and
frozen, with no producer or consumer yet; see the §3 amendment.

Amends [ADR 0025](0025-bounded-journals-are-an-effect-controller-obligation.md),
[ADR 0042](0042-tool-attempts-are-atomic.md),
[ADR 0062](0062-the-typescript-dialect-is-an-exact-ecma-262-subset.md),
[ADR 0065](0065-concurrent-settlement-is-a-durable-group-at-the-effect-host-seam.md),
[ADR 0094](0094-child-lifecycle-is-a-registration-fact-settled-by-scope-end.md)
and [ADR 0095](0095-processes-are-values-and-process-controls-are-tools.md), each
of which carries the matching amendment and links here. Supersedes FIG-1416
ruling 3 for unfinished tool execution at normal opener end, and for nothing
else.

## Context

[ADR 0065](0065-concurrent-settlement-is-a-durable-group-at-the-effect-host-seam.md)
made concurrent settlement a durable group at the effect-host seam and left one
question open: what happens to a tool child that lost a race when its opener
ends or dies. That question was ruled under FIG-1538 after two endpoints were
rejected.

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

### What exists today, and what does not

This ADR builds on real primitives and specifies a lifecycle that does not yet
exist. The distinction matters section by section, so each clause is labelled
**holds today** or **new**, and this ADR never claims a target as current
behaviour.

The primitives that exist: the durable group record and its rank counter; the
host-registered `GroupExecutors` resolver; the loser drain over unsettled group
children; the in-process committed-final protection and source-ordered intent
drain gate; started-process possession and its segment handover; the shared
token ledger; the Durable Wait seam; the native host's scope-liveness fence.

What does not exist: any production caller of `open_effect_group`; a durable
child request; production recovery of accepted tool children; a durable
live→closing transition; deletion exclusion for a closing group;
`Promise.race`/`Promise.any` at all, which
`crates/lash-typescript/src/lower/calls.rs` still refuses with `"Unsupported:
Promise.{method} requires durable partial-settlement ordering (FIG-1416)."`

### Anchors

Anchors quote a path and source text, never a line number: a line pin rots on
the next edit above it and then points confidently at the wrong construct, while
a quoted phrase either still exists or visibly does not.

## Decision

### 0. The lifecycle

**live.** Aggregate selection cancels nothing; a losing child keeps running,
exactly as a losing promise does in ECMA-262. Worker loss does not end an
opener: an accepted child is *recovered* while its opener lives, from durable
input on the SQL and Restate tiers and from retained in-memory references on the
native tier.

**closing.** Entered by a durable transition (§7), not by a worker dying. New
tool work stops, cancel-eligible attempts become cancel-decided, and every
attempt whose final result already committed drains its declarations and records
its projection.

**settled.** Finalization has committed its outcome and accounting, parent end
is recorded, and retirement has completed.

Two latencies, and they are separate numbers: **winner latency** (when `race`
resumes — at the first settlement) and **finalization latency** (when the opener
reports success — after closing drains its protected obligations). No successful
turn latency bound follows from the cancel grace.

**Background work that must survive `finish` is a process the program named.**

---

### 1. Logical opener identity

**An opener is `Turn(session_id, turn_id)`, `QueueDrain(session_id, drain_id)`
or `Process(ProcessRef { process_id, incarnation })`.** Its identity is stable
across worker attempts and segments, and it changes on process re-registration.

**A queued-work drain is an opener, not a turn's container.** `drain_id` and
`turn_id` are two ways for a host to identify one physical unit — "keep
`drain_id(...)` as the durable idempotency key for retried drains, or keep
`turn_id(...)` as the host-minted physical turn identity"
(`crates/lash/src/turn.rs`) — and `execution_scope` there resolves to
`queue_drain_scope(session, drain_id)` when no turn id exists, so a queued
turn runs its whole effect tree, cells included, under
`ExecutionScope::QueueDrain`. A drain is durable and retry-stable for the same
reason a turn is. One drain may run several queued turns, and the drain's
opener lives until the drain ends rather than until its first turn does, so
group identity, retained authority, cancellation and retirement bind the drain
and not the turn inside it. Found by FIG-3394 when a cell of a queued turn was
refused for naming no opener.

**`SessionDelete` and `RuntimeOperation` scopes are not openers and run no
cells.** A scope that is none of the three is refused with a typed error rather
than given an invented identity: widening this set is an amendment to this
section, which is how the drain arm arrived. Group identity, retained
authority, cancellation, usage attribution and retirement all bind that exact
opener. A retired or mismatched incarnation is refused, never rebound to the
current process carrying the same name.

**Today's execution scope does not establish that identity.**
`crates/lash-sansio/src/effect_identity.rs` defines `ExecutionScope::Process {
process_id }` with no incarnation, while
`crates/lash-core-store/src/process_identity.rs` defines `ProcessRef {
process_id, incarnation }` precisely to "Pin a reusable process name to one
store-minted incarnation", and ADR 0094 renders a process Parent Scope as
`process_id#incarnation`. A group key's documented shape —
`{scope_id}:group:{batch_id}:{occurrence}` in
`crates/lash-core-execution/src/runtime/effect/group.rs` and
`.../group_journal.rs` — is a string shape, not an incarnation binding. Reusing
a process name can therefore alias a prior close, cancellation fence or group.
**FIG-3394 binds the incarnation into the shared group and child identity;
FIG-3396 validates it during recovery.**

**A process-backed session turn runs its cells under the process opener.** A
`ProcessInput::SessionTurn` row — what every `agents.spawn` child is — creates a
child session and runs one turn of it under the *process's* scope:
`SessionTurnRequest::new_process_backed`
(`crates/lash-core-execution/src/plugin/runtime_host.rs`) refuses any other
scope, and the managed turn that rescopes a turn scope passes a process scope
through untouched (`crates/lash-core/src/runtime/session_manager/turns.rs`). So
that child turn's cells are opened by the process and not by the child turn: a
worker retry keeps the incarnation and reuses the journal, while a
re-registration under the same name is a different opener. The incarnation
reaches the cell because the process runner binds it onto the admitted
controller — `ScopedEffectController::with_admitted_process`, from the record
the authority CAS returned — and a process-scoped execution that carries no
admitted incarnation is refused rather than opened on the reusable name. Found
by FIG-3394, when refusing a process scope outright took every subagent cell's
first tool call out: the child's `task.fail(...)` came back as "has no logical
opener", its driver re-asked the provider to the cap, and the parent read
`Stopped(MaxTurns)` instead of the child's own reason.

**A dead worker is neither live-ended nor closed.** Recovery classifies an
opener by the durable closing fact of §7, never by the liveness of a lease. The
existing drain guard is explicitly local and cannot answer this:
`crates/lash-core-execution/src/runtime/effect/group_drain.rs` says "A group this
process is still working is refused outright", and then immediately limits it —
"These guards see only what this process can see. A group open in *another*
process is not distinguishable from a closed one here, which is why closedness is
the caller's knowledge".

**Occurrence carriage.** The occurrence ordinal rides the VM continuation, so two
identical `race` calls straddling a segment boundary do not both derive
occurrence 0. It lives in `crates/lashlang/src/runtime/vm/continuation.rs` as
`VmContinuation::occurrence_counters`, embedded in the `LashlangSegmentState` the
Lashlang runtime serializes at a boundary; the sibling `ReplayOrdinalsState`
carries the separate sleep, event and signal ordinals and is not where aggregate
occurrences live.

*Status.* The group-key shape and the continuation carriage **hold today**; **no
production path mints an occurrence**, which is FIG-3394's. The opener identity
above and its recovery-time validation are **new**.

---

### 2. Invocation driver

**A tool child is a replayable invocation driver.** Retry, completion-key
derivation, deferred await and the orchestrating lane are *coordination* and run
at handler level on the child's own admitted controller. Only atomic attempts run
inside recorded bodies.

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
`crates/lash-core-execution/src/runtime/effect/group_drain.rs` resolves through
`fn executor_for(&self, envelope: &RuntimeEffectEnvelope)`. **No caller-closure
route is reintroduced**: a caller vector answers only first dispatch, while
retry, drain, a resuming process and a fresh handler execution all run with no
caller in scope.

**On Restate, children become `call` children of the parent with the child's
replay key as the idempotency key.** They are not today: every group child is
dispatched one-way —
`crates/lash-restate/src/effect_group/dispatch.rs` builds each as
`.child(Json(EffectGroupChildRequest { … })).send()` — and the pinned VM's
implicit cancellation covers tracked `call` children while deliberately exempting
one-way sends. **Implicit cancellation therefore covers zero group children
today**, and it never becomes the sole close protocol once it does cover them
(§4).

*Status.* The resolver seam and the recorded-body rule **hold today**. The
handler-level driver, `call`-child dispatch and idempotency-keyed identity are
**new** (FIG-2266).

---

### 3. Execution authority and accepted work

**The durable child request reuses `ProcessExecutionEnvRef` and existing artifact
ownership, and adds only what environment capture lacks.** Capture already
publishes under an owner —
`crates/lash-core-execution/src/session/execution_context.rs` exposes
`captured_process_execution_env_ref(&self, owner: &crate::ArtifactOwner)` — and
what it captures is two fields:
`crates/lash-core-store/src/process_identity.rs` defines
`ProcessExecutionEnvSpec { plugin_options, policy }`.

Two fields are not a tool execution descriptor. The child request adds the
admitted grant (kept separate today:
`crates/lash-core-execution/src/tool_provider.rs` carries `pub struct
PreparedToolBatchCall { pub call, pub replay_suffix, pub execution_grant:
Option<Box<ToolExecutionGrant>> }`, and the grant holds `source_id` and
`execution_binding`), the admitted scope, lineage, and cancellation authority.

**Before acknowledging open or dispatching any child, retain the complete
accepted membership and a reconstructible request for every unique child,
including unclaimed children**: input, replay identity, admitted grant, the exact
opener and scope, lineage, cancellation authority and environment reference. A
persisted accepted group may never exist without discoverable complete input.
This is a real gap, not a restatement: `EffectGroupRecord` in
`crates/lash-core-execution/src/runtime/effect/group_journal.rs` stores "How many
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

#### Amendment (FIG-3408): what a reconstructible request contains

The clause above names the facts a child request carries. Building the shape
forced four decisions that the clause did not settle, recorded here so a later
reader does not re-derive them differently. **Status: the shape is minted and
frozen; nothing produces or consumes it yet** — the handler-level driver is
FIG-2266's and group formation is FIG-3397's.

**A tool child needs a command of its own.** ADR 0065 recorded that "groups
introduce no new command variant, because what is new is the *composition
above* attempts". That holds for every child the journal could already name and
fails for a tool child. `crates/lash-core-execution/src/runtime/effect/envelope.rs`
carries `ToolAttempt { call, execution_grant, attempt, max_attempts }` — the
atomic body of *one* attempt, so a driver expressed as one could not retry,
because a second attempt is a second envelope with a second hash — and
`ToolBatch { batch }`, the whole batch a group replaces. §2's invocation driver
is neither. The retained request is therefore the payload of a new
`ToolInvocation` command rather than a second record beside an existing one:
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

**1b. The opener is a typed identity, not a scope.** §1's opener is
`Turn(session_id, turn_id)` or `Process(ProcessRef { process_id, incarnation })`,
and `ExecutionScope::Process` carries only `process_id`, so a retained scope
leaves recovery-time validation nothing to validate and lets a re-registered
name alias its predecessor's groups and fences. `EffectOpener`
(`crates/lash-core-store/src/effect_opener.rs`) is that identity, shared by this
request, by recovery, and by FIG-3394's Lashlang host bridges. It is an enum
with a `kind` tag rather than a rendered string because a turn's scope identity
is free-form text that can spell `{process_id}#{incarnation}` exactly; untagged,
the two openers would mint one identity. The child's *claim address* stays an
`ExecutionScope`, which is what the journal fences a row on. An enclosing
process is likewise a `ProcessRef`, never a bare name.

**2. Completion routing is recorded, not re-derived.** Completion-key
preparation answers `Issued | NotNeeded | Unsupported` from two live inputs —
whether the tool may defer, which consults the live registry or provider, and
whether the host routes completions durably. Both are deployment facts at
recovery time and admission facts at formation time. The request records which
of `inline`, `durable` or `process-lifetime` the child was admitted under, so a
recovered child never derives a key nothing will resolve; a process-lifetime
child recovered in another process is a typed refusal, never a fresh key (§14).

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

**4. Turn context is never recorded, and the fence already exists.**
`TurnContext` holds a live `LiveTurnInputs` and an optional live
`ProviderHandle` and has no `Serialize`. It is not carried. On the tiers where a
group child is recovered this costs nothing, because live plugin inputs are
already refused at turn admission: `ensure_durable_effect_input`
(`crates/lash-core/src/runtime/turn_loop.rs`) rejects them with
`DurableEffectLivePluginInput` on the durable admission paths
(`runtime/session_api.rs`, `runtime/turn_loop/prepare.rs`), and process runners
construct their tool dispatch with `TurnContext::default()`. The sole
tool-facing reader is `ToolContext::plugin_input`. So the request records no
turn-context payload and needs no new refusal: this is a restatement of an
existing fence, not a behaviour change.

*Status.* Capture, ownership and the separate grant **hold today**. Retained
accepted membership, the reconstructible request and environment protection
through the last dependency are **new** (FIG-3396). The request shape and the
`ToolInvocation` command are **minted but unproduced** (FIG-3408).

---

### 4. Commit versus cancel arbitration

**Cancellation requests are not cancellation decisions.** The final-attempt
record and the cancel disposition compete at **one durable, fenced linearization
point**, and exactly one may commit. A winning final record retains settlement
ownership. A winning cancel disposition refuses any subsequent attempt-final
record. Signalling the body and the bounded local grace *follow* the cancel
decision and cannot reverse it. **A final record found after recovery is
protected even if its in-memory commit notification was never published.**

The in-process machinery today is local, not durable, and the difference is the
work. `crates/lash-core-execution/src/session/tool_execution/batch.rs`
short-circuits its own grace with `if final_result_committed.is_committed() {
return tool_call.await; }` before arming `Duration::from_millis(50)`, and
`crates/lash-core-execution/src/tool_dispatch/attempt_coordinator.rs` publishes
that signal where the terminal is sealed — `begin_final_drain` "Publishes this
child's committed final result and waits for its turn to drain the declared
intents". That is a `tokio::sync::watch` send followed by a gate wait: correct in
one process, and not an arbitration primitive across a crash.

#### Transition table

One child. **Cancel-decided** means the cancel disposition won the linearization
point. Child disposition, rankability, and the opener's live/closing/settled
phase are three separate facts; nothing below waits on opener close.

| state | cancel decision commits | final record commits | drain completes | grace expires | worker loss |
|---|---|---|---|---|---|
| **accepted, unclaimed** | → **cancel-decided** | n/a | n/a | n/a | → *accepted* (recovered while the opener lives, §1) |
| **executing, uncommitted** | → **cancel-decided**; body signalled, grace armed | → **committed** | n/a | n/a | → *executing* (resumed from the retained request) |
| **committed** (final recorded, intents not drained) | **refused** — the point is already taken | n/a | → **drained** | not armed | → **committed**; recovery finishes the drain |
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
original opener. This matters because an orchestrating child has no attempt frame
of its own — ADR 0042 says `batch` and `spawn_agent` "have no `ToolAttempt` frame
of their own", and "every journal command it issues is a direct child of the
enclosing process invocation" — so an *uncommitted* orchestrator can already
contain committed descendants and recorded semantic commands. **Orchestrating
children are classified by their retained command and child obligations, never by
an invented outer `ToolAttempt`.**

Subject to that, the fence covers four sinks: child dispatch, nested semantic
writes (triggers, process registration, the live possession and usage buffers for
*new* admission), completion delivery, and rank insertion. A refusal that guards
only rank insertion arrives after the escaped coordinator's writes have landed.

**Known usage is never fenced out.** See §13: cancellation refuses semantic
results and new work; it does not discard usage already attributable to the
admitted child.

**Neither close nor cancellation acknowledgement permits deletion.** Retirement
requires discharged obligations, no retained consumer dependency, and a surviving
identity fence. The quiescence read is the shape of the right question —
`crates/lash-store-sql/src/effect.rs` defines `scope_is_quiescent` over an
`in_progress` replay row, a group whose `grp.children > (SELECT COUNT(*) …)`, and
an `await_event_waits` row with `terminal_json IS NULL`, in one statement because
"quiescence is the one question whose answer spans both families" — but it is not
today wired to session deletion (§7).

**Restate's engine cancellation is never the sole close protocol.** The pinned
shared core cancels tracked child calls from inside `do_await` without consulting
any Lash commit fence, so switching `.send()` to `.call()` does not protect a
committed final by itself. The journaled cooperative path and retained
protected-work recovery must preserve this section's rules even when implicit
cancellation interrupts a child invocation.

*Status.* The local committed-final protection and the 50 ms grace **hold today**.
The durable linearization point, the cancel disposition, the descendant rule and
the deletion exclusion are **new** (FIG-3396).

---

### 5. Rankability and intent order

**A child becomes rankable only after its own authoritative final intent outcomes
and its required projection.** Publishing a rank earlier would let a consumer
observe a settlement whose declared effects have not happened, and then checkpoint
past it.

#### Intents are admitted in final-commit order, not in source order

**Within a group, a child's intent drain is admitted in the order its final
record won the §4 linearization point.** That order is durable, monotonic per
group, assigned at the moment a final record commits, and journaled; a child never
waits on an unfinished sibling. It is the order in which the group's children may
emit nested semantic commands, and therefore the order a replay must reproduce.

This replaces today's cross-child **source** order, which cannot be carried into
groups. `settle_terminal_attempt` in
`crates/lash-core-execution/src/tool_dispatch/attempt_coordinator.rs` calls
`slot.begin_final_drain().await` for **every** terminal attempt, whether or not it
declared a single intent, and `BatchIntentDrainGate::wait_for` parks until `next
== index`. So every terminal leaf of a batch today settles in source order, and
first-settled selection is reachable only when the source-first child *parks*
(a deferred completion) or fails during preparation — which is exactly what the
FIG-3395 oracle measured. Carried into groups unchanged, `Promise.race([slow(),
fast()])` could never resolve with `fast`, and one hung source-first tool would
stop every later sibling from ever becoming rankable. `race` and `any` would be
accepted and useless.

**Source order was a determinism device, not a product law.** Intent realization
is journal-first and happens *after* the `ToolAttempt` is sealed, so on an
ordinal-addressed tier those commands land in the parent's journal and their
cross-sibling order must be replay-stable or the replay meets a mismatch. Before
groups the only replay-stable order available was source order: completion order
was "derived from a `FuturesUnordered` yield order in process" and journaled only
when the whole batch record sealed (ADR 0065's Context), so a redrive re-raced and
could produce a different permutation. ADR 0065 exists to make settlement order a
durable fact, and once the final-commit order is itself durable it is an available
deterministic order — so the device is no longer needed and its cost is no longer
paid.

Three consequences are stated rather than discovered:

- **Rank order equals commit order.** Drains proceed in commit order and rank is
  allocated after a child's drain, so the two agree; only the *duration* of a
  drain varies, never the order. Cancel-decided children still route through for
  rank like any other terminal (ADR 0065), so rank covers more children than
  commit order does.
- **`Promise.all`'s intent realization order changes** from source order to
  completion order. This is observable — two children that each start a process
  now register in completion order — and it is what ECMA-262 hosts do: side
  effects inside `a()` and `b()` happen as each settles, not in argument order.
- **The standalone Lashlang list-batch follows the same rule**, because it is the
  same batch path. Its *consumer* surface is unchanged: it keeps its existing
  all-results wait and its first-settled rejection selection (§10 L7). Only the
  order in which its leaves' declarations are admitted moves.

**ADR 0042's "drains its declarations in source order" is untouched.** That clause
is about the declarations *of one attempt*, admitted in the order the provider
listed them, and it stays exactly as it is. The cross-child order this section
changes is a batch-level mechanism that no ADR ever recorded.

#### Discharge is a recorded fact

**A group's source slot is durably discharged only after its required intent
outcomes are recorded, or after cancellation proves it has no remaining protected
admission obligation. Losing a task or a lease is not discharge.** The in-process
gate discharges from `Drop` — `IntentDrainGuard` is "One child's exactly-once
claim on its slot … Holding the guard is the claim; dropping it discharges the
slot" — which is right for a process-local future and **wrong** if copied into
durable recovery, where a crash would release an earlier protected slot before its
intents finish.

**A child with no remaining intent admission is admitted and discharged
immediately**, without ever blocking a sibling: an attempt that declared nothing,
and a timer or parked wait with no remaining admission, take their place in commit
order and release it in the same step.

**What is refused is an undefined barrier**, in particular any rule of the form
"drain every already-settled sibling", which is either circular or adds a barrier
behind an unrelated sibling.

The existing gate is a `Mutex` plus a `Notify`: two Restate handlers cannot share
it and a crash past a checkpoint loses it, so commit order and discharge must be
representable durably — as facts in the existing group authority, not as a new
scheduler.

**Across the resumed continuation and running losers there is no total order, and
none is invented.** Existing target transaction order decides, and the
consumer-visible observation prefix is journaled (§6). **No turn-wide intent
scheduler.**

*Status.* The per-group gate and its in-process discharge **hold today**, in
source order. Commit order, durable discharge and the journaled observation prefix
are **new** (FIG-3396, FIG-3397).

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

Possession is the authority this protects.
`crates/lash-core-execution/src/session/process_handles.rs` explains what its
absence cost: "A start declared as a tool intent is realized in `tool_dispatch`,
which holds no runtime execution context, so nothing recorded it and the child was
unreachable to the very run that started it — `await handle` refused with
`ProcessNotVisible`". `record_processes_started_by_intents` takes possession "from
the same realized outcome the bound value's projection is taken from", and
`crates/lash-core-execution/src/session/execution_context.rs` carries it across a
boundary through `restore_started_process_ids` and `started_process_ids`.

Two consequences the arc adds. **A Restate child mutating its own possession set
grants the parent nothing** — its settlement must carry the semantic facts. And
**already-realized starts and triggers are not erased by refusing a late
completion**: the refusal suppresses delivery of a result, never the world, and
ADR 0094 governs any process that really started.

**Losing values stay unreturned.** Incorporation concerns facts the runtime owns —
possession, trigger evidence, usage — never a value the model did not select.

*Status.* Possession recording and its segment handover **hold today**. The
opener-owned mapping, its replay-history recording and cross-invocation carriage
are **new** (FIG-3396, FIG-3397).

---

### 7. Closing

**Before stopping admission or issuing any cancellation, durably transition the
exact opener from live to closing in the existing lifecycle/group authority, and
record the proposed terminal disposition there.** Recovery resumes closing
whenever that fact exists, even if no turn commit, process terminal or parent-end
row exists.

Without it recovery cannot tell closing from live. A crash after closing began and
before the terminal leaves neither of the facts an earlier draft proposed — a
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

**A close deadline is an attempt-local drain budget**, supplied as
controller construction-time input beside the segment effect budget — the shape
`crates/lash-restate/src/controller/mod.rs` already uses for
`segment_effect_budget`, whose default is `10_000`. It **starts when that
attempt's cancel decision commits**, not when closing began and not when the
group opened, so a slow sibling cannot consume another child's budget. On expiry
the attempt is logically cancelled and **closing stays recorded and discoverable
by the existing work driver**; finalization does not commit an ordinary terminal
that would fence out the remaining obligations. **Changing the budget never
changes committed obligations**: a redrive under a different budget still owes
every declaration the first attempt recorded.

The drain's queue is the journal, not a second table:
`crates/lash-core-execution/src/runtime/effect/group_drain.rs` states "Nothing is
enqueued. The drain's work list is
`EffectReplayRowStore::read_unsettled_group_children` … No synthetic queued-work
item, no `work_kind`, and no table exists for this."

**No fresh attempt retries after close** except what is necessary to recover a
committed obligation.

**Session deletion must exclude an accepted or closing group, and that exclusion
does not exist.** Both SQL session-retirement paths delete effect-group rows —
`crates/lash-store-sql/src/effect/group.rs` carries `delete_by_session = "DELETE
FROM runtime_effect_group WHERE session_id = ?1"` — and the refusal that does
exist, in `crates/lash-sqlite-store/src/session_deletion.rs`, counts
`turn_cancel_closure_authorizations`, which is a turn-cancel closure obligation
and not a group-closing one. **FIG-3396 must extend the existing
deletion/lifecycle authority so accepted and closing groups retain their required
storage and environment ownership until settlement.** Current session retirement
is not evidence of that protection.

#### What this supersedes, exactly

FIG-1416 ruling 3 assigned losers to the queued-work driver under both
dispositions and rejected leaving them on the opening scope's task set. **That
ruling is superseded for unfinished tool execution at normal opener end, and for
nothing else.** Recovery ownership of protected settlement stays, as do the drain,
the disposition-at-open rule and the work-driver seam. The owner gives up implicit
durable background tools and keeps the explicit process.

**Opener-close cancellation is a host lifetime contract.** Within a live opener,
letting losers run *is* Promise semantics. The divergence is at opener end, and
the honest Node comparison is a host that stays alive after an async function
returns — where a losing timer or socket is not cancelled and its later write can
land after the caller returned. Comparing against Node *process death* would hide
the difference. The statement concerns cancellation only; it claims no general
Node scheduling or lifetime equivalence, and opener close fences further
unprotected Lash semantic writes without guaranteeing that external I/O already
issued stops.

*Status.* The drain, disposition-at-open and work-driver seam **hold today**. The
durable closing transition, the finalization sequence, the drain budget and the
deletion exclusion are **new** (FIG-3396, FIG-3397).

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
is a typed recovery failure, never permission to rerun a side effect.** The Rust
SDK constructs only `AttachInvocationTarget::InvocationId`, so idempotency-key
attach is reachable from Rust only through the ingress client. Retention defaults
of 24 hours exist in the server configuration; a default is **not** an admitted
deployment guarantee, so the failure path is normative and the window is not.

*Status.* The group object, cursor and shape fence **hold today**. Retained ids,
retained results across handover and the typed expired-attachment failure are
**new** (FIG-3396, FIG-3397).

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
handover commands. **FIG-3397 names those accounting units and their release
conditions before deciding whether mid-aggregate VM suspension is necessary**; it
is not assumed here.

**No universal result-byte cap follows.** ADR 0025 says the segmentation guarantee
"does not claim that every effect result has a universal byte ceiling", and that a
universal byte rejection "would be a separate product contract". Intent payloads
keep their existing hard admission bounds. The only width ceiling today is
protocol-side: `crates/lash-protocol-standard/src/lib.rs` has `const
BATCH_MAX_TOOL_CALLS: usize = 25`, and neither it nor the segment budget specifies
retained-work admission.

*Status.* Segment boundaries, the decline path and the effect budget **hold
today**. Reattachment, the two-dimensional bound with its reservation contract,
and group-level retirement under a live opener are **new** (FIG-3397).

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

**L7 — the standalone Lashlang list-batch await is unchanged.** It retains its
existing all-results wait and its first-settled rejection selection; ADR 0086's
comprehension rules are untouched. Changing that surface requires its own ruling.

*Status.* The three-way wake policy, the source-order projection and the
preparation prefix **hold today**. The total response algebra, the classification
split and the duplicate mapping are **new** (FIG-3397); FIG-3395 pins the current
baseline first.

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
   beside opener close. FIG-3397 names the error code.
6. **`Promise.any([])` rejects with an `AggregateError` whose `errors` is
   empty**, as ECMA-262 specifies, and needs no group.
   `crates/lashlang/src/runtime/heap/validation.rs` already refuses an
   "AggregateError object … missing its errors list" and a non-aggregate error that
   "carries AggregateError errors".
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

*Status.* One handle kind, the raw-handle refusal, operand evaluation order and
`AggregateError` shape validation **hold today**; `race`/`any` themselves do not.
Clauses 1–8 land in FIG-3397.

---

### 12. Durable Wait as a child

**`processes.await(h)` is a resumable child on the existing Durable Wait protocol.**
ADR 0095 said it "is never a batch child", written against the *atomic* batch whose
children had to settle inside one resource operation. A group child is an
independently durable unit with no such bound, so the sentence changes meaning: the
wait is a child of this kind — resumable, retained across segments, not subject to
the cancel grace a running attempt is, because there is no attempt body to
interrupt.

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

*Status.* The Durable Wait routing, the terminal conversion and the released-wait
semantics **hold today**. Admission as a group child of this kind is **new**
(FIG-3397).

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

*Status.* The shared ledger and its single commit point **hold today**.
Cross-boundary attribution, deduplication on attach or redrive, and
cancellation-surviving usage are **new** (FIG-2266, FIG-3397).

---

### 14. Native tier

**One semantic path on every tier, with substrate-specific storage.** Native is not
a second tool implementation; it is this lifecycle with references in memory
instead of rows on disk, and it obeys §4's arbitration, §7's closing sequence and
§13's usage rules unchanged.

**An owned opener supervisor polls losers while the VM or provider is elsewhere.**
A scoped vector polled only while the aggregate is awaited is insufficient: the
opener resumes after the winner and then spends time in the VM and in provider
calls, during which a loser must still progress, settle and have its facts
incorporated. The supervisor is owned by the opener and lives as long as it does.

**Contexts are retained through protected drain**, not until the aggregate returns:
a native loser whose final attempt committed still needs its execution context to
drain its intents.

**Unclaimed tasks are counted.** The host already keeps and fences live counts:
`crates/lash-core-execution/src/runtime/effect/native_host.rs` describes
`ScopeLiveness` as "the in-process twin of a journal's `in_progress` rows and open
group rows, which a quiescent-gated retirement must not cut under", with an
`admission` mutex that orders "check the fence, then count as live" against "prove
nothing is live, then fence".

**No disk manifest, and no promise past OS-process death.** Native durability ends
at the runtime's lifetime, as the host already says of externally routed completion
keys through `allow_process_lifetime_completion_keys` — "Explicitly accept that
externally routed completion keys die with this process."

*Status.* Scope liveness, its fence and the completion-key opt-in **hold today**.
The owned supervisor and context retention through drain are **new** (FIG-2266).

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
| W7 | after the final record committed, before its intents drained | Recovery finishes the drain and realizes the declared intents. |
| W8 | after intents realized, before the opener incorporated them | Recovery reconstructs the recorded child outcomes and follows §6's observation protocol. Realization alone does not advance the prefix; no later settlement is added retroactively. |
| W9 | **closing recorded, before any cancel was issued** | Recovery resumes closing. No child is retried as though the opener were live, and the recorded terminal disposition is reused rather than re-decided. |
| W10 | **all drains complete, before the terminal/accounting commit** | Recovery resumes at finalization step 2 (§7). Obligations are not re-run and usage is not double-counted. |
| W11 | **terminal/accounting committed, before parent-end recording and retirement** | Recovery resumes at step 3, then step 4. Both are idempotent; ADR 0094's own commit-to-ledger window is the same shape. |
| W12 | close deadline expires with an attempt still draining | The attempt is logically cancelled; **closing stays recorded** and the work driver rediscovers it. No ordinary terminal is committed that would fence out remaining obligations. |
| W13 | segment handover: continuation committed, successor not started | ADR 0025's three handover requirements apply unchanged, and outstanding children are reattached by the successor (§8). Handover does not enter closing. |
| W14 | child handler death with the opener alive | The child invocation is retried or reattached by invocation id. Abandonment is never inferred from a dead handler. |
| W15 | attach retention expired before the successor attached | A typed recovery failure. Never a re-execution of an opaque tool body, never a synthesized terminal. |
| W16 | session delete requested while the group is accepted or closing | Refused until settled. **This exclusion does not exist today** (§7) and is FIG-3396's. |
| W17 | a late completion arrives after the cancel decision committed | Refused, typed, with **no journal write**; the refusal's evidence survives retirement. Already-admitted descendant commands are not undone (§4). |
| W18 | native: OS process death | Nothing is promised. Native durability ends at the runtime's lifetime (§14). |
| W19 | **final record committed and its commit-order position assigned, crash before the drain runs** | Recovery drains in the **recorded** commit order and never re-derives it from whatever completes first on the redrive. The position is a durable fact assigned at the §4 linearization point, not a property of the run that observed it. |
| W20 | **two finals commit concurrently** | The linearization point serializes them, so exactly one takes the lower position, and both positions are durable before either drain begins. A tie is not resolvable by source index, by wall clock or by whichever writer returned first; if the point cannot order them it has not committed either. |

---

### Worked examples

lash TypeScript dialect. "Today" is `main`.

**1. Fan-out.** `await Promise.all([fetch_doc("a"), fetch_doc("b"), fetch_doc("c")])`
is concurrent on native, SQLite and Postgres and **serial on Restate** today — the
controller answers `fn supports_concurrent_effects(&self) -> bool { false }`, so
the batch takes its serial branch. Under this ADR it is three concurrent children
on every tier, and a redrive replays the same settlement order from the ranks. No
loser exists. FIG-3400 pins the parallelism by rendezvous.

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
successful-turn latency bound follows from the 50 ms grace.**

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
while the whole consumption stays inside that invocation (§8). The ingress
long-poll — unjournaled HTTP held open for the child's whole life — is removed in
the same cutover as its journaled replacement, never before, and implicit engine
cancellation never becomes the sole close protocol (§4).

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

- **Three facts replace one.** Child disposition, child rankability, and the
  opener's phase are separate; an implementation that derives any of them from
  another will be wrong at a crash boundary.
- **`race` and `any` become useful rather than merely accepted.** Carrying the
  cross-child source-order intent gate into groups would have made
  `Promise.race([slow(), fast()])` unable to resolve with `fast`, and one hung
  source-first tool would have blocked every sibling from ranking. Commit order
  removes that, at the cost of one observable change: `Promise.all`'s intent
  realization moves from source order to completion order, which is what an
  ECMA-262 host does anyway. FIG-3395's oracle holds the pre-cutover baseline
  (`sqlite_terminal_leaves_settle_in_source_order`) so the move is measured, not
  assumed.
- **Closing is a durable fact with its own crash windows.** W9–W12 exist only
  because the transition is recorded; without it they are indistinguishable from a
  live opener.
- **`ExecutionScope::Process` is not sufficient identity.** FIG-3394 must bind the
  incarnation; until it does, group identity can alias across a process
  re-registration.
- **Session deletion gains an obligation.** Today both SQL retirement paths delete
  group rows and neither consults a closing group.
- **Cancellation stops becoming a usage eraser.** Known usage survives a cancelled
  attempt, and unknown stays unknown under ADR 0032.
- **`Promise.race([])` ends the cell.** The program's semantics are ECMA's — the
  promise never settles — and the host reports a typed unsettled-await failure
  rather than parking forever.
- **The bound is a reservation protocol, not a number.** FIG-3397 names the
  accounting units and their release conditions before mid-aggregate suspension is
  considered.
