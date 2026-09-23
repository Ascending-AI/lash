# 0103: Code cells replay by re-execution on every host

## Status

Accepted 2026-09-23 (FIG-3549). Implemented.

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

Three inputs are not journaled, and each can make a re-run differ from the live
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
- **The compiler's call-site identities.** A nested effect's replay key embeds
  the AST-path node id of its call site. Within one build those keys are
  stable, so a re-run reaches the same keys in the same order. Across a deploy
  that changes lowering or AST paths, the keys move: a redrive misses the
  journal and, outside strict replay, issues those nested LLM and tool calls
  again. Restate does not share this exposure, because Restate pins an
  in-flight invocation to its deployment and the SQL hosts have no such pin.
  Before this ADR the SQL hosts served a completed cell's recorded response,
  so the window was only a crash mid-cell; it now covers any redrive of a turn
  whose cells completed before the crash.

Both are follow-up work: FIG-3586 makes a redrive across a compiler change
refuse rather than re-issue a cell's nested effects, and FIG-3587 covers a
redriven cell linking against a drifted live tool surface.

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
replay it runs the cell live and journals it.

## Consequences

- One replay model for code cells across Restate, SQLite and PostgreSQL. Any
  deployment that runs on the shared driver inherits it.
- A redrive does the cell's local compute again. Within one build, nested
  effects are not re-issued; across a deploy that moves call-site identities
  they can be (see the determinism contract).
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
