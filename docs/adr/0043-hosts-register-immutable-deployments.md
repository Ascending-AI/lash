# Hosts register immutable deployments

## Status

Retired: the assumption it served, that a deployment's code stays immutable
while any invocation may replay against it, is deleted with journal replay. No
code re-runs against a recorded history
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §2), so
changing kernel code never needs the old deployment kept registered or
drained. Generation routing, the journal prefix, the build-generation sentinel
and `JOURNAL_LOGIC_EPOCH` go with Restate. Durable formats still drain under
[ADR 0106](0106-durable-formats-upgrade-by-migration-or-drain.md) §1. Code on
main still cites this file; the Restate deletion lane removes that code and
deletes this file.
