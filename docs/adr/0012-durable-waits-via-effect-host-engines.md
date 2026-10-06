# Durable waits use effect-host engines and engine-owned journals

## Status

Replaced by [ADR 0132](0132-durability-is-state-first-over-the-lash-store.md)
§6 and [ADR 0003](0003-keyed-promise-is-scope-agnostic.md). Code on main still
cites this file; the Restate deletion lane removes that code, deletes this
file and moves its citations to ADR 0003.

## Note

This decision gave long-lived work one engine-owned keyed promise,
`AwaitEvent`, with deadlines replayed from an engine journal. In the end state
a durable wait is a wait row in the lash store: a kind, an owner actor, an
owner scope and a deadline written once at creation, resolved by its first
winner (ADR 0132 §6). Signals, process joins and timers are wait kinds of that
one mechanism. ADR 0003 owns the wait contract shared by turns and processes.
