# 0142: A whole-node crash in a cell's first slice may re-send one model call

## Status

Accepted.

## Context

A model response that asks for a code cell can commit with that cell's first
save, rather than requiring a separate `model.done` commit before execution.
This saves one owner commit in a one-park cell turn: six untraced or seven
traced, versus seven or eight with an eager response commit. Holding the
response opens a window between receiving it and committing the first save.

## Decision

### 1. The first save carries the response

The turn holds the response's checkpoint writes while a fresh cell runs its
first slice. Its first save commits those writes, the cell snapshot and its
effect admissions atomically as `model.done+cell.snapshot+admit`.

The hold is bounded: `model.done` commits on its own when the cell outlasts
its first slice, before waiting on effects, at a restored checkpoint or when
no worker is ready. A cell that answers without saving carries the response
to the commit that consumes its answer. A turn that owes an earlier answer
commits the response separately.

### 2. Whole-node death may cost one extra provider call

If the whole node dies during the first slice, before any commit holds the
response, recovery finds the model call's admission and pinned request, but
no response checkpoint. It re-sends that request for the same turn while the
recorded deadline allows, under the `Repeatable` model-call rule of
[ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §4.

One such crash costs at most one extra provider call. Its response may differ
and may ask for a different cell. This is a bound per crash in this window,
not a promise of one extra call across repeated node deaths. At the host's
`Provider` seam, the provider sees a second request for the same turn; the
host must allow for its cost and a different answer.

A worker crash leaves the node alive. When the cell attempt returns an error
while the response is held, the turn attempts a standalone `model.done`
commit before propagating the failure. If that commit succeeds, retry
re-delivers the cell without asking the model again. Whole-node abort or
power loss runs no failure handler, so it cannot take this path.

### 3. Committed effects remain exactly once

No tool body starts before the cell save commits its admission. Death before
the first save therefore loses no executed tool work from that cell. Recovery
loads committed tool outcomes without re-running their bodies; a started
`Once` call without an outcome becomes `Interrupted`, and an unfinished
`Repeatable` call follows its recorded policy (ADR 0132 §5).

The turn retains exactly-once committed effects, epoch fencing and its
idempotent terminal commit. These guarantees require acknowledged database
commits to survive the failure (ADR 0132 §13). Cell stream observations and
traces stay held until the response commits, so the replacement response's
cell is the one the report and host observe. Streaming model text follows
the attempt-reset rule of [ADR 0040](0040-retried-model-attempts-retract-live-text-by-correlation.md).

## Rejected alternative

Committing `model.done` eagerly before entering every cell closes this
first-slice window once the response is committed. It adds a standalone
commit to every cell turn instead of paying for one extra provider request
only when a node dies in the window. The bounded hold accepts that rare crash
cost to save the routine commit, while preserving committed tool work.

## Evidence

The hold and worker-failure fallback are in
`crates/lash-core/src/runtime/durable/phases.rs:469` and `:570`. The bounded
slice and first-save paths are in
`crates/lash-vm-broker/src/kernel/run.rs:524`, `:554` and `:596`; the atomic
save and admission fence are in
`crates/lash-vm-broker/src/kernel/store.rs:658`, `:886` and `:912`.

`crates/lash-durable-test/tests/cell_restore_laws.rs:2083`
(`a_re_sent_call_replaces_its_cell_on_sqlite_memory`) cuts the first save with
whole-node abort and checks that only the replacement cell's bodies, report
and observations survive. The counted one-park law is in
`crates/lash-durable-test/tests/support/turn_commit_bound.rs:115`.
