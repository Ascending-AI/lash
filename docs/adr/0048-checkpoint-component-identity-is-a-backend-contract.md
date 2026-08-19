# Checkpoint component identity is a backend contract

A session checkpoint is one manifest plus independently addressed tool-state,
plugin-snapshot, and execution-state components. Their durable identity is part
of the session-store contract, not a representation choice each backend may
reinterpret.

The defect that forced this ruling predates the shared-history cutover. It has
existed since the PostgreSQL backend was introduced in `973bf591` on
2026-06-09. PostgreSQL stored a backend-only `SessionCheckpointEnvelope` that
embedded component bodies but merely echoed any component refs supplied by the
caller. Unlike SQLite, it minted no refs for newly stored bodies. After the
runtime cleared a clean RLM executor's hydrated bytes, the next ref-only
checkpoint therefore persisted no execution state, and a cold open silently
lost the RLM globals. The in-memory store also echoed refs without implementing
the same identity contract.

When a backend stores a checkpoint component body, it must mint the
content-addressed ref that addresses that body and return the ref in
`RuntimeCommitResult::manifest`. A later commit carrying that ref without a
body means “unchanged”: the backend must resolve the body from storage when it
hydrates the checkpoint. A ref-only commit whose body is absent is invalid and
fails with the typed missing-component error rather than persisting partial
state.

SQLite, PostgreSQL, and the in-memory store now implement this same rule.
PostgreSQL adopts the manifest-plus-component-blob shape and deletes
`SessionCheckpointEnvelope`; PostgreSQL schema 24 is a reject-and-recreate
boundary for the incompatible old envelope. Cross-backend conformance covers
body-to-ref minting, clean ref-only commits, cold hydration, and rejection of
unknown refs.

> **Historical versions.** The version numbers in this ADR record the state at ratification. The current values live in `lash::formats`; see `scripts/check_format_versions.py`.

This does not turn checkpoints into an event log. Each boundary still replaces
the complete resumable-state snapshot, while component refs let an unchanged
body be reused without recapturing or rewriting it. Backend storage mechanics
such as compression remain private only after this identity and hydration
contract is satisfied.

[ADR-0049](0049-session-ids-are-used-once.md) removes the separate session
lifetime discriminator. That does not alter content-addressed component
identity: component refs still identify bytes, while the host-provided,
single-use session id identifies the checkpoint owner and binding.
