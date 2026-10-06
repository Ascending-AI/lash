# Bounded journals are an effect-controller obligation

## Status

Retired: segment journal budgets, journal-budget cuts and segment handovers are
deleted with Restate, and no decision replaces them. A process has no journal
to bound. It persists state as phase rows and VM snapshots under
[ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §5 and §8, so
nothing grows with its effect count. Code on main still cites this file; the
Restate deletion lane removes that code and deletes this file.

The intent admission budget this file stated is owned by
[ADR 0116](0116-tools-are-opaque.md) §1.7.

### 5. The handover and its requirements

Retired with segment handovers. A process actor resumes from its last
committed state on whichever node claims it (ADR 0132 §3).
