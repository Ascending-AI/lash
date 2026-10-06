# ADR 0130: Protected realization runs in its own invocation

## Status

Replaced by [ADR 0132](0132-durability-is-state-first-over-the-lash-store.md)
§5. Code on main still cites this file; the Restate deletion lane removes that
code, deletes this file and moves its citations to ADR 0132.

## Note

This decision ran a protected final's intent realization in a separate
Restate invocation, because a Run's positional journal could not interleave
realization commands with its schedule. In the end state there is no journal
to interleave: the store half of realization commits in the transaction that
records the tool result, so it is exactly once (ADR 0132 §5). Intent
identities and their exactly-once fences are unchanged, and protected drain
order stays under
[ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md).
