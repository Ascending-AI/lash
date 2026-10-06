# Attachments have blob storage, reference tracking and host lifecycle policy

## Decision

Attachments have three owners. Hosts supply flat blob storage, Lash tracks durable references and write evidence, and hosts schedule lifecycle operations through Lash's reclamation lever. Content-addressed reads do not enforce auth; the host owns security policy.

## Blob storage

`AttachmentStore` exposes `put`, `get`, `delete`, `head`, `list` and a persistence descriptor. It has no session namespace or session ownership. Identical bytes share one digest and physical blob. File and S3 stores use `blake3/<prefix>/<digest>`. Missing reads return typed `NotFound`; list and head support reclamation.

The file store stages to a unique pid/counter sibling, syncs bytes, renames it and syncs the parent directory. Unsupported directory syncing is tolerated; other I/O failures fail the write. Puts refresh blob modification time even on deduplication, so delete-time freshness checks observe recent writes.

## Reference tracking

`AttachmentReferrers` holds digest edges, pending writes and upload evidence. A `RuntimeAttachmentStore` binds the flat backend, referrer port and runtime holder. Turn puts are held by their turn's execution; process puts by their process record; out-of-turn session puts by a fresh expiring session upload. Ephemeral holders record no durable roots.

`begin_attachment_write` records the pending attempt and its edge before writing bytes. `complete_attachment_write` requires its current permit and records positive upload evidence. A stale permit records nothing; the facade can retry behind a fresh permit within its bounded attempt budget. Aborting settles only that attempt and releases its edge only when no evidence or sibling write requires it.

A boundary commit acquires receiving-session edges for stored addresses in committed messages and tool outputs in the same transaction as graph publication. Adoption requires upload evidence and refuses an in-flight delete as `UnknownAttachment` without publishing any edge or graph state. Holding an edge proves liveness, not authorization. Reads resolve content addresses directly. Facade `delete` forgets the holder's lasting edge when retained history permits it; physical deletion belongs to GC.

## Fenced reclamation

`reclaim_unreferenced_attachments(root_set, backend, policy)` enumerates blobs and the deployment-wide roots. An edge or pending write keeps a digest live. Age and owner-death inference do not end referrers. The cleanup executor owns referrer end under ADRs 0113 and 0124. An empty root set requires explicit `AuthorizeDeleteAll` before deleting eligible blobs.

The root authority owns a conditional state machine: absent/free, `Condemned` and `Deleting`. A sweep opens a generation and adopts predecessor condemnations before considering new candidates under ADR 0067 §6. Condemnation refuses rooted digests and removes adoption evidence under its fence. Arming grants physical delete authority; a writer can claim an unarmed condemnation with its pending-write token, but records nothing against an armed delete and retries with bounded backoff. Success, or a fenced head proving absence, retires condemnation. Failed deletes retain typed retry/stall evidence; store-clock retry deadlines grant eligibility rather than ownership.

The sweep rechecks modification time after arming and spares fresh blobs. Freshness is a filter; transactional writer/sweeper fencing closes the delete race. Completing, aborting or ending a pending write structurally releases its condemnation claim. Host recovery of an abandoned pending write requires establishing that its writer is stopped. Sweep ownership is recovered by generation adoption rather than a host release operation or TTL.

A `Fenced` authority implements the writer and sweeper halves together. Unsupported condemnation downgrades the report to `BestEffort`; a missing writer half is an implementer defect the sweeper cannot detect. Best-effort reclamation rechecks roots and reports `deleted_while_referenced`; it detects rather than prevents the race. A fenced report must keep that list empty.

## History and lifecycle consequences

Root enumeration failure refuses deletion when eligible blobs require that answer. With no destructive candidate the result still carries enumeration diagnostics, and an unwitnessed non-empty scope can be refused. Per-blob failures accumulate without aborting the entire sweep.

SQLite's durable catalog and PostgreSQL's deployment tables supply complete referrer answers. The blob backend must be exclusive to that deployment; otherwise a complete local root set cannot establish global garbage. Hosts choose scheduling and grace periods; core supplies no GC schedule.

Deleted-session edges remain while graph nodes are retained by children, heads or pins. `AwaitSessionGraphRetired` ends them when the retained prefix is gone; forgetting a session edge uses the same precondition. This conservatively retains suffix attachments until the final prefix ends. Exact per-node attachment edges are a possible refinement, without changing the safety rule.

Physical per-session copies are rejected because complete referrer tracking already permits safe shared blobs and keeps storage independent of sessions. Durable format admission follows the current format registry and reject-and-recreate policy; this ADR does not assign version numbers or migration defaults.

[Blob and runtime ports and GC](../../crates/lash-core-store/src/attachments.rs), [write/referrer contract](../../crates/lash-core-store/src/store/attachment_referrers.rs), [file durability](../../crates/lash-core-store/src/attachments/file_store.rs) and [SQL referrer implementation](../../crates/lash-sqlite-store/src/attachments.rs) implement the layers.
