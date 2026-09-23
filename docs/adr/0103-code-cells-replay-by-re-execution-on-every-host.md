# 0103: Code cells replay by re-execution on every host

## Status

Accepted 2026-09-23 (FIG-3549). Implemented. Amended 2026-09-23 (FIG-3586):
a nested effect's identity is its issue ordinal, not its call site; see
[Issue-ordinal identity](#amendment-fig-3586-issue-ordinal-identity).

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
*results* reach the cell only through journaled tool attempts. Interpreter
globals are ordered maps (`BTreeMap`), so the snapshot does not depend on
insertion or hash order. The contract is now the same on every host, not a
Restate-only obligation.

Two inputs are not journaled, and each can make a re-run differ from the live
pass:

- **The wall-clock bound on active VM time.** A cell that ran close to the
  limit live can exceed it on a redrive, which usually runs on a loaded or
  recovering node, and the reverse can happen too. This affects borderline
  cells, not only runaway ones. The instruction and memory bounds are
  deterministic.
- **The cell's link-time host environment.** The ambient tool surface a cell
  binds and links against comes from the live registry when the cell re-runs,
  not from the journal. If a tool source or manifest changes between the crash
  and the redrive, the re-run can bind differently or fail to link. Its
  `ExecResponse` then differs from the live one, and the next journaled
  envelope conflicts: the turn fails closed and cannot recover.
A third input, the compiler's call-site identities, used to reach every nested
effect's replay key: the key embedded the AST-path node id and occurrence of
the call site, so a deploy that changed lowering moved the keys and a redrive
missed the journal and issued those calls again. FIG-3586 removed it from every
key (see the amendment below): identity is the issue ordinal, and the compiler
cannot move it. FIG-3587 covers a redriven cell linking against a drifted live
tool surface; under the ordinal grammar that drift is a typed divergence at the
first affected entry, never a live dispatch.

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
  run, but its first journal write is refused (`CommandJournalGuard`), so
  nothing is dispatched;
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
