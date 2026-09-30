# 0103: Code cells replay by re-execution on every host

## Status

Accepted.

## Context

A code cell returns an `ExecResponse` and changes interpreter state that the
turn commits. Serving only a recorded response cannot reconstruct that state.
The cell therefore re-runs local computation while its nested effects answer
from their journal. The storage engine does not execute or replay the cell.

## Decision

### Code cells reconstruct state on replay

`RuntimeEffectCommand::replays_by_reexecution` is true for `ExecCode` only.
Restate classifies it as `DirectLocal` and runs the local executor on replay.
Its nested model calls, tool attempts and language-runtime values retain their
own journal identities. The executor restores execution state and deferred
resolution state through the same cell path on a fresh attempt.

A cell seals only a response that has no controller abort and is not cancelled.
A nested replay refusal stops further dispatch; a seal cannot hide it.
Replaying a checkpoint treats its recorded result as the authority for messages
already incorporated there, so a later checkpoint does not deliver them twice.

Evidence: `crates/lash-core-execution/src/runtime/effect/envelope.rs:571`,
`crates/lash-restate/src/controller/execution.rs:122`,
`crates/lash-protocol-rlm/src/executor/mod.rs:152`,
`crates/lash-protocol-rlm/src/executor/mod.rs:547`,
`crates/lash-core/src/runtime/turn_driver/effects.rs:260`.

### Issue-ordinal identity

Every command leaving the VM takes the next issue ordinal before the bridge
can fail. Replay keys derive from the run namespace and that ordinal, not an
AST node or bytecode address. Host-injected retry operations use subkeys of
the command. A cell namespace is `{exec_replay_key}:lk2`; a process namespace
is `lashlang:v2:{opener_scope}:lk2`. A command key appends a ten-digit,
zero-padded ordinal. The cell's final key is `P:~seal`.

A process segment carries its ordinal state forward. A cell seal records the
issued count and a digest of dispatched ordinals; it rejects a keyed replay
that ends before a recorded command. Producer attribution is diagnostic.

The run reads its recorded frontier once through `read_recorded_journal`.
A keyed controller validates recorded command shapes and refuses an unrecorded
write beneath the namespace when later work or a seal exists. A positional
controller answers `RecordedJournal::Positional`; Restate's journal and
canonical-envelope checks enforce its replay. A replay mismatch is a typed
refusal before fresh dispatch, not permission to run the missing work live.

Evidence: `crates/lash-lashlang-runtime/src/replay_run.rs:127`,
`crates/lash-lashlang-runtime/src/replay_run.rs:391`,
`crates/lash-lashlang-runtime/src/replay_run.rs:518`,
`crates/lash-lashlang-runtime/src/replay_run.rs:545`,
`crates/lash-lashlang-runtime/src/replay_run.rs:565`,
`crates/lash-lashlang-runtime/src/replay_run.rs:674`,
`crates/lash-restate/src/controller/mod.rs:990`.

### Journaled prompt and binding set

The execution-environment sync returns the prompt environment and tool catalog
as a recorded outcome. The drive installs that outcome on the live pass and
on replay. The live registry supplies executors; it does not replace the
recorded definitions used by the turn.

Before a cell's first effect, its executor journals every referenced ambient
path with its full tool definition or `null` for an unbound path. Deferred
resolution has its own record. On replay the cell links against the recorded
paths. It compares each recorded definition with the live dispatch contract:
identity, bindings, activation, argument projection, retry policy, schemas and
output contract. Descriptions and examples do not decide drift.

A missing or changed binding is served only. A recorded outcome can replay;
a dispatch that needs the live tool refuses `lashlang_cell_binding_drift`.
The engine answers whether it can serve the outcome. Once a guarded command
refuses, its later writes refuse too. A turn parks without a terminal result,
and a restored compatible tool can allow its replay to complete.

Evidence: `crates/lash-core/src/runtime/turn_driver/effects.rs:346`,
`crates/lash-core/src/runtime/turn_driver/handlers.rs:339`,
`crates/lash-lashlang-runtime/src/cell_bindings.rs:108`,
`crates/lash-lashlang-runtime/src/cell_bindings.rs:164`,
`crates/lash-lashlang-runtime/src/cell_bindings.rs:193`,
`crates/lash-protocol-rlm/src/executor/mod.rs:601`,
`crates/lash-restate/src/controller/journaled_effect.rs:314`.

### Group tool children judge their own tools

A tool child's request carries its recorded definition. The child judges that
definition against the registry where it executes, because the engine can retry
it independently of the opener. A drifted child serves dispatching effects
only from its journal. The opener offers the recorded membership rather than
injecting a live drift verdict into the group-open request.

A turn-scoped child's live-frontier refusal writes the turn's park and ends
its attempt without seating a settlement. Its opener waits. A closed or
retired group parks no turn. A binding-drift refusal without an attributed
turn settles as a typed child outcome. Tool bodies are opaque under
[ADR 0116](0116-tools-are-opaque.md); they do not issue nested controller
commands as orchestration bodies.

Evidence: `crates/lash-core-execution/src/runtime/effect/tool_child_driver.rs:978`,
`crates/lash-core-execution/src/runtime/effect/tool_child_driver.rs:1080`,
`crates/lash-core-execution/src/runtime/effect/tool_child_driver.rs:1363`,
`crates/lash-restate/src/effect_group/dispatch.rs:99`.

### Executable generation at admission

A turn's root admission records the executor's executable generation and
checks it before turn effects. The Lashlang cell generation hashes semantic
identity, bytecode generation, instruction accounting and cell-journal grammar.
An incompatible recorded generation refuses as `retired_generation` and parks
with the recorded generation. An absent stamp is accepted only when the
current executor also names none. This is distinct from the engine's
build generation, which routes journal-bearing handlers under ADR 0106 §1.

The pre-1.0 version freeze applies. Shapes change in place; version counters
and grammar stamps do not imply a compatibility reader for arbitrary builds.
[ADR 0106](0106-durable-formats-upgrade-by-migration-or-drain.md) and
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md) govern the
release boundary.

Evidence: `crates/lash-lashlang-runtime/src/replay_run.rs:82`,
`crates/lash-core/src/runtime/drive/root.rs:346`,
`crates/lash-core/src/runtime/turn_loop/generation_fence.rs:1`.

### Laws

`effect_controller_code_cell_replays_by_reexecution` requires two local cell
executions across live and replay passes and one nested effect execution.
The binding, tool-child and model-call drift laws require recorded results to
replay, fresh drifted dispatch to park, and restored tools to complete.
The laws use the controller/backend contracts. The current storage matrix is
SQLite file, SQLite memory and PostgreSQL; execution hosts are the Restate
server double, live Restate and lash-sim's in-process effect host.

Evidence: `crates/lash-conformance/src/conformance/effect_host.rs:595`,
`crates/lash-conformance/src/conformance/cell_binding_drift.rs:306`,
`crates/lash-conformance/src/conformance/tool_child_drift.rs:319`,
`crates/lash-conformance/src/conformance/model_call_drift_park.rs:203`.

## Rejected alternatives

Journaling an interpreter-state delta adds a second state representation and a
second restoration contract beside the committed snapshot. Re-execution uses
the same interpreter path. Compiler-derived effect keys let a compiler change
move durable identities; issue ordinals keep them tied to issued commands.
Linking against the live catalog changes a replay's dispatch contract; recorded
bindings preserve the contract and refuse a required fresh call that drifted.

## Consequences

Replay repeats local cell computation while recorded nested effects avoid
fresh dispatch. Observation ids and measured durations can differ between
attempts and cannot decide committed state. A mismatched generation, envelope
or binding parks rather than silently accepting a different execution. A
park requires compatible code or tool restoration, or an operator control
intent under the root's durable control contract.
