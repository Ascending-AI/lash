# Drainage reads over artifact references

## Decision

`ProcessRegistry::live_reference_summary` aggregates non-terminal processes by definition identity and environment reference with counts. It reads a consistent current snapshot on demand and supplies in-flight version drainage information. It does not authorize artifact reclamation. Under ADR 0113 an artifact remains alive while any referrer edge holds it, including referrers outside non-terminal processes.

## History-node reachability

History liveness derives from indexed parent edges and each session's retained revisions (its head, its resolved pins and its retention window) in the destructive transaction. Shared prefixes belong to every retaining root. Session deletion removes its head and reclaims only producer nodes without a child or a retained revision, stopping at a shared prefix. PostgreSQL locks affected history rows to serialize root and edge changes.

Every head publication records a row in `session_revisions`. A pin names an input, a turn or a revision of its session and is a root only for host collection; it can be written before, during or after its target, and it is deleted with its session. `fork_at` adds a head at a retained revision without copying graph nodes. Checkpoint-blob reclamation considers the retained revisions (FIG-4731). Process version drainage remains an independent store-family read.

## Why and alternatives

Maintained drainage counters in artifact or environment stores are rejected because they couple store families and duplicate process liveness. Client-side paged scans require every host to reconstruct the same aggregate and transfer full payloads. Maintained history incoming-reference counters are rejected because drift creates a second liveness answer. Edges and roots are the authority.

## Consequences

Hosts can observe which captured versions are in flight. Lash reclaims artifacts when the final referrer ends; an absent drainage count is insufficient. [Process summary](../../crates/lash-sqlite-store/src/process_registry.rs), [consistent scan](../../crates/lash-sqlite-store/src/process_registry/pages.rs), [history storage](../../crates/lash-sqlite-store/src/history.rs) and [graph storage](../../crates/lash-sqlite-store/src/graph.rs) implement the reads and reachability.
