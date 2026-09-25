# 0103: Code cells replay by re-execution on every host

## Status

Accepted 2026-09-23 (FIG-3549). Implemented. Amended 2026-09-23 (FIG-3586):
a nested effect's identity is its issue ordinal, not its call site; see
[Issue-ordinal identity](#amendment-fig-3586-issue-ordinal-identity).
Amended 2026-09-24 (FIG-3587): a redrive runs against the surface its live
pass saw — the cell's journaled binding set and the iteration's journaled
prompt surface — and any recorded effect's replay hash conflict parks the
turn; see [Journaled surface](#amendment-fig-3587-journaled-surface).
Amended 2026-09-25 (FIG-3719): the engine, not the frontier read, answers
whether a drifted binding's effect is served, so a positional journal
(Restate) serves a recorded result too.
Amended 2026-09-25 (FIG-3680): a command the journal holds nothing for refuses
only writes under the run's namespace, so an orchestrating call that journaled
no nested effect redrives by serving its presentation.
Amended 2026-09-25 (FIG-3725): a group tool child judges its own tool where it
runs; see [Group tool children](#amendment-fig-3725-group-tool-children).

Amended 2026-09-24 (FIG-3669), **not yet implemented**:
[ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md)
makes Restate the only effect engine and the SQL stores storage only. This ADR
specifies SQL-engine behaviour: the journal-row hosts (`StoreEffectReplayDriver`
over `EffectReplayRowStore`) and the replay-hash park on every SQL host;
re-execution on replay and the park stay, as engine obligations. Those passages
stay as written until the PR that deletes the code (FIG-3667, FIG-3668, or
FIG-3600 for the session lease) rewrites them.

## Context

A code cell (`RuntimeEffectCommand::ExecCode`) does two things. It returns an
`ExecResponse` to the turn, and it changes the interpreter: globals and
deferred-resolution records that the turn's final commit snapshots into
`checkpoint.components.execution_state`.

Restate already treated the cell as a direct local call. It re-ran the cell on
every handler replay, and the cell's nested LLM, tool and durable effects
answered from their own journal entries. The journal-row hosts (SQLite, and
PostgreSQL through the same `StoreEffectReplayDriver`) did something else. They
journaled the cell as a row and, on redrive, served the recorded `ExecResponse`
without running the interpreter. The response was right and the interpreter was
empty. A crash between the cell and the turn-final commit, followed by a
redrive, committed a fresh interpreter (135 B, `globals: {}`) where the live pass
held 706 B (`globals.result` plus deferred link keys). Nothing reported the loss.
After a successful commit, the same redrive fails the receipt check with
"retried with different commit content".

Two designs close the gap. The first journals the interpreter-state delta in
the `ExecCode` outcome and applies it on replay. The second re-executes the cell
on replay everywhere, as Restate does. The first adds a second representation of
interpreter state (a delta format beside the snapshot the commit already takes),
a replay hook in every code executor, and a second replay model beside
Restate's.

## Decision

**A code cell is never journaled on any host. Replay re-executes it.** Every
nested effect the cell issues (LLM calls, tool attempts, durable effects,
journaled language-runtime values such as `Date.now()` and `Math.random()`) is
still journaled on its own replay key and served from the journal on replay.
Re-running the cell against those answers rebuilds the interpreter state
deterministically.

- `RuntimeEffectCommand::replays_by_reexecution` names the rule. It is true for
  `ExecCode` only.
- `StoreEffectReplayDriver` answers such a command by calling the local
  executor directly: no claim, no row, no lease, and strict replay mode does not
  apply because there is no row to find. Before the call it deletes any
  ungrouped row already at the command's address (see Upgrade). The "serve the
  recorded `ExecResponse` and skip the interpreter" path is gone.
- Restate keeps `ExecCode` as `DirectLocal`. The journal-row driver refuses a
  grouped `ExecCode`: a group child settles through its row, and this path
  writes none.
- A re-run cell re-incorporates the settlements it incorporated live, which
  refills the turn's checkpoint message buffer. A checkpoint served from the
  journal is the authority for everything enqueued before it, so the turn
  discards that refill when the checkpoint replays. Without this, a later live
  checkpoint would deliver the messages a second time.
- The portable law `effect_controller_code_cell_replays_by_reexecution` runs on
  SQLite, PostgreSQL and Restate. It requires that the cell runs on the live
  pass and again on redrive, and that its nested effect runs once. The RLM
  end-to-end laws in `lash-restate`'s `tool_context_conformance` require that a
  redrive after a crash at the turn-final commit commits execution state
  byte-identical to a clean live pass, with no provider call re-issued.

### Determinism contract

Re-execution is only as sound as the cell is deterministic given its journaled
inputs. The TypeScript dialect ([ADR 0096](0096-typescript-is-the-sole-rlm-dialect.md))
already enforces this for Restate: `Date.now()`, `new Date()` and
`Math.random()` lower to journaled operations, and the lowering rejects
`crypto.randomUUID` and locale- or timezone-dependent `Date` methods. Host tool call
*results* reach the cell only through journaled tool attempts. That covers
results, not bindings: which tool a call path names — its manifest and
contract — was read from the live registry at link time until FIG-3587, and is
now the cell's journaled binding set (see the amendment). Interpreter
globals are ordered maps (`BTreeMap`), so the snapshot does not depend on
insertion or hash order. The contract is now the same on every host, not a
Restate-only obligation.

One input is not journaled, and it can make a re-run differ from the live
pass:

- **The wall-clock bound on active VM time.** A cell that ran close to the
  limit live can exceed it on a redrive, which usually runs on a loaded or
  recovering node, and the reverse can happen too. This affects borderline
  cells, not only runaway ones. The instruction and memory bounds are
  deterministic.

The cell's link-time host environment was a second one: the ambient tool
surface a cell binds came from the live registry when the cell re-ran, so a
tool source or manifest that changed between the crash and the redrive made
the re-run bind differently or fail to link, and the turn failed closed on a
hash conflict it could not recover from. FIG-3587 journals it (see the
amendment).
A third input, the compiler's call-site identities, used to reach every nested
effect's replay key: the key embedded the AST-path node id and occurrence of
the call site, so a deploy that changed lowering moved the keys and a redrive
missed the journal and issued those calls again. FIG-3586 removed it from every
key (see the amendment below): identity is the issue ordinal, and the compiler
cannot move it. FIG-3587 covers a redriven cell linking against a drifted live
tool surface: the cell links against its journaled binding set, and a call
that would reach a drifted tool live refuses typed and parks.

### Upgrade

No persisted shape or journal vocabulary changes. A pre-cutover in-flight turn
redriven by this build re-runs its cell over the nested rows it already
journaled, which is the recovery this ADR is for.

An `exec_code` row written by an earlier build is not inert on its own. A
*completed* one is never served. An *in-progress* one, left by an old-build
worker that crashed mid-cell, would count against every quiescence read on its
scope (the queue-drain end, `WhenQuiescent` retirement) forever, because
nothing claims or finalizes an `ExecCode` row any more. So the re-executed
command deletes any ungrouped row at its own address before it runs
(`EffectReplayRowStore::discard_reexecuted_row`). The re-run that would have
reclaimed the row under the old build is the one that removes it. Rows whose
turn is never redriven again stay until their scope retires; a scope that is
never redriven owes no quiescence read. The durable-read fixture pins the
completed case: its pre-cutover `exec_code` row is never served, and replaying
the envelope re-runs the executor.

Rollback: an older binary replaying a journal this build wrote finds no
`exec_code` row. In strict replay that is a missing-row refusal; outside strict
replay it runs the cell live and journals it. The FIG-3586 grammar cutover
follows the current clean-cutover policy: this build refuses pre-cutover
journals fail-closed with a typed error before any effect, and a pre-cutover
binary run against journals this build wrote is not supported for now — its
call-site keys miss every v2 row, and nothing on the new side can make it
refuse. The policy is temporary. The stamps stay readable on old durable
state — a sync outcome without `cell_replay_grammar`, a `ProcessStarted`
without `replay_grammar`, and `LASHLANG_REPLAY_KEY_GRAMMAR_VERSION` itself —
so a later migration or drain can identify pre-cutover journals by them
before refusing.

## Consequences

- One replay model for code cells across Restate, SQLite and PostgreSQL. Any
  deployment that runs on the shared driver inherits it.
- A redrive does the cell's local compute again. Nested effects are not
  re-issued: a redrive that issues the recorded commands in the recorded order
  is served from the journal on any build, and one that does not is refused
  before anything is dispatched (FIG-3586).
- Values the cell mints without journaling (an `await_handle` call id, measured
  durations) are minted again on a re-run, so live stream events from a redrive
  can carry ids and durations that differ from the live pass. They are not
  committed state.
- A running cell reads as quiescent between its nested effects. A stale worker
  keeps running its cell until its next nested claim or its commit is fenced.
- On the journal-row hosts, a code cell no longer holds a lease row while it
  runs. Two redrivers of the same turn could run the cell's local compute
  concurrently. Each nested effect is still claimed and fenced one at a time,
  and turn ownership is the session execution lease's job, as it is on Restate.
- A retired scope no longer refuses the cell itself. It refuses the cell's first
  nested effect.

## Amendment (FIG-3586): issue-ordinal identity

Amended 2026-09-23. Applies to code cells and to lashlang process bodies, which
also replay by re-execution.

**Identity is the issue ordinal; the compiler cannot move it.** Each command
that leaves the VM through `ExecutionHost::perform` toward the effect host — a
resource operation (a `typescript.runtime` value included), a whole aggregate,
a sleep, an await of a handle, a trigger, and in a process body a signal wait
or event — takes the run's next issue ordinal `k` when it is issued, before
anything can fail in the bridge. Host-injected sub-effects (attempts after the
first, retry sleeps, the timers' admission, deferred-tool resolution) take
none. Every key the command writes lives under that ordinal, in one namespace
per run (replay-key grammar v2, `LASHLANG_REPLAY_KEY_GRAMMAR_VERSION = 2`):

```
cell namespace     P = {exec_replay_key}:lk2
process namespace  P = lashlang:v2:{opener_scope}:lk2
command            P:{k:010}                  a journaled value
aggregate group    {scope_id}:group:P:{k:010} under the opener's group prefix (ADR 0099)
tool attempt       P:{k:010}:attempt:{a}      retry sleep P:{k:010}:attempt:{a}:sleep
tool await         P:{k:010}:await
aggregate child    P:{k:010}:child:{i}        timers' admission P:{k:010}:timers-admitted
sleep              P:{k:010}:sleep
signal wait        P:{k:010}:signal
handle await       P:{k:010}:process:await:{process_id}
seal               P:~seal
call_id            lashlang:v2:{opener_scope}:{k:010}   child calls append :child:{i}
```

The ordinal is zero-padded so byte order is ordinal order, and `~` sorts after
every digit, so `[P:, P:~seal]` is the run's whole key range. No key atom comes
from compiler output, bytecode layout, argument content or the live
environment: `node_id`, occurrence, the aggregate instruction pointer,
`batch_id` and the live-resolved tool operation left the key and stay trace
and graph metadata only. A process body's ordinals ride its segment state
(`LASHLANG_SEGMENT_STATE_VERSION` bumped); `call_id` no longer moves under a
benign compiler change, so the subagent session and process ids and signal
ids derived from it stop re-minting.

**Every hit is checked against the whole envelope.** A served entry is compared
as today (kind, tool and operation, args, grant, sleep spec, group membership,
`call_id`). An aggregate's group head is checked too: its `batch_id` is a
content digest, and the reopen compares every offered child to the retained
one (ADR 0099, FIG-3586 amendment), so an aggregate whose arguments drifted
refuses before any child is claimed.

**Recorded-frontier fence.** Before a run's first command reaches the host the
run reads its recorded journal once:
`RuntimeEffectController::read_recorded_journal` over `[P:, P:~seal]`. The
SQL hosts answer with the replay keys in that range and the group keys in the
same range under the opener's `{scope_id}:group:` prefix, read back without it
(PostgreSQL's
key columns are `COLLATE "C"` so the range and the seal's order match SQLite's
byte order), and the seal's outcome. From then on each command is admitted
against that snapshot:

- recorded at `k` as the same shape: replay;
- recorded at `k` as another shape (a scalar call replayed as an aggregate):
  refuse at `k`;
- nothing at `k` but anything recorded beyond it, or a seal: the command may
  run, but its first journal write under the namespace, or one that names no
  key (a group open, a proxied process command), is refused
  (`CommandJournalGuard`), so nothing is dispatched. A keyed write outside the
  namespace is the host's to judge, as under the key fence: an orchestrating
  call whose body issued no nested effect, or a call settled in preparation,
  wrote only its presentation (`{call_id}:present`), and the redrive serves it
  from the journal (FIG-3680; the law
  `an_empty_orchestrating_call_redrives_from_its_cell` runs on SQLite,
  PostgreSQL and the Restate server double);
- nothing at or beyond `k`: the run is live from here.

A command the journal records as having written that writes nothing on the
redrive also refuses when it closes. A crash mid-run (no seal) replays the
recorded prefix and takes over the in-progress entry under ADR 0042, then runs
live. Restate answers the read as `Positional`: its SDK replays the journal by
position and compares each run's whole `RunCommandMessage`, name included, so
a re-keyed or reordered run is `JOURNAL_MISMATCH` (570) before anything is
dispatched. That positional name check, not deployment pinning, is why Restate
never had this exposure (ADR 0043 records that Restate does not enforce the
pin); the text above that credited pinning was wrong. Restate's nested `Sleep`
and `AwaitEvent` are not runs and are covered only by the stamp below.
A journaled controller that cannot answer the read refuses with
`RecordedJournalReadUnsupported`; a local controller has nothing recorded.

**Seal.** When a cell's executor returns a response (a setup failure included,
a controller abort not), the cell journals `P:~seal` as its last nested effect:
its envelope is `issued={count}:dispatched={digest}`, the count of commands
issued and a digest of which ordinals wrote. A redrive of a completed cell must
meet an identical seal, which refuses a run that ended early and one that no
longer writes a command it wrote. The seal's outcome carries the producer
(compiler version, VM ABI, module ref), served back into divergence messages
for attribution and never compared (ADR 0043). A process body journals no
seal: its terminal is the registry's, and it closes by refusing when the
journal holds a command at or beyond the last one it issued.

**Refusal.** A divergence is `lashlang_cell_replay_divergence`
(`LashlangCellReplayDivergence`); a replay mismatch any command meets at a
recorded entry — a hash conflict, a group-head content mismatch — is re-typed
to it with the key and attribution. Both it and
`lashlang_cell_replay_key_format_cutover` are `is_replay_mismatch()` and
classify as `Parked`. A refusal stops the run uncatchably: it becomes the
execution's nested error and cancels the run's cancellation scope, so
`try { await a() } catch {}; await b()` never dispatches `b` after `a`
diverged. The turn aborts without committing and **parks**: its claims stay
held, a typed parked-turn record (its reason the divergence or the cutover) is
kept, and `LashCore::drain_status` counts it. It is never failed and never
retried live; every redrive refuses again with zero dispatch. The operator
redeploys the build that wrote the journal, cancels the turn, or forks it from
a cell prefix. A process segment that refuses fails its run instead of
committing.

**Cutover.** A journal written under v1 cannot be read by v2, and v2 does not
re-derive legacy keys. The execution-environment sync outcome carries
`cell_replay_grammar`, stamped from the code executor on every successful sync;
a cell whose iteration's served sync names another grammar, or none, refuses
with `lashlang_cell_replay_key_format_cutover` before it runs. A process's
`ProcessStarted` record carries `replay_grammar`, inherited across segments; a
body whose start record names another grammar refuses with the same code
before running, and a parked segment written under the old state shape is
refused by the `LASHLANG_SEGMENT_STATE_VERSION` bump with its existing remedy.
Both over-refuse only work in flight across the one cutover deploy, and both
fail closed, before any effect. Under the current clean-cutover policy there
is no migration, compatibility reader or legacy-key re-derivation yet; the
grammar stamps are what one would key off (see Upgrade).

## Amendment (FIG-3587): journaled surface

Amended 2026-09-24. A redrive runs against the surface its live pass saw: the
model prompt it was asked under and the tool bindings its cells linked
against, both served from the journal, never re-read from the live registry.

**The prompt surface is journaled at every iteration.** Every
`SyncExecutionEnvironment` — the protocol-start one included, which used to be
host-only — builds the environment the iteration's model call is built from
(system prompt, tool specs, projector inputs, FIG-3538) and journals it; the
machine installs whatever the sync returns. This holds for every protocol that
syncs its environment (Standard and RLM do). A live fault rebuilding the
environment — a store, lease or session fault — is not the sync's outcome:
its claim is released unsealed, the same retry authority an uncommitted
assistant-response derivation has, and the turn aborts as a live fault, so a
redrive rebuilds the environment instead of replaying a failed turn. A
deterministic refusal is still journaled and fails the turn. A redriven iteration's model call
is therefore built from the journaled surface, so a tool removed or
redescribed since the live pass no longer changes the recorded `llm_call`
envelope. `update_machine_config` is gone from the effect, the command and the
turn checkpoint (`TURN_CHECKPOINT_SCHEMA_VERSION` 10).

**A cell journals its binding set before its first effect.** When a cell
references host call paths, its executor journals one language-runtime value at
`{exec_replay_key}:cell-tool-bindings` (operation
`cell_tool_bindings:v1:{referenced paths}`): each referenced ambient path and
the full `ToolDefinition` of the tool the catalog bound there, or `null` for an
unbound path. Paths a deferred resolution records are left to that journal. A
redrive is served the record and compares it with the live catalog: a recorded
tool whose id the registry no longer holds is *missing*, and one whose live
definition differs in what decides how a call links and dispatches — id,
name, bindings, activation, argument projection, retry policy, input and
output schema, output contract — is *changed*. A reworded description or new
examples are not drift: they only reach the model's prompt, which the redrive
serves from the journal. The cell links against the live catalog with every
recorded path replaced by what the record holds whenever the two differ — a
binding drifted, a path the live pass found unbound is now bound, or another
tool now claims a recorded path — so a drifted binding keeps its recorded
manifest and contract and the cell links exactly as the record does.

**A drifted binding is served only from the journal.** A call on a drifted
binding is authorized under its recorded definition
(`ToolCallAuthorization::Recorded`: recorded manifest and contract, identity
preparation, no grant in the envelope — the catalog call it replays carried
none), so its attempt envelope is the recorded one. An orchestrating tool the
registry still holds (`agents.spawn`, for one) orchestrates as the catalog
call it replays did: its body re-runs, prepared by its live provider, against
its recorded nested effects (a started process and its await, which the
frontier read now reads as the call's own rows). The command is served only:
every *dispatching* effect it issues under its run's namespace — a tool
attempt, a retry sleep, a nested effect — carries the drift refusal to its
engine on the local executor (`RuntimeEffectLocalExecutor::served_only`), and
the engine answers whether the journal holds its outcome (FIG-3719). An
engine that serves a recorded outcome never runs the executor; one that would
run it live — no outcome recorded, or an in-progress entry after a crash
mid-dispatch — refuses with `lashlang_cell_binding_drift` instead, naming the
binding, its tool and whether it is missing or changed, and records nothing.
The SQL driver asks its row store for a settled row before it claims; Restate,
whose journal is positional and answers no key read, learns it as its replay
reaches the run: a recorded `ctx.run` is served without running its closure,
and a closure that does run is the live frontier, so it signals the refusal
and never completes, and the run proposes no result. A budget give-up entry
for the effect is the frontier the same way. Nothing may be journaled after
such an orphaned run in the same attempt: the next replay would await it
while commands remain (`JOURNAL_MISMATCH`); the refusal stops the run, no seal
follows a nested error, and the park is written through the session store.
Restate's served-only effects that act outside a `ctx.run` closure — process
commands, which reach Restate through their own journaled commands (a
drifted orchestrating binding's process start, for one), and timers — cannot
tell a recorded outcome from a live one before they act, so they refuse up
front: such a command parks even when its process command or sleep was
recorded, and never starts a process or journals a sleep. A process command
reaches the engine served only on every route — including the one that
proxies it through an effect task to the raw controller, which Restate's
owned turn and group-child controllers take; before FIG-3725 that route lost
the mark and started the process live. A command a keyed
journal holds nothing for refuses before it issues anything; on a positional
journal the command enters and its engine refuses at the live frontier. A
wait on an external completion (a deferred tool's `{cmd}:await`) dispatches
nothing and carries no refusal. A command the journal does not hold as issued is refused as the
run's divergence first (`lashlang_cell_replay_divergence`). The code classifies `Parked` and the turn
parks as for a divergence (`TurnParkReason::BindingDrift`): nothing is
dispatched, nothing terminal is written, and every redrive refuses again until
the tool is restored, the turn is cancelled, or it is forked.

Limitations, deferred:

- A non-orchestrating provider whose preparation is not the identity would
  prepare a call differently than the recorded identity preparation does;
  such a call diverges instead of replaying.
- An orchestrating tool the registry no longer holds cannot re-run its body,
  so a call on it parks even when its nested effects all settled.
- On Restate, a drifted command whose effects include a process command or a
  timer parks even when those were recorded (they refuse up front, above).
  FIG-3779 carries the frontier-marker design that would serve them.

**Cell journal grammar 3.** `LASHLANG_CELL_JOURNAL_GRAMMAR_VERSION` (3) is the
replay-key grammar plus the binding set, and is what the sync's
`cell_replay_grammar` stamp now names. A cell whose sync names 2 — journaled
without a binding set — or none refuses with
`lashlang_cell_replay_key_format_cutover` before it runs. Process bodies
journal no binding set and stay on `LASHLANG_REPLAY_KEY_GRAMMAR_VERSION` 2.
Under the current clean-cutover policy there is no migration; the stamp stays
readable.

**Any recorded effect's hash conflict parks.** The binding set and the
journaled prompt remove the known drifts; the backstop covers the rest. A
replay hash conflict on any recorded effect on the SQL hosts
(`sqlite_effect_replay_hash_conflict`, `postgres_effect_replay_hash_conflict`)
classifies `Parked` instead of recording a failed turn (superseding FIG-3575's
outcome reading for these codes). On Restate the same divergence, and a
reopened effect group whose child or shape is not the recorded one, refuses
with the engine-neutral `effect_replay_divergence`, which parks the same way
(it replaced `worker_replacement_abort`, a live fault, with no alias). The
turn handler fails that attempt retryably, so the invocation keeps its
journal, and bounds its attempts before the invocation pauses
(`lash_restate::turn_service`). A parked turn issues no further journaled
effect: its journal diverged at the refusal, so the teardown's orphan repair,
whose cancel gate peek is journaled, is skipped, and its inputs stay pinned
to the turn id its redrive carries. The mismatch report names the diverged
effect's kind (its command `type`: `llm_call`, `tool_attempt`, …) as
`effect_kind`, and the park records it
(`TurnParkReason::EffectReplayDivergence { effect_kind, message }`), so an
operator can tell a model-call drift from a tool or cell drift. A turn that
parked on a drifted model call finishes once the surface is restored: the
redrive replays the recorded call and the commit clears the park. A content
checked group reopen whose shape drifted parks under `effect_group`.

Old state never reaches a replay: the SQL hosts reject and recreate a store
written before this amendment at open (SQLite durable core 83, PostgreSQL
component 123). On Restate, a journal written before it holds a
protocol-start sync envelope with `update_machine_config`, so its first sync
is refused, typed and before any effect, as `effect_replay_divergence`, and
the turn parks. The conformance law
`model_call_drift_parks_then_completes_once_restored` runs on SQLite,
PostgreSQL and Restate (an in-process invoker over the real endpoint).
`redriven_cell_links_against_its_journaled_binding_set` runs on SQLite,
PostgreSQL and the Restate server double: each tier's runner cuts the first
attempt at the journal point the law names (FIG-3665), and on the double a
drifted binding whose result was recorded replays it while one needed live
refuses (FIG-3719).

## Amendment (FIG-3725): group tool children

Amended 2026-09-25. A tool call that runs as a child of a durable effect
group — a model-issued call of a turn's tool group, a leaf of a cell's
aggregate — judges its own tool where it runs, and a drifted one is served
only from the child's journal, under the same engine-answered contract as a
cell's drifted binding.

**The child judges, not the opener.** The child's request already records
the definition its opener admitted it under (its catalog manifest, and the
contract the recorded tool surface holds for it, FIG-3712). At execution the
child judges that definition against the catalog the serving deployment's
registry resolves to under the child's recorded tool access and subagent
context, by the one FIG-3587 rule (`ToolSurfaceDrift::judge`). A drifted
child's controller serves every dispatching effect it issues only from its
journal (`ServedOnlyRange::every_key`); reading its recorded environment,
presenting its result and recording an incorporation dispatch nothing and pass.
The judgment cannot sit with the opener: a turn tool group reopens under its
retained membership, so a verdict a redrive put in the offered request would
never reach the child that runs; on Restate the group open is a journaled call
whose parameter carries the membership, so a changed request is a
`JOURNAL_MISMATCH` exactly where the recorded result should replay; and a
Restate child is retried by the engine, on whatever deployment serves it,
independently of its opener's redrive. The opener prepares a drifted call
under its recorded definition, so the child it offers is the recorded one; it
no longer refuses before the group opens, and an aggregate naming a drifted
binding no longer refuses at admission. The request's bytes do not change. A child whose recorded surface holds no
definition for its own tool fails closed: it is served only from its journal.

**The first refusal is the last write.** Once any effect under a guarded
command or child was refused, the guard refuses every later write — an
effect, a group open, a proxied process command — with the same refusal
before it reaches the engine. On Restate a refused run is an orphaned
`ctx.run`, and anything journaled after it (a later nested call of an
orchestrating body, its `admit_semantic` call) would fail the next replay
with `JOURNAL_MISMATCH`; with the guard, every retry replays up to the orphan
and refuses the same way.

**A refused child records nothing and parks its turn.** A child whose effect
the engine would run live refuses with `lashlang_cell_binding_drift` and
settles nothing: sealed, the refusal would be served to every later redrive,
even one after the tool was restored. On Restate the child ends its attempt
the way a park ends (FIG-3697), after writing its turn's
`ParkReason::BindingDrift` through the session's own store
(`park_turn_of_refused_group_child`, keyed by the turn scope's root turn): its
opener is suspended on the child's rank and cannot learn of a refusal that
settles nothing. The opener waits; every retry of the child refuses and parks
again, until the tool is restored — the child then runs and the opener
resumes — or the turn is cancelled, which closes the group. A child whose
group its opener already closed or retired (a race or any loser, after the
opener's end) parks no turn: only its own attempt ends. A child whose scope
names no turn — a process opener, a queue drain with no attributed turn — has
no park to write, so it does not retry unseen: a drift refused at the live
frontier settles as the child's typed outcome, which its opener reads (a
process segment fails its run). On the SQL tiers the child's claim is released
unsealed and the opener's rank wait answers with the refusal, so the turn
parks through its own abort path; a reopen of the group in the same process
dispatches the refused child again, judged anew, even while its siblings
still run. A trigger the body emits still ingests its occurrence (keyed,
idempotent, outside the journal by design, FIG-806); its deliveries are
served only.

The law `a_group_tool_child_judges_its_own_drifted_tool` runs on SQLite,
PostgreSQL and the Restate server double: a model-issued call and an
aggregate's leaves whose results were recorded are served with nothing
dispatched; calls needed live park, keyed by their turn, with the binding
drift and nothing dispatched, and park again on every further run; restoring
the tool finishes the turn; a reworded tool runs live. `lash-restate-test`'s
`tool_child_drift` laws redeploy under a child in flight on the double, so its
next attempt runs on a rebuilt context: a plain call parks until the tool is
restored, a drifted orchestrating body's second nested call is refused before
the engine and every retry parks with no journal mismatch, and a drifted
orchestrating body starts no process.
