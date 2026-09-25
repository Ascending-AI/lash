# Durable waits lean on effect-host engines; substrates own their journals

Amended 2026-09-24 (FIG-3669), **not yet implemented**:
[ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md)
makes Restate the only effect engine and the SQL stores storage only. This ADR
specifies SQL-engine behaviour: the SQLite and PostgreSQL substrates' effect
journals (`runtime_effect_replay`) and their await-event promise rows; the
keyed-promise contract stays, as an engine obligation. Those passages stay as
written until the PR that deletes the code (FIG-3667, FIG-3668, or FIG-3600 for
the session lease) rewrites them.

Long-lived processes need to suspend durably (waiting on a signal, a long timer, or a
child process) without holding a worker. We close this by growing the effect-host
contract by exactly one primitive — a durable, one-shot, keyed promise
(`AwaitEvent { key }` plus a resolve seam) — and leaning on whatever engine implements
the contract correctly (Restate today, Temporal or others tomorrow) for suspension
economics. All richer wait semantics (named typed signals, child-process joins, timer
wakes) are lash-defined compilations onto that one primitive with deterministic,
occurrence-sequenced keys.

Lash owns the effect-journal contract; the configured substrate owns the journal.
Restate supplies its native journal. The SQLite and PostgreSQL substrates implement
the same contract in `runtime_effect_replay` and `lash_runtime_effect_replay`.
Every substrate journals: FIG-3585 deleted the journal-less native substrate. The session commit store does not absorb
this responsibility: effect replay and settled session history remain separate seams
joined by stable operation identity.

Amended by ADR 0065 (FIG-1416): the "exactly one primitive" claim above is now
"exactly one *wait* primitive". Durable effect **groups** add a second, distinct
primitive — a structured set of independently journaled children with a durable
wake rule and a settlement order that is a journal fact. Groups are the
composition *above* attempts and add no new command variant, so the reasoning
here about keyed promises and their occurrence-sequenced keys is unchanged; only
the claim of singularity is narrowed. See ADR 0065 for the group contract and its
normative obligations.

As originally accepted, this ADR said that lash never journals effect outcomes itself,
while both SQL substrates already did. The code had diverged from the record, and the
record was wrong. FIG-655 corrected it to the contract/substrate split above rather
than preserving the rejected claim as settled history.

## Restate deadline and cancellation replay shape

A deadline-bearing Restate wait journals one absolute Unix-epoch deadline before it
calls `LashDurableWaitWorkflow/await_resolution`. Replacement workers reuse that
journaled value and derive the timer's remaining duration from it. Journaling the
absolute deadline preserves the caller's one total budget; recomputing a relative
`timeout_ms` would both extend the budget and change the nested call payload that
Restate compares during replay. Deadline wire version 2 is a clean cutover from the
unversioned `timeout_ms` field: predecessor payloads are refused, and deployments
with deadline-bearing waits must drain them before upgrading. No-deadline requests
retain their existing bytes and command geometry.

`observe_turn_cancel` is also part of the Restate journal contract. It chooses between
one durable-wait call and a wait-plus-gate command sequence, so the value must be
reconstructed identically for the entire invocation. It is not journaled separately:
doing so would alter every existing no-deadline wait even though shipped park flows
already derive the flag from stable turn scope. Any future caller that cannot prove
that stability must journal the choice before emitting either shape.

## Considered Options

- **Process-event-log journal**: record effect outcomes into the process event log and
  replay from it, giving uniform suspension on every backend and demoting engines to
  transports. Rejected: it duplicates the effect-journal contract inside an
  observation log and fuses process history to one replay implementation. SQL-backed
  implementations of the effect-host contract are substrates, not this rejected
  second journal.
- **Per-semantic contract growth** (`AwaitSignal`, `AwaitProcessTerminal`, …): rejected —
  the probability of a correct third-party engine implementation falls with contract
  surface area; one promise primitive is the smallest thing an engine must get right,
  and new wait flavors then cost zero contract change.

## Consequences

- Every wait is durable on the configured substrate. The inline implementation's
  in-memory wait over the process registry, whose replay did not survive loss of the
  runtime, was deleted with the native substrate (FIG-3585).
- `EffectReplayOwnership` records only the mechanical fact of whether the runtime or
  its controller owns replay. It is not an end-to-end durability claim. The Host
  Application owns that deployment-level assertion.
  *(Superseded: FIG-2226 made this the one sync `effect_journaling()` fact, and FIG-3585 deleted that fact because every host journals.)*
- Signals are named and typed only: declared per-process as event types with payload
  schemas, validated at send time; the unnamed untyped `wait_signal()` is removed.
- Waiting is an observability facet on a running process (wait state on the record,
  mirrored by waiting/resumed events), not a fifth lifecycle status — terminal/lease
  semantics are untouched.
