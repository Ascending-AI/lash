# Artifact lifetimes use exact owner edges

## Status

Accepted.

## Context

Several durable readers can share immutable modules and execution environments.
Deleting content by address can break another reader; retaining every
publication indefinitely leaks abandoned inputs. Compilation cannot choose the
lifetime of its output.

## Decision

Compilation links and introspects a lowered program and returns a
`ModuleArtifact` without I/O. The TypeScript front end owns parsing.
Publication is a separate artifact-store operation.

Every publication or acquisition names a checked `ReferrerClaim`. Stores keep
exact `(artifact, referrer)` edges. Equal immutable content can have several
referrers. Cleanup severs only the ended referrer's edges and reclaims bytes
when no edge remains. A content id alone retains nothing.

The vocabulary and cleanup protocol belong to
[ADR 0113](0113-artifacts-are-kept-alive-only-by-their-referrers.md).
`ArtifactReferrer` names a frame environment, process record, subscription
revision, start, execution, host pin, definition revision, session, or
upload. Stores decode each kind/id pair through its canonical encoding and
refuse malformed or unknown vocabulary.

Publication checks the referrer's fence, verifies immutable bytes, adds the
edge, and arms any required cleanup guard in one transaction. Acquisition uses
the same fence and guard rules and refuses missing bytes. Ending a referrer
applies resolved cleanup atomically: fence it, acquire required carries,
sever its edges, and reclaim unreferenced content. Repeating applied cleanup is
a no-op. The fence prevents a delayed publisher from reviving the referrer.

Process-start retention uses start and process-record referrers. Frame switches
carry retained artifacts to the successor frame. Cleanup consumes durable end
evidence, not elapsed time or an absent worker. Host pins remain until release.

Evidence: `crates/lashlang/src/compile.rs:38`,
`crates/lash-core-store/src/artifact_referrer.rs:46`, `:146`, `:201`,
`crates/lash-core-execution/src/module_artifacts.rs:132`,
`crates/lash-core-execution/src/runtime/process/start_staging.rs:385`,
`crates/lash-sqlite-store/src/artifact_store.rs:186`, `:309`, `:538`, and
`crates/lash-postgres-store/src/postgres/artifact_store.rs`.

## Alternatives considered

Reference counts conceal which durable reader retains content and add mutable
retry accounting. Exact edges make acquisition and release idempotent per
reader. Time-based reclamation cannot prove a durable reader has relinquished its
inputs; cleanup requires reader end evidence.

## Consequences

- Several readers safely share immutable bytes.
- Reclamation follows exact edges and end fences.
- Compilation and publication have separate responsibilities.
- Every backend implements atomic publication and cleanup.
