# 0144: Declared reads are owned by the caller's saved state

Recovery and external-effect guarantees follow [What survives a crash](../../README.md#what-survives-a-crash).

## Status

Accepted. [ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §9
covers host data reads through declared tools in code cells; ADR 0132 §7
states their `Read` execution policy.

## Context

A recorded tool call in a code cell is its own admitted execution (ADR 0132
§5, §8); a planned `Read` follows §2 below. A cell parks at the `perform`, the broker commits the park with the
call's admission, the body runs between `x_start` and `x_outcome`, and the
cell is answered from the committed record. That is right for a call that
changes something: committed outcomes prevent re-execution, while an
unfinished call follows its execution policy. External behaviour requires
host deduplication on `call_id`.

A call that changes nothing pays the same price. A cell that looks up 50
values commits 50 admissions, 50 starts, 50 outcomes and their traced
exports, and parks 50 times. None of those records protects anything: the
answer is a function of what the provider holds when it is asked, and the
cell may as well ask again.

Host data reads use this same Read-tool mechanism (ADR 0132 §9). There is no separate projection-handle read path, and RLM has no transcript read ([ADR 0147](0147-an-rlm-cell-sees-its-frame-and-its-variables.md)).

## Decision

### 1. A read is owned by the saved state

A tool declares `ExecutionPolicy::Read`: its answer is not recorded; after a
crash before the next save it is fetched again and may differ. Nothing about
an individual read is durable. What survives of it is what the cell's next
saved state holds, and a crash before that save reads it again. There is no
per-read audit.

`Read` is not `Repeatable`. A `Repeatable` call is recorded and retried by
policy; a read is neither. A provider whose call must be recorded, retried,
deferred, isolated or must declare intents or end a turn declares `Once` or
`Repeatable`: a manifest that declares `Read` with any of those is refused.
A serialized policy this build does not know refuses to decode.

### 2. Code cells and workflow processes read

A `Read` tool's provider registers a reader beside itself
(`ToolProviderRegistration::with_reads`). The reader gets the manifest the
run's surface retained, the arguments, the run's configuration, a
cancellation and a deadline, plus the immutable run-history view for a cell; it gets no attempt context, completion client,
intents or call identity. A provider that advertises a `Read` tool without a
reader is refused at registration, and a read never falls back to `execute`.

A model round's call of a `Read` tool stays recorded, and so does a cell's
call that is not planned as a read (§3). On the recorded route a `Read` call
that was interrupted runs again at its ordinal, and a reported failure is
never retried.

A workflow process derives its read modes from its recorded environment's
operation routes (FIG-7099). A read-mode `perform` calls the reader registered
by that route's process component, without a session or a durable tool step.
An uncommitted stretch runs again from its last park and redraws its reads;
its next committed park keeps the resulting heap. Counted process segments
retain read answers and modes only as diagnostic tapes for explicit replay.
Live execution never consumes those tapes. Recorded operations still run as
durable process steps.

### 3. A cell plans its reads, and owes their providers

When a cell is lowered, its read plan is the set of effects its `main`
certifiably reaches whose catalog tool declares `Read`, with the provider the
run's surface retained for each. A dynamic target the closure analysis
cannot certify keeps its call recorded, and so does a deferred match. A
session in which a plugin sees tool arguments or results through a hook plans
no reads, since a read runs no hook. The protocol's own argument projection
is not such a hook: a read applies the same projection to its arguments, and
validates them against the tool's input schema, as a recorded call's are.

The plan is recorded in the cell's envelope. A cell with a plan saves its
start, with the plan, before any statement runs: one commit, which carries
the model response its turn holds (ADR 0142). Every parked root of the cell
owes the reader of each planned provider, as it owes the callbacks of its
grants, so only a node that serves those readers claims the cell. The debts
are retired with the root at the commit that consumes the cell's answer.

### 4. The kernel binds the mode, and the state keeps it

The machine binds each effect `Recorded` or `Read` at start, from the plan,
and the binding is part of the run's saved state: a restore cannot change
it. A read-bound `perform` does not yield and mints no wait. It is answered
inside its statement through the host, decoded against the effect's declared
result type, and charged exactly as a recorded `perform` is. A read the host
cannot answer at all (the parent's own failure) stops the run unanswered,
and its last save stands; the read's own failure is raised in the guest,
which may catch it. Both dialects lower to the same `perform`, and ordinary package operations are the only data-read interface.

### 5. The runtime holds reads to a cell's bounds

A read has no admission, wait, `x_start`, `x_outcome` or `round.traced`. It
counts against the cell's `max_tool_calls` with its calls, in the count the
cell's saved state holds. It is bounded by the sooner of its tool's
execution bound and how long the worker waits on its parent's answer, and is
cancelled with the cell's run. It is never retried. Each read emits
the ordinary `ToolCallStarted` followed by `ToolCallReturned`, marked
`execution: read`. No new semantic event type is introduced. Both readers call
`lash_core_execution::observe_tool_read`, the sole emitter of this pair; cell
and process execution remain separate. Model-round reads keep their existing
recorded execution path.

Each pair carries a `ToolCallId`, derived with one BLAKE3 hash from its caller
and one-based read ordinal, with a separate identity form from admitted calls.
Cell reads carry the enclosing code block's `graph_key`, exactly as recorded
cell calls do. Process reads travel on their existing process observation feed
in an `Activity` envelope. Read metadata names the caller, provider, ordinal,
site and origin; the events carry the tool id and projected arguments. The
return carries the ordinary success or failure output, including the read's
failure message, contract changes, input refusals, cancellation and timeout.
It also carries elapsed microseconds and JSON argument/answer byte sizes.
Unavailable arguments are null with size zero; an unavailable answer has size
zero. Hosts render these with their ordinary tool-call rows.

A read never emits `ToolCallCompleted`: nothing commits that call. Its events
are live and provisional (ADR 0122), never a durable record, transcript entry,
admission or commit. Existing bounded replay windows may retain them for
reconnect. After a crash between saves, an unsaved stretch runs again and its
read events can repeat under the same caller, ordinal and call id with a new
answer. A host can see events from a stretch that never commits. Correlate by
caller plus ordinal and reset provisional rows on a replay gap. Saved state
retains the read count, including across process parks; it retains no list of
read activities. Whether the next save should retain such a list awaits Sam's
ruling. The shared metadata and outcome pair can be collected at that save
boundary without changing either reader's event contract.

## Consequences

- A cell that reads 50 values commits once before it starts and once when it
  ends, whatever the reads answer.
- After a crash between saves, a read's answer may differ from the one the
  lost stretch saw. Recorded effects carrying the answer are admitted with a save that holds
  it. Printing, logging and advisory observations can expose an unsaved answer
  and may repeat or be lost.
- A cell's recorded writes are unchanged: they are admitted, run and
  answered as before, and a read never loses or duplicates one.

## Evidence

The policy and its refusals are in `crates/lash-vocabulary/src/tool_contract.rs`;
the reader interface in `crates/lash-core-execution/src/tool_reads.rs`; the
plan in `crates/lash-protocol-rlm/src/executor/mod.rs` (`read_candidates`)
and `crates/lash-kernel-check/src/manifest.rs` (selected linked reach); the cell's
reads in `crates/lash-protocol-rlm/src/executor/reads.rs`; the start save in
`crates/lash-vm-broker/src/kernel/run.rs`; the kernel binding in
`crates/lash-kernel-vm/src/machine/actions.rs` (`perform_read`). The laws are
in `crates/lash-durable-test/tests/cell_read_laws.rs`.
