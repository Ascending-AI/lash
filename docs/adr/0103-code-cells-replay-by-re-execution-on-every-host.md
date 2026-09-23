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
  apply because there is no row to find. The "serve the recorded `ExecResponse`
  and skip the interpreter" path is gone.
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
`crypto.randomUUID` and locale- or timezone-dependent `Date` methods. Host tools
reach the cell only through journaled tool attempts. Interpreter globals are
ordered maps (`BTreeMap`), so the snapshot does not depend on insertion or hash
order. The contract is now the same on every host, not a Restate-only
obligation.

One input is not journaled: the wall-clock bound on active VM time. A cell that
exceeds it live stops at a point the replay does not reproduce exactly. The
instruction and memory bounds are deterministic. This is the same exposure
Restate has always had. It is limited to runaway cells, which already fail.

### Upgrade

No persisted shape or journal vocabulary changes. An `exec_code` row written
before this change stays in the journal until its scope retires, and nothing
reads it. A pre-cutover in-flight turn redriven by this build re-runs its cell
over the nested rows it already journaled, which is the recovery this ADR is
for. The durable-read fixture pins this: its pre-cutover `exec_code` row is
inert, and replaying the envelope re-runs the executor.

## Consequences

- One replay model for code cells across Restate, SQLite and PostgreSQL. Any
  deployment that runs on the shared driver inherits it.
- A redrive does the cell's local compute again. Nested effects are not re-issued.
- On the journal-row hosts, a code cell no longer holds a lease row while it
  runs. Two redrivers of the same turn could run the cell's local compute
  concurrently. Each nested effect is still claimed and fenced one at a time,
  and turn ownership is the session execution lease's job, as it is on Restate.
- A retired scope no longer refuses the cell itself. It refuses the cell's first
  nested effect.
