# 0131: Durable types declare their version surface

## Status

Accepted. Stored formats remain frozen until 1.0.

## Decision

A durable root declares `DurableRecord::SURFACE`. A recorded phase declares its
record type with `{Output, SURFACE, KIND}` and supplies an admitted identity:
a Run record, a wait row, a VM snapshot, a turn checkpoint or an engine state.
The store accepts that declaration rather than an independent string and serde
type. Each kind has one output type. Process completion and recovery have
different kinds; process command names carry no `:v1` suffix.

The format gate collects these implementations and follows their types through
Rust imports. Hand root lists disappear where ownership declarations reach
those roots. Explicit guards remain for DDL, encoders, hash domains, generic
wire families, and closures shared with another surface. Record kinds join
their surface signature. The surfaces a build decodes make up its format set,
which the claim filter of
[ADR 0106](0106-durable-formats-upgrade-by-migration-or-drain.md) §1 compares
with each actor's stored formats.

Typed raw carriers defer body decoding until the surface's read range admits
the stamp. They name the concrete payload type for closure discovery without
moving its Serde decode ahead of that check.

A typed plugin writer move records one validated map of plugin ranges with
the fleet epoch it accompanies. Suspended cell state declares the enclosing
code mode snapshot surface; typed bindings need no independent inner version.

Payload stamps protect stored representations, and each reader checks its
stamp before decoding its body. Changing kernel code changes no surface and
needs no drain
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §2); only
a changed stored representation does.
