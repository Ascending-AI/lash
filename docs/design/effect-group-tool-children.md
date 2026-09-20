# Effect-group tool children: lifecycle and observation contract

**Status: ruled, not implemented.** This note records the lifecycle and
observation contract for tool children of effect groups, decided under FIG-1538
and ruled in FIG-3392. Nothing here describes behaviour `main` has today: on
`main` a tool batch is still one atomic effect, `Promise.race`/`Promise.any` are
refused at lowering, and no production caller opens an effect group at all. The
note exists so that FIG-2266, FIG-3396 and FIG-3397 can be built without further
rulings; [what each ticket implements](#what-each-ticket-implements) maps every
clause onto its owner.

The endpoint itself — the *hybrid*, `live → closing → settled` — is ruled and is
not reopened here. Two endpoints were considered and rejected: **A (reduced)**,
in which a losing tool child is durably owned by the host and finishes after its
opener ended, and **B-prime**, in which a crash past the winner abandons an
accepted child with a recorded decision. A loses to unobserved post-turn side
effects (worked examples 4 and 5); B-prime loses because abandoning a child
whose final attempt already committed destroys recorded declarations that
[ADR 0042](../adr/0042-tool-attempts-are-atomic.md) protects and that
[ADR 0094](../adr/0094-child-lifecycle-is-a-registration-fact-settled-by-scope-end.md)
calls "interrupted, not ended".

## How to read this

**Anchors carry a path and quoted text, never a line number.** A line pin rots
on the next edit to the file above it and then points confidently at the wrong
construct; a quoted phrase either still exists or visibly does not. Where an
anchor names something that does *not* exist yet, it says so.

**Every section ends with an implementation line.** "Holds on `main`" means the
clause is already true of the code. "New" means it is a target the named ticket
builds. A clause with no implementation line is an error in this note.

**The ADRs are the decision of record; this note is the contract.** Where a
clause changes an ADR, the ADR carries an amendment naming FIG-3392, and this
note is the longer form. The amended ADRs are
[0025](../adr/0025-bounded-journals-are-an-effect-controller-obligation.md),
[0042](../adr/0042-tool-attempts-are-atomic.md),
[0062](../adr/0062-the-typescript-dialect-is-an-exact-ecma-262-subset.md),
[0065](../adr/0065-concurrent-settlement-is-a-durable-group-at-the-effect-host-seam.md),
[0094](../adr/0094-child-lifecycle-is-a-registration-fact-settled-by-scope-end.md)
and [0095](../adr/0095-processes-are-values-and-process-controls-are-tools.md).

## 0. The lifecycle in one page

One opener, three phases, one law per phase.

**live.** Aggregate selection cancels nothing. A losing child keeps running,
exactly as a losing promise does in ECMA-262. Worker loss does not end an
opener: an accepted child is *recovered* while its opener lives, from durable
input on the SQL and Restate tiers and from retained in-memory references on the
native tier.

**closing.** Semantic opener end — `finish`, turn cancellation, process terminal
— stops new tool work and cancels every *unfinished* attempt under the standing
cancel law. Every attempt whose **final result already committed** keeps its
intent outcomes and its required projection, through recovery if the worker dies
in the middle. A close deadline may suspend or fail finalization with retained
recovery work; it may never erase that work or report cleanup complete.

**settled.** Accounting and whole-group retirement. Only here may the group's
rows retire, and they retire whole (ADR 0065 N3).

Two latencies, and they are separate numbers: **winner latency** (when `race`
resumes — at the first settlement) and **finalization latency** (when the opener
reports success — after closing drains its protected settlements). Conflating
them is how "fast `race`" gets mistaken for "fast turn".

**Background work that must survive `finish` is a process, named by the
program.** This supersedes FIG-1416 ruling 3 for *unfinished tool execution at
normal opener end* and for nothing else; see §7 and the ADR 0065 amendment.

---

## 1. Logical opener identity

**An opener is the logical process incarnation or turn, not a worker attempt, a
lease, a handler execution or a segment.** A group's opener is named by the
execution scope the group key already carries, and that name is stable across
every worker attempt and every segment the incarnation runs.

A group key is `{scope_id}:group:{batch_id}:{occurrence}` —
`crates/lash-core-execution/src/runtime/effect/group.rs` carries it as doc text
("`{scope_id}:group:{batch_id}:{occurrence}`") and
`crates/lash-core-execution/src/runtime/effect/group_journal.rs` documents the
same string as "the group's primary key". `scope_id` is the opener. The
`occurrence` ordinal discriminates two textually identical aggregates in one
protocol iteration, because `batch_id` is a content hash
(`crates/lash-core-execution/src/session/tool_execution.rs` derives it through
`deterministic_tool_invocation_batch_id` over the call preimage, under
`TOOL_BATCH_FAMILY_VERSION`).

**Recovery distinguishes a live opener from a closed one by durable facts, never
by the liveness of a worker.**

- A **live opener** is one whose scope has neither retired nor recorded its
  semantic end. On the SQL tiers the drain refuses a group whose caller is still
  entitled to it: `crates/lash-core-execution/src/runtime/effect/group_drain.rs`
  states "A group this process is still working is refused outright. The drain
  reclaims groups whose caller is gone."
- A **closed opener** is one whose end is a durable fact. For a process parent
  that is its terminal outcome; for a turn parent it is the parent-end ledger row
  ADR 0094 describes ("Each of the three turn exits and each terminal process
  completion writes one parent-end ledger row").
- **A dead worker is neither.** ADR 0094 says it plainly: "A turn that crashed
  before its commit is interrupted, not ended." Nothing in this contract may read
  worker loss as opener close.

The two failure modes this forbids are named so a reviewer can look for them: a
recovery pass that treats a missing lease as a closed opener (it would cancel a
live opener's children), and a close path that treats a live opener's crash as
its semantic end (it would abandon work the opener is still going to observe).

**A group's occurrence ordinal rides the VM continuation**, so two identical
`race` calls straddling a segment boundary do not both derive occurrence 0. ADR
0065 already requires this and ADR 0025 already enumerates occurrence counters
among the continuation's contents; the Lashlang segment state carries them today
(`crates/lash-lashlang-runtime/src/process.rs`, `LashlangSegmentState` with its
`ordinals: ReplayOrdinalsState`).

*Status.* The group-key shape holds on `main` as a contract type; **no
production path mints an occurrence** — `deterministic_tool_invocation_batch_id`
has no occurrence component and the `:{occurrence}` shape appears only in doc
comments and test fixtures. Closing that gap is FIG-3394. The opener-versus-worker
distinction is **new** and is FIG-3396's.

---

## 2. Invocation driver

**A tool child is a replayable invocation driver.** Coordination — retry,
completion-key derivation, deferred await, the orchestrating lane — runs at
*handler level*, on the child's own admitted controller. Only atomic attempts run
inside recorded bodies.

This is ADR 0042's structural rule unchanged: "A recorded body must not emit
commands into an ordinal-addressed journal." On Restate the consequence is
sharper, because a recorded body is a `ctx.run` closure: nothing inside it may
emit a command, and no SDK can interrupt a closure that is already executing.
`crates/lash-restate/src/lib.rs` states the first half ("not to call the Restate
context from inside the closure"); the second half is a property of the pinned
SDK, and it is the strongest technical reason the driver must sit at handler
level rather than inside one `ctx.run`.

**There is one resolver for first dispatch and for recovery, and it is the
host's.** ADR 0065 already made the registered `GroupExecutors` resolver
normative: "the open, a retry after a claim expires, and the loser drain all
reach for a child's runner through the same registered object". The code has it:
`crates/lash-core-execution/src/runtime/effect/group_drain.rs` resolves through
`fn executor_for(&self, envelope: &RuntimeEffectEnvelope)`.

**No caller-closure route is added back.** A caller-supplied executor vector can
answer only first dispatch; retry, drain, a resuming process reopening a group it
never opened, and a fresh handler execution have no caller in scope. ADR 0065's
FIG-1578 amendment already retired the caller-paired form, and this arc does not
reintroduce it under another name.

**On Restate, children become `call` children of the parent with the child's
replay key as the idempotency key.** Today they are not: every group child is
dispatched one-way —
`crates/lash-restate/src/effect_group/dispatch.rs` builds each child with
`.child(Json(EffectGroupChildRequest { … })).send()`. This matters beyond
tidiness: the pinned SDK's implicit cancellation covers `call` children and
deliberately exempts one-way sends, so **implicit cancellation covers zero group
children today**. Any claim that the engine already cancels lash's group children
is false on `main` and becomes true only after this cutover.

*Status.* The resolver seam and the recorded-body rule **hold on `main`**. The
handler-level driver, the `call`-child dispatch and the idempotency-keyed
identity are **new**, and are FIG-2266's.

---

## 3. Execution authority

**The durable child request reuses `ProcessExecutionEnvRef` and existing
artifact ownership, and adds only what environment capture lacks.**

Environment capture already exists and already publishes under an owner:
`crates/lash-core-execution/src/session/execution_context.rs` exposes
`captured_process_execution_env_ref(&self, owner: &crate::ArtifactOwner)`, and
`crates/lash-core-store/src/process_identity.rs` shows what a captured
environment *is* — `ProcessExecutionEnvSpec { plugin_options, policy }`, two
fields and no more.

Two fields are not a tool execution descriptor. What capture lacks, and what the
durable child request therefore carries, is exactly four things:

1. **the admitted grant** — `crates/lash-core-execution/src/tool_provider.rs`
   keeps it separate from the call
   (`pub struct PreparedToolBatchCall { pub call, pub replay_suffix, pub
   execution_grant: Option<Box<ToolExecutionGrant>> }`), and the grant is what
   carries `source_id` and `execution_binding`;
2. **the admitted scope**, so the child validates its envelope against the scope
   it was admitted under before doing any work;
3. **lineage** — the opener, the group, the position, and the stable call/attempt
   identity the child's usage and intents are attributed to;
4. **cancellation authority** — who may cancel this child, and under which fence.

**No second environment store, and `RuntimeExecutionContext` is never
serialized.** The context is a borrowed live object
(`RuntimeExecutionContext<'run>`); serializing it would capture live senders,
plugin handles and a turn's borrowed authority, none of which survive a handler
boundary and none of which a durable request may contain. Semantic completion
facts travel; live channels do not.

**A pre-journal publish must tolerate its own prior success, and a durable-owner
publish must not.** `captured_process_execution_env_ref` deliberately fails on a
retired owner, and says why in its own doc comment: "Every caller publishes under
a durable owner and then persists the reference … so a retirement must surface at
publish time rather than hand back a reference to bytes the fence already
reclaimed. Process starts do not publish at all before their journal: they go
through `Self::process_start_execution_env`." A child request is on the durable
side of that split: it persists the reference, so it keeps the strict behaviour.

*Status.* Environment capture, artifact ownership and the separate grant **hold
on `main`**. The durable child request that carries grant, scope, lineage and
cancellation authority is **new**, and is FIG-3396's.

---

## 4. Commit versus cancel arbitration

**Final-attempt commitment is serialized against cancellation, and a winning
commit retains settlement ownership.**

The protected phase is real and already coded. A child that has committed its
final result is exempt from the cancel deadline:
`crates/lash-core-execution/src/session/tool_execution/batch.rs` runs the cancel
arm as `if final_result_committed.is_committed() { return tool_call.await; }`
before it arms its `Duration::from_millis(50)` grace, and even inside the grace
race it keeps an arm for `() = final_result_committed.committed() => tool_call.await`.
The signal is published by the drain guard at the moment the terminal is sealed:
`crates/lash-core-execution/src/tool_dispatch/attempt_coordinator.rs` documents
`begin_final_drain` as "Publishes this child's committed final result and waits
for its turn to drain the declared intents", and `settle_terminal_attempt` calls
`slot.begin_final_drain().await` before `execute_final_tool_intents` and
`project_recorded_intent_outcomes`.

ADR 0042 states the law the protection exists for: "Lash records the final
attempt first, then admits and drains its declarations in source order."

### The transition table

One child attempt. `C` = a durable cancel/abandon disposition has been recorded
for this child. Rows are the child's state; columns are the event.

| state | cancel recorded (`C`) | final result commits | grace expires | worker loss | opener reaches close |
|---|---|---|---|---|---|
| **dispatched, uncommitted** | → *cancelling* (fence armed; body signalled) | → **committed** (commit wins; `C` is refused after this point, not queued) | n/a | → *dispatched* (recovered while opener lives, §1) | → *cancelling* |
| **cancelling** | no-op (idempotent) | → **committed** *only if* the commit landed before the fence closed; otherwise refused as a typed late completion | → **cancelled** (terminal; no intents realized) | → *cancelling* (re-driven from the recorded disposition) | no-op |
| **committed** (final attempt recorded, intents not drained) | **refused**: the fence may not cover a committed final | n/a | **not armed**: a committed final is exempt from the deadline | → **committed** (recovery must finish the drain; this is the window B-prime would have erased) | → **committed**; closing waits for the drain |
| **drained** (intents realized, projection recorded) | refused | n/a | n/a | → **drained** (facts are durable) | → **rankable** |
| **rankable** | refused | n/a | n/a | → **rankable** | → **settled** |
| **cancelled** | no-op | **refused** as a late completion; no journal write | n/a | → **cancelled** | no-op |

Two readings the table forbids. A commit arriving *after* the fence closed is a
typed late-completion refusal with **no journal write**, not a silent
resurrection. A cancel arriving *after* a commit is refused, not deferred: an
implementation that queues it and applies it once the drain finishes has
reintroduced exactly the loss the protected phase exists to prevent.

### What the fence covers

**Cancel/abandon is a durable, fenced disposition, distinct from physical stop.**
Lash has no hard-kill primitive — ADR 0042 says so ("Lash processes are
cooperative and Lash has no hard-kill primitive; engines own their own kill
semantics") — so a disposition can never claim the work stopped. What it can
claim is that no *further authoritative write* will be accepted. The fence
therefore covers four sinks, not one:

1. **child dispatch** — a fenced group admits no new child;
2. **nested semantic writes** — an escaped coordinator may not emit a trigger,
   register a process, or mutate the live possession or usage buffers;
3. **completion delivery** — a late terminal is refused at ingress *and* the
   refusal survives the group's retirement;
4. **rank insertion** — the sink a fence that covered only rank would have
   caught, and the least important of the four.

A completion-ingress refusal that only guards (4) arrives too late. By the time
an escaped coordinator's completion reaches ingress it has already realized its
intents; the writes are done and refusing the rank changes nothing.

**Retirement requires discharged obligations plus a surviving identity fence.**
The quiescence read already refuses to retire over unfinished work:
`crates/lash-store-sql/src/effect.rs` defines `scope_is_quiescent` as an
`EXISTS` over "`runtime_effect_replay` … `status = 'in_progress'`", a
`runtime_effect_group` whose `grp.children > (SELECT COUNT(*) … )`, and an
`await_event_waits` row with `terminal_json IS NULL`. Its own doc text says why
it is one statement: "quiescence is the one question whose answer spans both
families, and splitting it into three statements would let a child start between
them." Deleting an abandonment row together with its group destroys that row's
own refusal evidence unless the scope-retirement fence outlives it; that fence is
the surviving identity.

*Status.* The committed-final exemption and the 50 ms grace **hold on `main`**
for the in-process batch path. The durable fenced disposition, its four sinks and
the late-completion refusal are **new**, and are FIG-3396's.

---

## 5. Rankability and intent order

**A child becomes rankable only after its own authoritative final intent
outcomes and its required projection.** Publishing a rank before the intents
drained would let a consumer observe a settlement whose declared effects have not
happened, and then checkpoint past it.

**Source-order intent admission is preserved within a group**, with recorded
discharge wherever separate handlers or recovery need it. The in-process gate
exists and is documented in
`crates/lash-core-execution/src/tool_dispatch/attempt_coordinator.rs`:
`BatchIntentDrainGate` where "`next` names the slot whose drain may run … A slot
publishes its own completion into `discharged`; `next` then walks forward over
the consecutive discharged prefix", and `IntentDrainGuard`, "One child's
exactly-once claim on its slot … Holding the guard is the claim; dropping it
discharges the slot."

That gate is a `Mutex` plus a `Notify`. **Two Restate handlers cannot share it,
and a crash past a checkpoint loses it.** So the ordering it provides must be
representable durably wherever independent invocations or recovery reconstruct
it — as discharge facts in the existing group authority, not as a new scheduler.

**The head-of-line delay is stated, not hidden.** A source-earlier child that is
still draining delays a source-later child's intent realization even when the
later child returned first. That is the price of a defined order and it is
accepted; what is refused is an *undefined* barrier — in particular any rule of
the form "drain every already-settled sibling", which either means "already
projected" (circular) or "body-returned" (not yet a final success), and in the
second reading adds a barrier behind an unrelated sibling.

**Across the resumed continuation and running losers there is no total order, and
none is invented.** After winner `W` resumes the opener, the continuation's
intent `C` and a loser's intent `L` can reach the same registry or trigger target
in either order. Source order within the old group says nothing about `C` versus
`L`. The rule is therefore two-part:

- **existing target transaction order decides** — the registry and the trigger
  store already serialize their own writes, and that serialization is the answer;
- **the consumer-visible observation prefix is journaled** — what the opener has
  *observed* is a durable fact, so a replay observes the same prefix rather than
  re-racing.

**No turn-wide intent scheduler.** A global ordering over every intent a turn
emits would be a new subsystem whose only customer is a race nobody can observe
except through timing.

*Status.* The per-group gate and its discharge **hold on `main`**, in process.
Recorded discharge across handlers and recovery, and the journaled observation
prefix, are **new**: FIG-3396 owns the discharge facts and FIG-3397 lands the
prefix with the integration.

---

## 6. Projection incorporation

**Started-process possession and trigger evidence from a settled child are
incorporated into the opener before any observation that depends on them, are
checkpointed as an incorporated prefix, and are rebuilt before dependent
continuation effects replay. Losing values stay unreturned.**

Possession is not decorative; it is the authority that makes a started child
reachable. `crates/lash-core-execution/src/session/process_handles.rs` says what
happens without it: "A start declared as a tool intent is realized in
`tool_dispatch`, which holds no runtime execution context, so nothing recorded it
and the child was unreachable to the very run that started it — `await handle`
refused with `ProcessNotVisible`, deferred or not." The fix in place today is
`record_processes_started_by_intents`, which takes possession "from the same
realized outcome the bound value's projection is taken from".

Possession is run-local state that already rides a segment handover:
`crates/lash-core-execution/src/session/execution_context.rs` carries
`restore_started_process_ids` ("Restore run-local child possession for a resumed
process-engine segment") and `started_process_ids` ("Snapshot run-local child
possession before a process-engine segment handover"), and
`crates/lash-lashlang-runtime/src/process.rs` puts `started_process_ids:
host.ctx.started_process_ids()` into `LashlangSegmentState` at the boundary.

Three consequences the arc adds:

1. **A Restate child mutating its own possession set grants the parent nothing.**
   The child is another invocation with another context. Its settlement must
   *carry* the semantic facts — which processes it started, which triggers it
   registered — and the opener must incorporate them on receipt.
2. **A checkpoint taken before a loser's projection cannot restore the later
   grant.** So incorporation is checkpointed as a prefix: "the opener has
   incorporated settlements 1..k" is the durable fact, and a replay rebuilds
   exactly that prefix before replaying any continuation effect that depends on
   it.
3. **Already-realized starts and triggers are not erased by refusing a late
   completion.** A refusal at completion ingress suppresses *delivery of the
   result*, never the world. After close, ADR 0094 governs any process that
   really did start: its Lifecycle Policy and Parent Scope are registration
   facts, and the parent-end sweep settles it.

**Losing values stay unreturned.** A loser's result never becomes a program
value, in any phase. Incorporation is about *facts the runtime owns* — possession,
trigger evidence, usage — not about the model seeing a value it did not select.

*Status.* Possession recording and its segment handover **hold on `main`**.
Cross-invocation carriage of the semantic facts and the checkpointed
incorporated prefix are **new**: FIG-3396 owns the carriage, FIG-3397 the
prefix.

---

## 7. Closing

**At semantic opener end: stop new tool work; cancel unfinished attempts under
the standing cancel law; finish protected settlement before final success and
before accounting.**

"Semantic opener end" is `finish`, a turn cancellation, or a process terminal. It
is *not* a segment boundary (§9) and *not* worker loss (§1).

**A close timeout suspends or fails finalization with retained recovery work,
driven by the existing work driver.** It never erases retained work and never
reports cleanup complete. ADR 0065 already has the machinery: under
`RunToCompletion` on the SQL tiers "ownership of unfinished children transfers to
the queued-work driver as a group-drain item keyed by the group", and the drain
"reads the disposition declared on the group row and applies it; it never invents
a policy at drain time".

The drain's queue is the journal, not a second table:
`crates/lash-core-execution/src/runtime/effect/group_drain.rs` states "Nothing is
enqueued. The drain's work list is
`EffectReplayRowStore::read_unsettled_group_children` … No synthetic queued-work
item, no `work_kind`, and no table exists for this: a second queue would be a
second copy of a fact the effect journal already holds exactly, and the two would
disagree the first time one of them was written without the other."

**Winner latency and finalization latency are separate.** `race` resumes at the
first settlement; `finish` completes after closing has drained its protected
settlements. A fast `race` does not imply a fast turn, and an acceptance
measurement that reports one number for both is measuring nothing.

**No fresh attempt retries after close** except what is necessary to recover a
committed obligation. Closing is not a retry window.

**Session delete waits for settled.** A session whose group is still closing has
protected obligations outstanding; deleting it would strand them. The existing
pin refusal is the mechanism, not a new one.

### What this supersedes, exactly

FIG-1416 ruling 3 assigned losers to the queued-work driver under both
dispositions and explicitly rejected leaving them on the opening scope's task
set. **This arc supersedes that ruling for unfinished tool execution at normal
opener end, and for nothing else.** Recovery ownership of protected settlement
stays. What the owner gives up is implicit durable background tools: an
unfinished loser is no longer guaranteed to eventually perform its opaque work or
produce new intents after `finish`. What the owner keeps is the explicit process,
whose ADR 0094 lifecycle is unchanged.

**Opener-close cancellation is a host lifetime contract, not a `race`
divergence.** Within a live opener, letting losers run *is* Promise semantics —
nothing diverges. The divergence is at opener end, and the honest Node comparison
is a host that stays alive after an async function returns: in a still-running
Node process, returning from an async function does not cancel a losing timer or
socket, and its later write can land after the caller returned. Lash suppresses
that write at opener end. Comparing lash's turn end with Node *process death*
would hide the difference rather than state it. `finish` and host shutdown are not
ECMA-262 concepts, and an unresolved promise alone does not keep Node alive, so
ECMA-262 cannot be cited either for or against this rule. It is a lifetime
contract and is documented as one (ADR 0062 amendment).

*Status.* The drain, the disposition-at-open rule and the work-driver seam **hold
on `main`**. Close-phase cancellation of unfinished attempts with protected
settlement retained, the close timeout, and the ruling-3 supersession are
**new**: FIG-3396 owns close recovery and FIG-3397 lands it on the product path.

---

## 8. Rank authority and consuming-bridge replay

**One rank authority per group.**

Restate's own ordering guarantee is real and is invocation-local. The protocol
requires that "the relative ordering of notifications delivered by the runtime —
completions, signals, and run-completion acks alike — is significant: on replay
the SDK MUST observe the same relative order it observed when processing"
(the Restate service-invocation protocol document in
`restate-sdk-shared-core`). That is sufficient for a consumer that replays the
same selection schedule inside the same invocation. It is **not** sufficient for
a successor segment: attaching several already-finished children from a new
invocation produces new notifications in a new order, which is not proof of the
predecessor's original observation order.

The rule follows:

- **SDK notification order may serve only where same-invocation replay proves the
  whole contract** — the consumer, its selection schedule and every consumption
  it will perform are inside one invocation.
- **The existing group authority stays wherever consumption, discharge or
  retained observation crosses invocations.** ADR 0065's Restate satisfaction
  section already names it: the group virtual object keyed by the group key, with
  the counter and shape fence beside each other.
- **Removing the group object requires proof of the whole cross-segment
  contract**, not a demonstration that same-invocation selection is
  deterministic.

**Consumed prefix and required result retention persist across segment
handover.** The handle already models the cursor:
`crates/lash-core-execution/src/runtime/effect/group.rs` carries
`EffectGroupHandle` with `group_key`, `children` and `consumed`, and ADR 0065
makes the handle "the sole cursor of record", with reopen returning `consumed =
0` so "a restored frame supplies the cursor it saved". What this arc adds is the
*result* side: a successor must be able to obtain the settlement payloads its
cursor still needs, not merely the count.

**Expired Restate attachment is a typed recovery failure, never permission to
rerun a side effect.** Attach is bounded by retention — the Restate server
defaults both journal retention and idempotency retention to 24 hours — and from
Rust the SDK constructs only `AttachInvocationTarget::InvocationId`, so
idempotency-key attach is reachable only through the ingress HTTP client. A
successor that can no longer attach has lost a *result*, not gained a licence to
re-execute an opaque tool body.

**Attach is by invocation id, so the id is durable state.** Retaining the exact
child invocation identity across handover is therefore part of the child record,
not an optimisation.

*Status.* The group object, the cursor and the shape fence **hold on `main`**.
Retained invocation ids, retained result payloads across handover and the typed
expired-attachment failure are **new**: FIG-3396 owns retention, FIG-3397 lands
reattachment.

---

## 9. Segments and bounds

**Outstanding children are reattached across segments. Boundaries are never
deferred indefinitely.**

"No segment boundary while tool children are unsettled" fails ADR 0025. That ADR
requires segmentation by accumulated step cost and requires identical results
whichever schedule a process takes: "a process must compute identical results
whether it segments every N steps or never", and "no authoring construct or host
projection ever sees a segment". A days-long process that repeatedly races one
quick tool against one hung tool sits at width two forever and would never
segment. Width is not duration, not command count, not nested orchestration
depth, and not total outstanding children.

The boundary is already declinable rather than mandatory:
`crates/lash-lashlang-runtime/src/process.rs` records a decline through
`record_segment_boundary_decline` with the message "lashlang segment boundary
declined at non-capturable point". Declining *at a non-capturable point* is
correct; declining *because a tool child is unsettled* is the rule this note
refuses.

**Admission bounds aggregate width AND retained outstanding work, nested groups
included, as a typed refusal of the whole open before dispatch, with command
headroom reserved for close and handover.**

- The refusal is of the **whole open**, before anything is journaled. ADR 0065
  already makes the whole-open refusal load-bearing for the no-executor case
  ("nothing of the group is journaled — the group row included"), and the same
  reasoning applies: a half-admitted group whose operator retries is answered as a
  *reopen*, which passes the miss through by design.
- **There is a width ceiling today only in the protocol, not in the group.**
  `crates/lash-protocol-standard/src/lib.rs` has `const BATCH_MAX_TOOL_CALLS:
  usize = 25`; `crates/lash-core-execution/src/tool_provider.rs` collects a
  prepared batch with no width admission, and the group constructor checks
  non-emptiness, membership and duplicate keys without a ceiling. The VM bridges
  forward their operand vectors directly.
- **Command headroom.** ADR 0065 already accepts that "a group turns one
  journaled effect into n" and that a 50-leaf group costs 50 against the segment
  budget; the Restate controller's budget is a construction-time option
  (`crates/lash-restate/src/controller/mod.rs`, `segment_effect_budget` with a
  default of `10_000`). Admission must leave enough of that budget for the close
  and handover the group will need.

**Mid-aggregate VM suspension is required only if command accounting shows the
bounded aggregate cannot meet the controller budget.** It is not assumed. A
resumable mid-aggregate VM frame is a large piece of work and this note does not
pre-authorize it.

**No universal result-byte cap follows.** ADR 0025 says so directly: the
segmentation guarantee "does not claim that every effect result has a universal
byte ceiling", and "changing to a universal byte rejection would be a separate
product contract because it can turn an otherwise valid tool result into a
deterministic failure." Intent payloads keep their existing hard admission bounds
(at most 32 declarations, at most 16 of one kind, at most 64 KiB of canonical
intent JSON per completed attempt).

*Status.* Segment boundaries, the decline path and the effect budget **hold on
`main`**. Reattachment across segments and the two-dimensional admission bound
are **new** and land together in FIG-3397.

---

## 10. Aggregate laws

Four laws, stated so that the consumer surface and the journaled surface cannot
be confused with one another.

**L1 — the four-way consumer mode is independent of the three-way journaled wake
policy.** `GroupWakePolicy` has exactly three variants — `First`, `FirstSuccess`,
`All` — and ADR 0065 explains why `all` and `allSettled` share one: "they ask the
host for exactly the same thing — deliver settlements in durable rank order, keep
the rest running — and differ only in how far the *caller* consumes". The wake
policy is journaled identity, folded into every child's envelope hash; a fourth
variant would change the identity of groups already recorded. The consumer mode
(`all`, `allSettled`, `race`, `any`) is a caller-side loop decision and is never
journaled.

**L2 — the response is typed: selected, all-results, or exhausted-rejections,
each with original positions and terminal classification.** A consumer receives
one of three shapes and never a bag of values it has to interpret:
- *selected* — one settlement with its original input position (`race`, and
  `any`'s success);
- *all-results* — every settlement, in original input positions (`allSettled`,
  and `all`'s success);
- *exhausted-rejections* — every rejection, in original input positions, when no
  arm can succeed (`any`'s failure).
Original positions are carried, not re-derived: the prepared batch already treats
its vector order as source order
(`crates/lash-core-execution/src/tool_provider.rs`: "The vector order is source
order. Calls run concurrently, but launches and pending completion consumption
are projected back through this order").

**L3 — infrastructure failure and cancellation are distinct from tool
rejection.** A tool that ran and said no is a rejection the program may catch. A
child that could not be routed, a child whose attachment expired, and a child
that was cancelled are *not* rejections and must not be handed to a `catch` as
though the tool had answered. ADR 0065 already takes this posture for routing: "A
child with no runner is a routing fact, not an outcome … so no terminal is ever
synthesized from a miss."

**L4 — a loser's value is never synthesized.** No `undefined`, no
`{status:"cancelled"}` smuggled into an `allSettled` result array, no placeholder
for a child that did not settle. If a position has no settlement, the response
shape says so in its own vocabulary or the group is not yet consumable at that
position.

*Status.* The three-way wake policy and the source-order projection **hold on
`main`**. The typed four-way response, the classification split and the
no-synthesis law are **new**, and land in FIG-3397. FIG-3395 pins the current
baseline before they land.

---

## 11. Value model

**One pending-operation handle for tools and timers.** The VM already has one
handle encoding — ADR 0095 made it `{__handle__: "lash", id}` with "one mint/parse
pair shared by the language and core" — and this arc does not add a second for
timers.

The clauses, each stated as a law:

1. **Bound arrays and duplicates.** An aggregate operand may be an array-valued
   expression, an array held in a binding, or a literal, and the same pending
   operation may appear twice. **Execution deduplicates; input positions never
   do.** Two positions naming one operation settle from one execution and both
   report, at their own positions.
2. **Operands evaluate once, in source order.** Evaluating the operand array is
   an ordinary expression evaluation and happens exactly once, left to right,
   before any settlement is consumed. ADR 0086 already fixes this for
   comprehensions ("the comprehension evaluates its clauses in source order
   (filters and nested clauses included), collects one `(receiver, args...)` tuple
   per accepted element, and starts every call as one host batch after the loop").
3. **Every pending operation is admitted, even when a plain value decides the
   aggregate.** A `race` whose array contains an already-resolved plain value
   still admits and dispatches its pending siblings; it does not skip the open
   because it can answer immediately. Skipping would make a side effect depend on
   an operand's arrival order.
4. **A timer's start point is its admission, and its fulfilment value is
   `undefined`.** The timer starts when the group opens, not when the consumer
   first awaits, and not at the operand's evaluation.
5. **`Promise.race([])` is a registered refusal.** In ECMA-262 it returns a
   promise that never settles. ADR 0065 already refuses empty groups and gives
   the reason: an empty `race` "is a never-settling program that must not become
   an unbounded durable await". This is a deviation and is registered as one, not
   accepted with a nearby meaning.
6. **`Promise.any([])` rejects with an `AggregateError`,** as ECMA-262 specifies,
   and needs no group at all. The heap already validates the shape:
   `crates/lashlang/src/runtime/heap/validation.rs` refuses an "AggregateError
   object … missing its errors list" and refuses a non-aggregate error that
   "carries AggregateError errors".
7. **`AggregateError.errors` is input-ordered**, not settlement-ordered. The
   settlement order decides *which* rejection an unwrapping aggregate reports
   (§10 L2); it does not reorder the collected errors array.
8. **A raw process handle at an element position stays refused**, with the repair
   naming the tool. The refusal exists today:
   `crates/lashlang/src/runtime/vm/pending_tools.rs` carries
   `PROCESS_HANDLE_LEAF: &str = "a process handle cannot be awaited directly; call
   \`processes.await(handle)\` and await that call, so the durable wait settles
   with the rest of the batch"`.
9. **Async-map operands are aggregate operands.** `await Promise.all(xs.map(async
   x => …))` produces pending operations like any other operand array. The v1
   async array driver runs callbacks **sequentially**, which is a registered
   deviation (`TS_ASYNC_MAP_SEQUENTIAL_V1`): result order matches Node, while
   callback interleaving and shared-mutation order can differ. This arc does not
   change that driver, and the deviation now carries its own census row.

*Status.* One handle kind, the raw-handle refusal, operand evaluation order and
`AggregateError` shape validation **hold on `main`**. `race`/`any` themselves do
not: `crates/lash-typescript/src/lower/calls.rs` still refuses them with
"Unsupported: Promise.{method} requires durable partial-settlement ordering
(FIG-1416)." Clauses 1–7 land in FIG-3397; FIG-3395 authors the oracle that pins
them.

---

## 12. Durable Wait as a child

**`processes.await(h)` is a resumable child on the existing Durable Wait
protocol, and cancelling it releases the wait, never the process.**

ADR 0095 made `processes.await` "a leaf tool that returns pending on a **Durable
Wait** resolved by the process terminal through the work-driver seam", and said
in the same breath "It is never a batch child." That sentence was written against
the *atomic* batch, whose children had to settle inside one resource operation —
which is exactly why a process terminal days away could not be one. Under this
arc a group child is a durable, independently recoverable unit with no such
bound, so the sentence changes meaning rather than being deleted: **a Durable
Wait is a child of the group, and it is a child of this kind — resumable,
retained across segments, not subject to a cancel grace as a running attempt
is.**

The routing does not move. `crates/lash-core-execution/src/runtime/effect/executor/process_local.rs`
already documents the equivalence ("`await processes.await({ handle })` answers
exactly what `await handle`"), and ADR 0087's replacement text already states the
law: "a parked leaf takes its place in the recorded order at the moment its
completion arrives, not at the position it was launched in. It is not a batch
child with a cancel grace, which is why the wait may last days without the
aggregate losing its ordering."

**Cancelling the wait is not cancelling the process.** The process has its own
captured environment, its own journal, its own cancel protocol and its own
terminal delivery, all of which exist today and none of which this arc touches. A
`race([processes.await(job), sleep(10_000)])` whose timer wins releases the wait
and leaves `job` running under its ADR 0094 lifecycle. This is the ruled answer
to "I want work that outlives the turn": name a process.

*Status.* The Durable Wait routing and the released-wait semantics **hold on
`main`**. Restating the wait as a group child of this kind is **new** and lands
in FIG-3397; the ADR 0095 amendment carries the rewording.

---

## 13. Usage

**Every known runtime-managed usage fact is attributed to its original opener and
to a stable call/attempt identity — losing and cancelled attempts included —
deduplicated before final accounting, and unobserved usage is reported as
unknown, never zero.**

The ledger this attaches to already exists:
`crates/lash-core/src/runtime/session_manager/direct_outcome.rs` records into a
shared ledger and deliberately does not commit, with the reasons in source —
"Record into the shared token ledger only. The ledger is the same `Arc` the turn
loop drains at turn-commit time … This usage is persisted exactly once by the
final turn commit", and "on effect-host replay this `apply` runs again with the
cached outcome, and an incremental persist would double-merge the usage into the
already-persisted state."

Four consequences:

1. **Winner-only accounting is false.** A loser can spend provider tokens before
   it is cancelled. Discarding its *result* does not discard its bill.
2. **A remote child's ledger is not the parent's ledger.** On Restate the child
   is another invocation with its own in-memory ledger, so its usage must travel
   as a semantic fact on its settlement.
3. **Attach and redrive must not double-count.** Identity is the deduplication
   key: the stable call/attempt identity, which ADR 0042 already defines as a
   lash-minted `LlmCallId` above the retry loop plus a per-attempt ordinal.
4. **A retired opener's ledger is not the next turn's ledger.** A late fact whose
   opener has retired is recorded as belonging to that opener or it is not
   recorded; it is never merged into whatever turn happens to be current.

**Unknown is a value.** ADR 0042 already concedes that "In-attempt effects are
consequently at-least-once; an LLM call can be billed again", and an opaque
request whose response is lost has genuinely unknown usage. Reporting zero for it
would be a false fact, and this arc does not claim to solve provider billing
exactly.

**No generic durable trace bus.** Semantic usage facts ride the existing usage
path. Live trace delivery is best-effort and stays best-effort; it does not
become a second durable message system.

*Status.* The shared ledger and its single commit point **hold on `main`**.
Per-attempt attribution across a child boundary, deduplication on attach/redrive
and the unknown-not-zero rule are **new**, and land with FIG-2266 (child-side
capture) and FIG-3397 (incorporation and accounting).

---

## 14. Native tier

**One semantic path on every tier, with substrate-specific storage.** Native is
not a second tool implementation; it is the same lifecycle with references in
memory instead of rows on disk.

**An owned opener supervisor polls losers while the VM or provider is
elsewhere.** A scoped vector polled only while the aggregate is being awaited is
insufficient: the opener resumes after the winner and then spends time in the VM
and in provider calls, during which a loser must still make progress, settle, and
have its facts incorporated. The supervisor is owned by the opener and lives as
long as the opener does.

**Contexts are retained through protected drain.** A native loser whose final
attempt committed still needs its execution context to drain its intents, so the
supervisor holds those contexts until closing finishes, not until the aggregate
returns.

**Unclaimed tasks are counted.** The host already keeps live counts and fences
them: `crates/lash-core-execution/src/runtime/effect/native_host.rs` describes
`ScopeLiveness` as "Effects executing and groups open under each non-session
scope, by journal key: the in-process twin of a journal's `in_progress` rows and
open group rows, which a quiescent-gated retirement must not cut under", and its
`admission` mutex "orders 'check the fence, then count as live' against 'prove
nothing is live, then fence'".

**No disk manifest, and no promise past OS-process death.** Native durability
ends at the runtime's lifetime, and the host already says as much about
externally routed completion keys: the opt-in is named
`allow_process_lifetime_completion_keys`, "Explicitly accept that externally
routed completion keys die with this process. Intended only for deliberately
single-process embeddings." The durable tiers keep reconstructible child input
and authority; native keeps references.

*Status.* Scope liveness, its fence and the completion-key opt-in **hold on
`main`**. The owned opener supervisor and context retention through drain are
**new**, and are FIG-2266's.

---

## Crash windows

Every window below is a crash of the worker or handler running the opener, the
child, or both. "Required outcome" is what an implementation must produce; an
implementation that produces anything else is wrong even if no test notices.

| # | window | required outcome |
|---|---|---|
| W1 | after the group open is journaled, before any child is dispatched | Reopen dispatches every child; no duplicate child executes, because dispatch is keyed by the child's replay key. Admission is **not** re-decided — the group's recorded shape is the fence (ADR 0065: a reopen "resolves only what it dispatches and refuses nothing"). |
| W2 | after dispatch, before any settlement, opener live | Accepted children are recovered and continue. The opener is live (§1), so nothing is cancelled and nothing is abandoned. |
| W3 | after a child settled and ranked, before the opener consumed it | The rank is durable. Replay serves rank `consumed + 1` and yields the same settlement (ADR 0065's cursor rule). |
| W4 | after the opener consumed the winner and checkpointed past the aggregate, losers in flight | Losers are recovered under the live opener, because the opener has not ended. This is the window Endpoint B-prime would have abandoned; it does not. |
| W5 | after a loser's final attempt committed, before its intents drained | Recovery finishes the drain and realizes the declared intents. The commit is protected (§4); losing these is the loss the hybrid exists to prevent. |
| W6 | after intents realized, before the opener incorporated the projection | Recovery rebuilds the incorporated prefix from the settlement's carried facts before replaying any dependent continuation effect (§6). A start that really happened is not undone. |
| W7 | during closing, before every protected settlement finished | Closing is resumable. The retained recovery work is driven by the existing work driver; the opener does not report success and does not account until closing completes. |
| W8 | after closing completed, before the group retired | Retirement is idempotent and group-atomic (ADR 0065 N3). A replayed close retires the same group once. |
| W9 | segment handover: continuation committed, successor not started | ADR 0025's three handover requirements apply unchanged — atomic boundary transition, idempotent successor start keyed to the stable process id, no second uncaptured pending operation. Outstanding children are reattached by the successor (§8). |
| W10 | child handler death with the opener alive | The child invocation is retried or reattached by invocation id. The opener does **not** infer abandonment from a dead handler. |
| W11 | attach retention expired before the successor segment attached | A typed recovery failure. Never a re-execution of an opaque tool body, and never a synthesized terminal. |
| W12 | session delete requested while the group is closing | Refused until settled (§7). The existing pin refusal is not weakened to let the delete through. |
| W13 | a late completion arrives from an escaped coordinator after the fence closed | Refused with a typed late-completion error and **no journal write**. The refusal's evidence survives the group's retirement (§4). |
| W14 | native: OS process death | Nothing is promised. Native durability ends at the runtime's lifetime (§14), and this is documented rather than approximated. |

---

## Worked examples

lash TypeScript dialect. "Today" is `main`; "hybrid" is this contract. Under **A
(reduced)** the loser is host-owned and finishes after the opener ends; under
**B-prime** a crash past the winner abandons it.

### 1. Fan-out

```ts
const [a, b, c] = await Promise.all([fetch_doc("a"), fetch_doc("b"), fetch_doc("c")]);
```

Today: concurrent on native/SQLite/Postgres, **serial on Restate** — the
controller answers `fn supports_concurrent_effects(&self) -> bool { false }`, so
the batch path takes its serial branch and the latency is the sum of three.

Hybrid: three concurrent children on every tier. Crash mid-batch and redrive
replays the same settlement order from the ranks. No loser exists, so all three
endpoints agree. FIG-3400 pins the parallelism by rendezvous.

### 2. Timeout, then keep working

```ts
const r = await Promise.race([slow_search(q), sleep(5000)]);
const answer = r ?? await cheap_search(q);   // the turn continues for a while
finish(answer);
```

`slow_search` keeps running after the timer wins. If it settles while the turn is
still working, its completion lands in the turn — usage, trace, triggers — and the
model never sees the loser's value. All three endpoints agree, and the hybrid adds
one thing: on Restate the completion is another invocation's, so its semantic
facts must travel back rather than being read out of a shared address space (§6).

### 3. Race, then finish immediately

```ts
finish(await Promise.race([search_a(q), search_b(q)]));
```

`search_a` wins at 300 ms; the opener reaches `finish` at 301 ms with `search_b`
in flight and uncommitted.

- **A:** `search_b` runs to completion under host ownership, possibly minutes
  later, and its usage and trace go to a host-owned destination.
- **B-prime and hybrid:** closing cancels it under the standing grace. The turn
  ends at about 350 ms. Nothing survives it.

### 4. Side-effecting loser

```ts
const sent = await Promise.race([send_email(msg), sleep(2000)]);
finish(sent ? "sent" : "still sending, will confirm later");
```

- **A:** `send_email` finishes after the turn. If it declares an intent — record
  delivery, start a follow-up process — the intent is realized after the turn
  ended, with nobody watching, and the model's next turn finds state it did not
  cause in any turn it can see.
- **Hybrid:** cancelled at close **unless its final attempt already committed**;
  if it committed, its intents are realized before the turn settles, and they
  survive a crash in that window (W5). The SMTP call may still have gone out
  either way: ADR 0042 already says in-attempt effects are at-least-once, so
  cancellation stops delivery of the *result*, not the side effect. The honest
  message to the user is the one the program wrote.

### 5. Loser whose result is a process handle

```ts
const h = await Promise.any([spawn_indexer("fast"), spawn_indexer("thorough")]);
finish(`indexing under ${h}`);
```

- **A:** the losing `spawn_indexer` completes later and its start intent is
  realized: a second indexer now exists, started by a turn that has ended, whose
  handle no program holds.
- **Hybrid:** if the loser settles while the opener lives, its process starts and
  possession reaches the parent replayably — including on Restate, where the child
  is another invocation and the possession fact must be carried (§6). Otherwise it
  is cancelled and the second indexer never starts.

### 6. Crash past the winner

```
t0  open group {search_a, search_b}      rows: a=in_progress, b=in_progress
t1  search_a settles, rank 1             a=terminal(rank 1)
t2  VM resumes, checkpoints past the aggregate
t3  worker dies; search_b was mid-HTTP-call
t4  redrive: the VM resumes from the checkpoint and never re-enters the aggregate
```

- **A:** the work driver re-dispatches `search_b` under a lease.
- **B-prime:** recovery records `abandoned-by-recovery`, retires `b`'s row with
  the group, and refuses any late completion.
- **Hybrid (W4):** `search_b` is **recovered**, because the opener is still live.
  A dead worker is not a closed opener.

**6b — the same crash, but `search_b`'s final attempt had already committed.**
A loses nothing; B-prime loses the recorded declarations; the hybrid realizes
them (W5). This case is why B-prime was rejected: dropping a committed
declaration is not "cancellation before tool completion", it is the destruction of
a fact ADR 0042 protects.

### 7. Work that must outlive the turn

```ts
const job = await processes.start({ definition: reindex, args: { scope } });
const r = await Promise.race([processes.await(job), sleep(10_000)]);
finish(r ? "done" : `running as ${job}, ask me later`);
```

Cancelling the losing `processes.await` releases the wait and never cancels the
process (§12). The process keeps its own captured environment, journal, cancel
protocol and terminal delivery — all of which already exist.

### 8. Restate mechanics under the hybrid

```rust
// Shape, not current code. Today each child is dispatched with `.send()`
// (`crates/lash-restate/src/effect_group/dispatch.rs`), so the SDK's implicit
// child-call cancellation covers none of them.
let mut pending = DurableFuturesUnordered::new();
for child in group.children() {
    pending.push(
        ctx.service_client::<ToolChildClient>()
            .run(Json(child.request()))
            .idempotency_key(child.replay_key())
            .call(),
    );
}
while let Some((position, settled)) = pending.next().await? { /* … */ }
```

The child handler runs the existing per-leaf coordinator **at handler level**:
atomic attempts inside `ctx.run`, coordination outside it (§2). The parent's own
selection order serves as rank only while the whole consumption stays inside this
invocation; the group authority serves every consumption that crosses one (§8).
The ingress long-poll — an unjournaled HTTP call held open for the child's whole
life — is removed in the same cutover as its journaled replacement, never before.

---

## What each ticket implements

| Clause | FIG-2266 (one invocation-aware tool driver) | FIG-3396 (accepted work and protected close recovery) | FIG-3397 (integration landing) |
|---|---|---|---|
| §1 opener identity | — | live-versus-closed opener test | occurrence on the product key (with FIG-3394) |
| §2 invocation driver | **owns**: handler-level driver, one resolver, `call` children, idempotency-keyed identity, ingress-watch removal | — | deletes the serial branch and the capability flag with working wiring |
| §3 execution authority | reconstructs grant/scope/env in the child | **owns**: the durable child request | — |
| §4 commit vs cancel | honours the committed-final rule before publishing a settlement | **owns**: the fenced disposition, four sinks, late-completion refusal | — |
| §5 rank and intent order | per-leaf coordinator shared | **owns**: recorded source-slot discharge | journaled observation prefix |
| §6 projection incorporation | child-side capture of possession/trigger facts | **owns**: carriage and recovery | **owns**: incorporated prefix and rebuild-before-replay |
| §7 closing | — | **owns**: close recovery, retained work, close timeout | ruling-3 supersession on the product path |
| §8 rank authority | — | **owns**: retention of ids, results, consumed prefix | **owns**: reattachment, one rank authority |
| §9 segments and bounds | — | — | **owns**: reattachment across segments, admission bound |
| §10 aggregate laws | — | — | **owns**: typed early/full replies, classification |
| §11 value model | — | — | **owns**: race/any acceptance and the value laws |
| §12 Durable Wait child | — | — | **owns**: existing Durable Wait routing preserved |
| §13 usage | child-side capture and attribution | retained across recovery | **owns**: deduplication and final accounting |
| §14 native tier | **owns**: owned supervisor, retained contexts | — | — |

Two tickets sit beside these and are not owners of any clause above: **FIG-3395**
authors the executable aggregate oracle and freezes the pre-cutover baseline, and
**FIG-3400** proves actual parallelism on every producer. **FIG-3398** measures;
its two headline numbers are winner latency and finalization latency, separately
(§7).

---

## Open questions for Sam

These are contradictions or gaps this note found and deliberately did **not**
decide. Each carries the options and a recommendation.

### Q1. `Promise.race([])` — refusal, or a never-settling program?

ECMA-262 says `Promise.race([])` returns a forever-pending promise. ADR 0065
refuses empty groups, and gives a good reason ("a never-settling program that must
not become an unbounded durable await"). ADR 0062 says "Nothing is accepted with a
nearby meaning", and a refusal is not a nearby meaning — it is a visible gap. But
ADR 0064's deviation register is described as "small and closed", and this would
add to it.

- **(a)** Register the refusal as a named deviation with its own `TS_*`
  diagnostic. **Recommended** — it is a real divergence from ECMA-262, and the
  register exists precisely so divergences are named rather than discovered.
- **(b)** Accept it as an unbounded durable await. Rejected here: it makes a
  program that can never make progress indistinguishable from one that is merely
  slow.
- **(c)** Resolve it locally as never-settling without reaching the host. This is
  (b) with extra steps — the turn still never completes.

### Q2. Does a `Cancel`-disposition group's close still protect a committed final?

ADR 0065 lets a group declare `LoserPolicy::Cancel` at open, and says
`close_effect_group(Cancel)` "returns once one idempotent cancel per unsettled
child has been durably issued … and does **not** await confirmed loser
termination". §4 of this note says a committed final is exempt from cancellation
and must drain. The two are consistent only if "unsettled" excludes "committed but
not yet drained" — which no current text says.

- **(a)** Read "unsettled" as excluding a committed final, and say so in ADR 0065.
  **Recommended** — it preserves ADR 0042's protection on every disposition, and
  the deadline arm's purpose (do not let a losing arm run on) is met by cancelling
  the *uncommitted* arm, which is the case that actually runs on.
- **(b)** Let `Cancel` drop a committed final's intents. This is B-prime's loss
  reintroduced through the disposition, and it would make a deadline arm's
  presence decide whether a recorded trigger exists.

### Q3. Where does the close deadline's value come from?

§7 requires a close timeout that suspends or fails finalization with retained
work. Nothing says who sets it. ADR 0023 makes retention a parameterized host
lever; ADR 0025 makes segmentation thresholds controller-construction-time input.

- **(a)** Controller construction-time input, like the segment budget.
  **Recommended** — the controller is the only party that knows its backend's real
  bounds, and this matches `segment_effect_budget`'s existing shape.
- **(b)** Host policy on the session, like the commit budget.
- **(c)** A fixed constant. Rejected: the native and Restate tiers have nothing in
  common here.

### Q4. Is the admission bound on outstanding work per group, per opener, or per session?

§9 requires admission of "aggregate width AND retained outstanding work, nested
groups included". "Nested groups included" implies the bound is not per group.
Nothing decides its scope.

- **(a)** Per logical opener, counting nested groups. **Recommended** — the
  resource being bounded is the opener's retained state and its command headroom,
  both of which are per opener.
- **(b)** Per session. Rejected: it makes one process's behaviour depend on a
  sibling's.
- **(c)** Per group only. Rejected: it is exactly the bound the width-two hung-loser
  scenario defeats.

### Q5. Does `docs/design/` exist as a documentation location?

`docs/agents/way-of-working.md` states that `docs/` "holds ADRs (`docs/adr/`) and
the agent runbook you are reading (`docs/agents/`)" and that a design spec belongs
on the ticket or in an ADR. That sentence is already stale — `docs/store-sql-authoring.md`
is a non-ADR prose document in `docs/` — and this note adds a second exception in a
new subdirectory, at the path FIG-3392 names.

- **(a)** Accept `docs/design/` as a named location for contracts too long for an
  ADR, and update `way-of-working.md`'s routing table to say so. **Recommended**,
  and what this PR does for the enumeration sentence only; the routing table is
  left alone pending your call.
- **(b)** Move this note into ADR 0065 as a very long amendment. Rejected here: it
  would roughly double that ADR and mix a decision with its implementation
  contract.
- **(c)** Keep it on the Linear ticket. Rejected: FIG-2266, FIG-3396 and FIG-3397
  cite it from their bodies and it has to be readable at HEAD, offline, which is
  `way-of-working.md`'s own test for "belongs in the repo".

### Q6. Which census row carries `TS_ASYNC_MAP_SEQUENTIAL_V1`?

The Test262 harness accepts `registered-deviation:` as a skip reason and nothing
used it. There is no upstream feature tag for "async array callbacks", so this PR
adds a `typescript`-kind row named `async-array-callbacks` — the same row kind the
dialect's own decisions use — which means extending the generator's list as well
as the two data files.

- **(a)** A `typescript`-kind row, as done here. **Recommended** — it is the only
  row kind that exists for a dialect decision with no upstream tag, and `skip` with
  a `registered-deviation:` reason is the only status that can carry a deviation.
- **(b)** Flip an existing `accepted` row (e.g. `async-functions`) to `skip`.
  Rejected: async functions really are accepted, and the row would then lie about
  the larger feature to record a narrower deviation.
- **(c)** Leave the deviation in prose only. Rejected: the ticket asks for the row,
  and the census is the index that makes "every gap is a ruling" enforceable.
