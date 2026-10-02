# 0125: Model usage is engine-owned accounting delivered per call

## Status

Accepted (FIG-4236). ADR 0099 §13, ADR 0100, ADR 0104 §3, ADR 0105 §9,
ADR 0109 §4, ADR 0112 §8, ADR 0119, ADR 0031 and ADR 0032 each state their
part of this decision in a model usage accounting section pointing here.

Sam's ruling binds this decision:

- A durable accounting continuation is owned by the model-call execution, and
  its obligation exists before anything is dispatched.
- Each provider attempt's identity is recorded before dispatch, and its result
  when it is available. Settled accounting is projected into SQL idempotently,
  then acknowledged.
- Root cancellation, refusal, parking, fork or loss never cancel accounting
  delivery. The delivery payload is retained until projection is acknowledged.
  Session deletion drains accounting first.
- There is one writer for ordinary, direct, compaction, tool-child and process
  calls, including failed attempts and corrections. The turn-end drains and
  the duplicate tool-settlement charging go.
- SQL accounting never advances the head and never needs the drive fence.
- Facts are keyed by owner, stable effect, provider attempt and fact kind. An
  identical retry is a no-op, a conflicting payload is a typed conflict, and a
  correction has its own identity.
- The guarantee is 100% of durable accounting evidence, with an explicit
  unknown liability for every charge the journal could not describe. Provider
  reconciliation happens only where the provider can look a charge up.
- Accounting facts survive journal poison substitution.

## Context

Until now a session's usage was a resident ledger. Every call site recorded
into a shared in-memory list, and the list was staged into the next runtime
commit: the turn's final commit, a host graph write, a park, a compaction's
settle, or a dedicated usage-ledger boundary. A tool attempt carried its
nested calls' usage on its settlement, and the opener charged it again at
incorporation.

That model lost money in every ending that is not a commit:

- A root that was refused, cancelled by an operator, forked while parked, or
  lost with its substrate never reached the commit that would have carried its
  calls.
- A runtime that died between a call and its commit took the resident rows
  with it.
- A body that Restate re-ran after an unrecorded fault paid the provider twice
  and recorded once.
- Deleting a session deleted its ledger rows with it.

The commit also made usage depend on the head and the drive fence. That tied
billing evidence to conversation authority it has nothing to do with.

## Decision

### The model

- **Owner.** A `RuntimeOwner`: `Session(s)` or `Process(p)`. This is the
  runtime the spend is attributed to. The dispatch site names it, and one run
  has one owner. Until FIG-4215 deletes them, a process runtime's calls are
  owned by its synthetic sessions `process-env:{pid}` and
  `process-session-turn:{pid}`.
- **Spending effect.** A journaled `LlmCall`, `Direct` or `ToolAttempt`
  effect: the effects whose body may dispatch a provider call. It is named by
  `UsageEffectKey`, the effect address's graph key. A tool attempt's nested
  direct completions never journal on their own; they are calls of the
  attempt's run.
- **Run.** One execution of a spending effect's body (`UsageRun`, a minted
  `UsageRunId`). It is admitted as one SQL `usage_runs` row before the body's
  first provider attempt, and never admitted if the body dispatches nothing.
  A read records admission evidence only when admission wrote it. A settlement
  can create a resolved row without admission, and never invents its scope,
  source or model attribution. Its state is `Open` or
  `Resolved { at_ms, outcome }`. A conflicted outcome keeps the typed fact
  identity and both payload hashes in SQL columns. Repeating the same
  resolution preserves its timestamp.
- **Fact.** One provider attempt of one call of the recorded run, or one
  correction of such an attempt. Its identity is
  `(owner, effect, call_ordinal, provider_attempt, kind)`. `LlmCallId` rides
  on the fact as trace attribution only, because it is not unique: every
  session direct call is `"{session}:direct"`.
  `UsageFactRecord.body` is `Attempt { run, outcome }` or
  `Correction { usage, generation_id }`. Kind and reporting disposition derive
  from that body. A correction keeps its attempt's attribution and replaces
  the body as one value; it has no run and always has a generation id.
- **Liability.** The run row. It is `open` until a settlement names it,
  `settled` when its facts land, `unknown` when it provably dispatched and no
  journaled result will ever describe it, and `conflicted` when its settlement
  disagreed with stored facts.

Invariants:

- **I1.** No provider attempt is dispatched without an admitted run. This is
  structural: `ProviderHandle::complete_prepared` takes a
  `&dyn DispatchAdmission`, and the kernel's `UsageCall` is the only
  production admission. The standalone host `DirectLlmClient`, which no lash
  execution owns, passes `<dyn DispatchAdmission>::host_owned()`: the host
  owns that call's billing.
- **I2.** Only one run of a spending effect is ever journaled. Its settlement
  settles it and resolves every other open run of the effect
  `unknown(superseded_run)`.
- **I3.** A fact identity maps to one payload forever. A repeat is a no-op; a
  difference is a typed conflict that appends nothing.
- **I4.** Accounting writes touch only `usage_facts`, `usage_runs` and
  `usage_owner_retirements`. They never read or write the head, the drive
  fence, receipts or root rows.
- **I5.** After an owner is retired no run of it is admitted, and a
  settlement of a run admitted before the retirement still lands.

### Before dispatch: the SQL run row

"Journaled before dispatch" is realized as the run's admission row. This is
the one non-literal reading of the ruling. Every provider attempt of a call
happens inside one recorded engine step (the retry loop in
`complete_prepared`), so no journal entry can sit between two attempts. A body
that Restate re-runs after an unrecorded fault also leaves nothing in the
journal at all. A journal entry written before the step would cover every run
of the step as one, and could not tell a paid, unrecorded run from the
recorded one. The SQL run row can: every run that may have dispatched has its
own row before dispatch, and a row no journaled settlement ever claims becomes
an explicit unknown liability.

The dispatch gate runs before every attempt. Each attempt re-checks the
durable owner-retirement fence through the run's idempotent admission, and
the run records each attempt ordinal it admitted. Only one run row is inserted:

- A retired owner refuses the dispatch with `TurnFailureCode::UsageOwnerRetired`,
  not retryable, and nothing is sent. Managed direct calls carry it as
  `RuntimeErrorCode::UsageOwnerRetired` through the existing
  `PluginError::Runtime` carrier.
- A store fault is not a refusal. The attempt is refused retryably, the run
  records the fault, and the engine ends the effect retryably **without
  journaling**, for all three kinds. This is FIG-3683's `Retried` rule applied
  to admission, even where the effect's faults are otherwise recorded.

### Beside the outcome: the recorded facts

When the body returns, `UsageRun::finish` yields the run's `EffectUsage`
(owner, run, facts, accounting), or `None` when no call was admitted. The
facts come from the sealed attempt records, one per attempt the gate
admitted:

- a `Reported` attempt with provider usage is a reported fact, zero included
  (ADR 0032: `Some(0)` is a fact);
- `UnreportedAfterAbort` and `UnreportedAfterFailure` are unreported facts,
  billed with the count unknown;
- `UnreportedByProvider` records no fact (ADR 0031);
- an admitted ordinal missing from the record is an unreported fact with no
  generation id.

A call admitted but dropped before its record was sealed (a cancellation
inside the provider handle) makes the accounting
`RunAccounting::CallWithoutRecord { calls }`, and the settlement resolves the
run `unknown(call_without_record)`.

`EffectUsage` is journaled beside the outcome, as
`RecordedRuntimeEffect.usage`, outside `outcome`. An `Err` outcome keeps its
spend. `EFFECT_JOURNAL_VERSION` is unchanged: the field is optional, and
in-flight invocations stay on their build (ADR 0105 §7).

### After the entry: the continuation

Right after the spending effect's entry, fresh or replayed, the controller
journals a one-way `settle` send to the Restate virtual object
`LashUsageAccounting`, keyed by the owner's `Display`. On replay the send is
matched, not sent again. No code runs between the entry and the send, and
only then is the outcome returned to the drive.

The object:

- is a **shared** lane (never split by generation; ADR 0111's namespace prefix
  applies);
- keeps no K/V state: SQL is the state, and the object exists for per-owner
  exclusivity and FIFO order;
- retries every handler forever (1 s initial interval, factor 2, 60 s
  maximum, `u64::MAX` attempts), so a projection waits out a database outage
  and is never paused or killed by policy;
- answers a request body version it does not read with a terminal error
  (`USAGE_ACCOUNTING_WIRE_VERSION`, drain policy), a store fault with a
  retryable error, and a fact conflict with `Ok`, the conflict marked on the
  runs in SQL;
- acknowledges by completing. Until then Restate holds the send's payload,
  which is the retention the ruling asks for, independent of the caller's
  journal.

`project_usage_settlement` is the one production writer of settlements. It
calls `settle_usage` in one transaction and, on a conflict, marks the runs
`conflicted` and answers `Projected::Conflicted`.

An engine that journals nothing (the local and test controllers) projects the
settlement directly when the body returns.

A run whose attempt ends with a fault after one of its calls was sealed is
never journaled: a tool attempt whose later direct completion cannot bind its
recorded model (ADR 0030) ends there, and the engine retries it under a new
run. Its sealed facts are known, so they are not left to become an unknown
liability. The executor projects them when the attempt ends, on every engine,
under the key of the effect and the run (`UsageEffectKey::for_unrecorded_run`),
and then resolves the run's admitted row, which carries no fact under the
effect's own key. The facts of the run the effect is later recorded with keep
the effect's key, so I2 and I3 hold: the two runs' facts never share an
identity. If the store refuses the projection the admitted row stays `open`
and is resolved `unknown` like any other run nothing settled (FIG-4632).

### Why delivery survives every ending

- The settle is a detached send journaled before the drive sees the outcome.
  Every later event (the root's commit, a refusal, a park, an operator cancel,
  a fork, a lost run, a deletion) happens after it is in Restate's log.
- A killed or cancelled caller does not recall a send.
- A paused (parked) invocation keeps its journal, and its sends are already
  out.

The one window left is an operator kill between the effect's entry and its
send. No code runs there, so only a kill fits. That run stays `open` until
`retire_execution` or the owner's drain resolves it
`unknown(execution_ended | owner_retired)`. The result is explicit, never
silent, and the provider was not called again.

### Journal poison

When an outcome cannot be journaled, the poison substitute replaces only
`outcome`. The record keeps its usage in three tiers:

1. the full `EffectUsage`;
2. if that does not fit, `usage.without_facts()`: the owner and run stamp stay,
   the facts are dropped, and the accounting becomes
   `FactsUnjournalable { dropped_facts }`, so the settlement resolves the run
   `unknown(facts_unjournalable)`;
3. if even the stamp does not fit, no usage. The run stays `open` until the
   execution or owner retirement resolves it.

The give-up proof measures its substitute with a maximal stamp (an owner of
256 bytes, a run id and a zero-fact `EffectUsage`), so "the poison fits"
stays a proof. The pre-flight `GaveUp` entry carries no usage: the body never
ran, so no run was admitted.

### Endings

| Ending | Accounting |
|---|---|
| Committed (completed, cancelled, failed stop), Refused, Parked | Nothing. Every settled call's send precedes it. |
| Operator cancel, fork of a parked root (`release_root`) | After the kill: `retire_usage_execution(Session(s), turn(s, root))`. |
| Lost root (`end_lost_root`), reconcile kill of a gone or terminal target | After the end or kill: `retire_usage_execution` for the root's turn scope. |
| Lost process run (`end_lost_run`) | After the end: `retire_usage_execution` for the process's owners. |
| Session delete | The deletion drain, below. |
| Process prune | `drain_usage_accounting` for `Process(pid)` and its synthetic sessions, before the journal retirement. |

`retire_execution` runs on the owner's object after every send the killed
execution issued (per-key FIFO), so it resolves only runs that can never
settle. It is scoped to the root's own turn scope: a run of a follow-on
physical turn of that root, and a group child that outlives its killed
opener, stay open until they settle or the owner is drained. A process whose
terminal segment is released is resolved at its prune drain.

Forking a session copies no usage. The child is a new owner; the parent keeps
its facts.

### The deletion drain

`SessionDelete`'s physical delete first calls
`EffectHost::drain_usage_accounting(Session(s))`, before any session state is
deleted. A failure is `SessionDeleteFailure::UsageAccounting`, retryable: the
relay's next attempt runs every step again (ADR 0109 §4).

The close's engine half killed the session's executions before its
acknowledgement armed the delete, so the drain reaches the owner's object
after every send they issued. The drain inserts the retirement row, and
admission is refused from then on (I5). A still-running group child is
refused before dispatch, typed `UsageOwnerRetired`. The remaining open runs
become `unknown(owner_retired)`. A settle that arrives later still lands, and
the facts outlive the physical delete until retention reclaims them.

### Lock order

Accounting writes take no session lock, head row or drive fence (I4).

- **SQLite** serializes every accounting transaction as an immediate write
  transaction.
- **PostgreSQL** serializes each owner's mutations (admission, settlement,
  correction, execution retirement, owner retirement) with a transaction
  advisory lock keyed by the owner. A row lock cannot do it: admission must
  serialize with a retirement row that may not exist yet, and `FOR SHARE`
  cannot lock an absent row. Every accounting writer also takes a shared
  accounting-writer lock, which the retention sweep takes exclusively, so a
  sweep never removes a retired owner's rows under an in-flight settlement.
  The public signatures carry none of this.

### Reads, reconciliation, retention

- `LashCore::owner_usage(owner)`, `usage_fact_page` and `usage_run_page` read
  by owner only. They open no session and drive nothing, and they answer for
  live, parked, refused, deleted-but-retained and pruned-but-retained owners.
- `LashSession::usage()`, `DurableSession::usage()` and `LashRuntime::usage()`
  are async durable reads of `OwnerUsage` for their session.
  `OwnerUsage::report()` renders the `SessionUsageReport`, keyed per
  `(source, model_key, requested_model)`: the recorded model key the call
  ran under and the wire model its request named, so two keys that share a
  wire model stay two rows (FIG-4405). A fact also keeps the served model
  the provider reported, and none where it reported none.
  `completeness.is_settled()` tells a host whether delivery is still pending. The sync resident `usage_report` and
  `unreported_usage_attempts` are gone.
- Visibility is eventual and explicit. A turn does not wait for its
  accounting: waiting would put the accounting call on the turn's path and
  make a kill of the turn a kill of the call.
- `reconcile_unreported_usage` reads the outstanding attempts from the store,
  so any host can reconcile. For each attempt with a generation id it asks the
  provider, and appends one `UsageCorrection` per recovered attempt through
  `append_usage_corrections`. A retried correction is a no-op. A conflicting
  or refused correction leaves the attempt `unresolved`. Attempts without a
  generation id and `unknown` runs are not reconcilable, because no provider
  in lash offers a lookup by lash's request id.
- The factory evidence sweep reclaims a retired owner's facts, runs and
  retirement row once the retirement is older than the host's horizon. A live
  owner's usage is never reclaimed.
- Lash has no spend policy. Completeness is exposed, and a host that gates
  spend on it decides.

### What is deleted

The resident shared ledger, its staging, confirmation and discard; every
commit's `usage_deltas`; `RuntimeUsageDelta` and its payload identity; the
usage-ledger semantic boundary; `ToolUsageDelta`, `ToolUsageLedger` and the
settlement charge at incorporation; the resident totals on
`RuntimeSessionState`, `SessionSnapshot`, `SessionWindowRead` and the runtime
observation; the `usage_deltas` and `usage_delta_holes` tables, replaced in
place by `usage_facts`, `usage_runs` and `usage_owner_retirements` under the
same schema version.

## Consequences

- 100% of durable accounting evidence reaches storage, and every other
  dispatch is an explicit `unknown` liability with a reason. No ending
  silently drops a charge.
- Usage is readable for an owner no runtime has open, including one whose
  session was refused, forked from, deleted or pruned.
- The fenced commit carries no usage, so a commit's size and identity are
  independent of how many calls the turn made.
- A reader right after a turn may see open runs. That is the honest answer
  while delivery is in flight.
- Each provider attempt checks admission before dispatch, and each recorded
  spending effect entry costs one send.
