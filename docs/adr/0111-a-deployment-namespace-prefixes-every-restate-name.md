# 0111: A deployment namespace prefixes every Restate name lash binds or calls

## Status

Retired: Restate service names, their namespaces and the registration guard
are deleted with Restate, and no decision replaces them. Independent cores are
separated by their stores: each deployment binds its own store set
([ADR 0102](0102-zero-infra-is-a-sqlite-in-memory-backend.md) D2).
[ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) lists this
file in its scope. Code on main still cites this file; the Restate deletion
lane removes that code and deletes this file.

### 3. Keys stay scoped to their services

Retired with Restate service keys. Process start keys are store identities
under [ADR 0107](0107-a-process-is-named-by-a-minted-id-a-start-by-its-key.md)
§2.
