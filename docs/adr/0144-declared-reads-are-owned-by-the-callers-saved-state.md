# 0144: Declared reads are owned by the caller's saved state

Recovery and external-effect guarantees follow [What survives a crash](../../README.md#what-survives-a-crash).

## Status

Accepted. [ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §9
covers projection reads and declared tool reads in code cells; ADR 0132 §7
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

Projection reads (ADR 0132 §9) already work this way. They are answered
inside the statement that makes them, are not journaled, and survive only in
the heap the next snapshot saves.

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

### 2. Only code cells read

A `Read` tool's provider registers a reader beside itself
(`ToolProviderRegistration::with_reads`). The reader gets the manifest the
run's surface retained, the arguments, the run's configuration, a
cancellation and a deadline; it gets no attempt context, completion client,
intents or call identity. A provider that advertises a `Read` tool without a
reader is refused at registration, and a read never falls back to `execute`.

A model round's call of a `Read` tool stays recorded, and so does a cell's
call that is not planned as a read (§3). On the recorded route a `Read` call
that was interrupted runs again at its ordinal, and a reported failure is
never retried.

A workflow process never reads inline: its `perform` of a `Read` tool is a
linked effect, admitted and executed as a durable process step like any
other, because a step's result must stay stable across replay and only a code
cell, which reruns as a whole, may read again.

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
which may catch it. Both dialects lower to the same `perform`, and
projection reads keep their handle interface.

### 5. The runtime holds reads to a cell's bounds

A read has no admission, wait, `x_start`, `x_outcome` or `round.traced`. It
counts against the cell's `max_tool_calls` with its calls, in the count the
cell's saved state holds. It is bounded by the sooner of its tool's
execution bound and how long the worker waits on its parent's answer, and is
cancelled with the cell's run. It is never retried. Each read emits an
advisory observation (cell, provider, tool, site, its ordinal among the
cell's reads, elapsed time, argument and answer sizes) that is not a record.

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

The policy and its refusals are in `crates/lash-sansio/src/tool_contract.rs`;
the reader interface in `crates/lash-core-execution/src/tool_reads.rs`; the
plan in `crates/lash-protocol-rlm/src/executor/mod.rs` (`read_candidates`)
and `crates/lash-kernel-check/src/closure.rs` (`main_effects`); the cell's
reads in `crates/lash-protocol-rlm/src/executor/reads.rs`; the start save in
`crates/lash-vm-broker/src/kernel/run.rs`; the kernel binding in
`crates/lash-kernel-vm/src/machine/actions.rs` (`perform_read`). The laws are
in `crates/lash-durable-test/tests/cell_read_laws.rs`.
