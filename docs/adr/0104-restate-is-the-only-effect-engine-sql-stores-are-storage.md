# 0104: Restate is the only effect engine; SQL stores are storage

## Status

Replaced by [ADR 0132](0132-durability-is-state-first-over-the-lash-store.md).
Code on main still cites this file; the Restate deletion lane removes that
code, deletes this file and moves its citations to ADR 0132.

## Note

This decision made Restate the only effect engine behind the pluggable
`EffectEngine` seam, with the SQL stores holding storage only. In the end
state lash's own runtime is the only durable engine, persisting state through
the lash store, and the seam is deleted (ADR 0132 §1).

### 2. The effect interface is engine-neutral

Replaced by ADR 0132 §1: `Backend` builds the durable engine directly over its
store set. Durable formats are declared by the format registry under
[ADR 0106](0106-durable-formats-upgrade-by-migration-or-drain.md).

### 3. Engine obligations are contracts

Replaced by ADR 0132 §3 to §12. Their laws run the production runtime under
ADR 0132 §14.

### 4. Zero-infra is a local Restate server

Replaced by ADR 0132 §1 and §12: zero-infra is one SQLite database file with no
server process ([ADR 0102](0102-zero-infra-is-a-sqlite-in-memory-backend.md)
D4).
