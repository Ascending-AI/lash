# 0142: A cell's first save carries its turn's model response

Recovery and external-effect guarantees follow [What survives a crash](../../README.md#what-survives-a-crash).

## Status

Accepted.

## Context

A model response that asks for a code cell is committed raw, as the call's
receipt, under `model.answer` before any response hook runs or the cell
starts (ADR 0132 §4); only a crash before that receipt commits can require
another provider call. Its derived checkpoint, the turn advanced past the
call, still needs a commit. That commit can ride the cell's first save
instead of standing alone before execution. This saves one owner commit in
a one-park cell turn.

## Decision

### 1. The first save carries the derived response

The turn holds the derived response's checkpoint writes while a fresh cell
runs its first slice. Its first save commits those writes, the cell snapshot
and its effect admissions atomically as `model.done+cell.snapshot+admit`.

The hold is bounded: `model.done` commits on its own when the cell outlasts
its first slice, before waiting on effects, at a restored checkpoint or when
no worker is ready. A cell that answers without saving carries the derived
response to the commit that consumes its answer. A turn that owes an earlier
answer commits the derived response separately. When the cell attempt
returns an error while the derived response is held, the turn commits
`model.done` on its own before propagating the failure, so the retried
attempt re-delivers the cell without deriving it again.

### 2. A crash in the hold costs a derivation, not a provider call

If the node dies before any commit holds the derived response, recovery
finds the call's receipt in its phase checkpoint. It derives the response
again from the receipt: it replays the recorded stream through the recorded
stream hooks, runs the recorded response hooks and re-delivers the same
cell. The provider is not asked again. Response hooks are pure derivations,
invoked at least once.

### 3. Committed effect outcomes are never re-executed

No tool body starts before the cell save commits its admission. Death before
the first save therefore loses no executed tool work from that cell. Recovery
loads committed tool outcomes without re-running their bodies; a started
`Once` call without an outcome becomes `Interrupted`, and an unfinished
`Repeatable` call follows its recorded policy (ADR 0132 §5).

The turn retains committed-outcome recovery, epoch fencing and its
idempotent terminal commit. An external action before an outcome commit may
repeat under `Repeatable`; the host deduplicates it on `call_id`. These
guarantees require acknowledged database commits to survive the failure
(ADR 0132 §13). Cell stream observations and traces stay held until the
derived response commits.

## Rejected alternative

Committing `model.done` eagerly before entering every cell adds a standalone
commit to every cell turn. Once the receipt is committed, a crash in the
hold costs only a repeated derivation and cell start, so the routine commit
buys nothing a recovery needs.

## Evidence

The receipt commit and the hold are in
`crates/lash-core/src/runtime/durable/phases.rs` (`record_answer`,
`hold_response`). The bounded slice and first-save paths are in
`crates/lash-vm-broker/src/kernel/run.rs`; the atomic save and admission
fence are in `crates/lash-vm-broker/src/kernel/store.rs`.

`crates/lash-durable-test/tests/model_answer_receipt_laws.rs`
(`a_cell_killed_before_its_first_save_runs_the_paid_cell_again`,
`a_cell_killed_after_its_first_save_resumes_its_progress`,
`a_cell_that_never_saves_is_derived_again_from_its_receipt`) cuts the hold
and checks that the paid cell runs with one provider call. The counted
one-park law is in
`crates/lash-durable-test/tests/support/turn_commit_bound.rs`.
