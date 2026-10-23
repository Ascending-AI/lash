# 0103: Code cells replay by re-execution on every host

Recovery and external-effect guarantees follow [What survives a crash](../../README.md#what-survives-a-crash).

## Status

Replaced by [ADR 0132](0132-durability-is-state-first-over-the-lash-store.md)
§8. Surviving citations use this pointer to the current durability contract.

## Note

This decision rebuilt a code cell's interpreter state by re-running the cell
while its nested effects answered from an engine journal, keyed by issue
ordinal. Under that contract a cell resumes from its committed VM snapshot: the
snapshot, the broker ledger and the admission of every operation issued since
the previous snapshot commit together, and on restore each admitted
operation's saved outcome is fed back in. An operation with a committed outcome does not run again; an admitted
operation without an outcome follows its execution policy, and unsaved reads
and computation may repeat (ADR 0132 §2 and §8). Per-call binding stays with Run admission under
[ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md) §1.
