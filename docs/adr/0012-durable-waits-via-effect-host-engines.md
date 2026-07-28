# Durable waits lean on effect-host engines; substrates own their journals

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
The inline substrate owns no replay journal. The session commit store does not absorb
this responsibility: effect replay and settled session history remain separate seams
joined by stable operation identity.

As originally accepted, this ADR said that lash never journals effect outcomes itself,
while both SQL substrates already did. The code had diverged from the record, and the
record was wrong. FIG-655 corrected it to the contract/substrate split above rather
than preserving the rejected claim as settled history.

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

- The inline implementation provides an in-memory wait over the process registry: a
  waiting process holds a parked future and keeps its lease alive, and replay does not
  survive loss of that runtime. Long-lived automation that requires cross-process
  replay belongs on a substrate configured to own it.
- `EffectReplayOwnership` records only the mechanical fact of whether the runtime or
  its controller owns replay. It is not an end-to-end durability claim. The Host
  Application owns that deployment-level assertion.
- Signals are named and typed only: declared per-process as event types with payload
  schemas, validated at send time; the unnamed untyped `wait_signal()` is removed.
- Waiting is an observability facet on a running process (wait state on the record,
  mirrored by waiting/resumed events), not a fifth lifecycle status — terminal/lease
  semantics are untouched.
