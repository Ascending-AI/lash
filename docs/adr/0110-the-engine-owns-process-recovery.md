# 0110: The engine owns process recovery; lash never re-runs started work

## Status

Accepted and implemented 2026-09-27 (FIG-3588, the last child of arc FIG-3570).
It records Sam's ruling of 2026-09-23 on FIG-3588: once work is handed to the
engine, the engine owns its retries.

Supersedes [ADR 0019](0019-process-recovery-obeys-declared-disposition.md).
Amends [ADR 0027](0027-unleased-completion-carries-explicit-authority.md) (the
authority matrix), [ADR 0045](0045-services-are-stateless-substrates-own-continuation.md)
(its FIG-3588 amendment now holds on every tier),
[ADR 0094](0094-child-lifecycle-is-a-registration-fact-settled-by-scope-end.md)
(*Retry bound*) and [ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md)
(the process-execution obligation). [ADR 0042](0042-tool-attempts-are-atomic.md)
stands.

## Context

ADR 0019 made every registration declare a **Recovery Disposition**:
`Rerunnable` (another owner may re-execute the work), `OwnerBound` (once
started, abandonment is the only recovery) or `ExternallyOwned` (lash never
executes it). It existed because a host could run a tool whose effects no
journal recorded: re-running a started `shell.start` row would start a second
command. Around it grew a lash-side recovery apparatus: an attempt budget on
every row (`max_attempts`, `EngineGaveUp`), an owner-drain lever that abandoned
a host's own `OwnerBound` work at close, and a durable **Abandon Request** an
operator wrote so the sweep could abandon a row once its lease lapsed.

That reason is gone. Every host journals its effects (ADR 0102 D1, FIG-3585),
and Restate is the only effect engine (ADR 0104). A started process either
resumes by replaying its journal, or its journal is lost and it cannot be
resumed safely. The disposition no longer changes what recovery does, and the
apparatus around it no longer has a caller:

- Restate's segment admission (ADR 0045, FIG-3588 amendment) already refuses a
  fresh execution of any started segment as `ResumeRefused { SubstrateLost }`,
  whatever its disposition. Its root start binds attempt 1 and never consults
  the attempt budget.
- The only reader of `max_attempts` and of the `OwnerBound` rules was the SQL
  engine's native process worker, which FIG-3860 deletes.
- On the Restate tier, the Abandon Request's reconcile wrote the terminal at
  the next sweep: there is no lash lease to lapse.

## Decision

### 1. No recovery disposition

A registration declares no recovery disposition, and a record carries none.
`RecoveryContract` and its `Rerunnable`/`OwnerBound` distinction are deleted
from registration, records, observations, events, the remote protocol,
conformance, lash-sim and the facade.

What remains is one fact: **lash executes the process, or it never does**. It is
the input class. A `ProcessInput::External` process is externally owned: lash
never runs it, recovery never claims it, the engine never submits it, and only
its external owner closes it (`ProcessInput::is_externally_owned`). Every other
input is a process the engine runs.

The fact is derived, not declared, because a separate declaration admitted two
contradictory rows. An `External` input declared as lash-executed had no body
to run; an `Engine` input declared externally owned was a row nothing would
ever run. ADR 0019 rejected deriving the disposition from the input class
because two rows of one class (a `shell.start` call and any other tool call)
needed opposite recovery. That split no longer exists: every process lash
executes recovers the same way, so the input class is the whole fact.

### 2. Recovery is the engine's replay

A process lash executes, once started, resumes only by its engine replaying its
journal. A start that would run already-started work without that journal ends
the process `Abandoned` with `ResumeRefused { SubstrateLost }`, before any
effect. Lash never re-runs started work from scratch.

This is ADR 0045's FIG-3588 amendment stated for the interface, not only for
Restate. Restate implements it with the journaled admission verdict, the nonce
and the set-if-absent segment marker (`lash-restate` `process/admission.rs`).
Another engine must refuse the same starts at the same point.
`ResumeRefused { RetiredGeneration | SubstrateLost }` is the one "cannot resume
safely" terminal, checked before any effect, shared with the generation fence
of FIG-3571.

The registry records start facts; it never decides whether a start may run. An
identical start is idempotent, a successor execution the engine resumes takes
exactly the next attempt, any other attempt is refused, and an externally-owned
process never starts.

A run the engine finished without the process's terminal is the same loss.
An operator's kill ends a segment's run `409 killed`, and Restate cascades a
kill from the invocation that started the run. Restate runs a workflow key's
`run` once, so no redrive or sweep reaches the process again. The recovery
tick's park reconcile reads such failed runs back through the admin API and
ends each live process whose current segment's run it was `Abandoned` with
`ResumeRefused { SubstrateLost }`. The terminal transaction arms the
`ProcessTerminal` publication, so the process's waiters are served
(FIG-3890).

A boundary's write of its successor's external reference is part of the
boundary's journaled handover step. A store fault there fails the step and the
engine retries it. It is never logged and dropped.

### 3. Effect implementors own the unjournaled window

One window no journal covers remains: an effect ran, and its result was not
recorded. An effect implementor makes that window safe by idempotency keyed by
lash's stable call id. ADR 0042's at-least-once rule stands:
`ToolRetryPolicy::Never` means no retry after a reported failure, and there is
no at-most-once marker.

A process engine that is not lashlang (figments, for one) owes the same: it
journals its effects through the effect host, or it accepts `Abandoned` with
`SubstrateLost` when its substrate loses a started execution. There is no
compatibility path.

### 4. No lash attempt budget

Lash keeps no attempt budget for re-running a process. `max_attempts` is
deleted from registrations, records, start requests and declarations, and the
remote DTOs. So are the engine-child bound
(`RuntimeControlConfig::engine_child_max_attempts` and its execution-context
plumbing), the bound pinned in lashlang segment state and in the RLM snapshot
root, `ProcessStartOutcome::{AlreadyStarted, AttemptsExhausted}`, the matching
`PluginError` variants and the `ZeroMaxAttempts` registration refusal.

The engine bounds its own retries. On Restate that is the deployment's
invocation retry policy, which pauses an invocation at its ceiling, and a
paused segment parks (ADR 0104 O3). The call-site evidence is that Restate's
root admission binds attempt 1 and refuses any recorded start, so the budget
was never reached on the one engine.

### 5. Who writes `Abandoned`

`AbandonWriter` has two writers:

- `ResumeRefused { reason }`: lash refused to resume work it cannot replay.
- `Producer`: the process's producer recorded that its work was lost. This is
  the external owner of an externally-owned process, including an operator who
  acts for an owner that is gone, or a producer-declared `Abandoned` terminal
  event.

`OwnerDrain`, `Sweep`, `ReconciledRequest` and `EngineGaveUp` are deleted with
the paths that wrote them. `ProcessCompletionAuthority` has three variants:
`ExternalOwner`, `WorkflowKey` and `WorkflowKeyRecovery`.

### 6. The Abandon Request is deleted

The Abandon Request let a non-owner authorize an abandonment when lash could
not prove the owner gone: the sweep wrote the terminal once the owner's lease
lapsed. Once the engine owns recovery, no operator needs it:

- For a process lash executes, an operator who wants it stopped cancels it. A
  lost execution ends `SubstrateLost` inside the engine. On the engine tier
  there is no lash lease to lapse.
- For an externally-owned process, the marker authorized nothing its external
  owner cannot write. The owner, or an operator acting for it, closes the
  process through the external-owner completion (`complete_external`) with the
  outcome it has, `Abandoned { Producer }` included. Restate's reconcile was that
  same write, deferred to the next sweep.

It is deleted end to end: the `process.abandon_requested` event and its session
observation kind, the record and observation fields, the registry's
`request_process_abandon`, `ProcessTransition::RequestAbandon`, the
`ReconciledAbandon` authority, the sweep reconciliation on both tiers, the
facade levers (`SessionProcessAdmin::request_abandon`,
`Processes::request_abandon`) and the remote DTOs.

The native worker's graceful owner-drain lever (`drain_owner_bound_work`,
`ProcessDrainReport`) is deleted with `OwnerBound`: no started work is ever
abandoned by its own host at close.

### 7. No SQL reference substrate

The SQL engine's native process worker, with its lease-takeover recovery, is
deleted (FIG-3860). Restate's process workflow is the only recovery path: a
started segment resumes by replaying its journal or ends `SubstrateLost`.

## Consequences

- `ProcessRegistration::new`, `ProcessStartRequest::new` and
  `ProcessStartDeclaration::new` take no disposition. Host code that declared
  `ExternallyOwned` registers a `ProcessInput::External` input, which it
  already did; code that declared `Rerunnable` or `OwnerBound` drops the
  argument.
- Process record, registration and observation shapes, the first-start event
  payload, the process event vocabulary and the remote protocol DTOs change in
  place. The generated host schemas are regenerated.
- One conformance law holds the registry's side of recovery instead of
  per-disposition state machines: the store-contract state machine's
  attempt-monotonicity law, whose generated registrations are externally owned
  or executed. The engine's side is the Restate law
  `lash_restate::tests::substrate_lost` (a journal lost after a segment started
  never re-runs it). A killed process's re-invocation is also covered by
  `lash-restate-test`'s `substrate_lost_zombie`.
- The ADR 0027 matrix is two columns, executed and externally owned: the
  external owner closes only externally-owned processes, the workflow key only
  executed ones.
