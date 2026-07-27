# Drainage reads over artifact refcounts

A host that retires old definition artifacts or env blobs must know when nothing in-flight
still needs them — recovery of a process whose artifact is gone is a hard
`process_module_artifact_missing` failure, so retirement without evidence is data loss. We
added a registry aggregate: `live_reference_summary()` groups non-terminal processes by
(identity definition, env ref) with counts, computed on demand from process rows. A definition
or env ref absent from the summary is drained and safe to retire; the counts double as an
"in-flight per version" drainage signal for host UIs.

We rejected maintaining live reference counts inside the artifact/env stores: it couples two
deliberately separate store families, adds a write to every process lifecycle transition, and a
drift bug silently corrupts retirement decisions — whereas the aggregate is recomputed from
truth on every call. Client-side counting via paged list reads was rejected as a substrate
gap: every retiring host would re-implement the same scan, paying full row payloads to compute
a count.

## History-node amendment

History nodes use a different retention boundary. Their parent edges, session-head roots, and
explicit host pins live in the session store, so each backend maintains an `incoming_refs` cache
beside an indexed `parent_node_id`. A transition to zero is never trusted by itself: before
tombstoning, the same transaction re-derives the count from edge and root rows and aborts with
`NodeRefcountDrift` on disagreement. Hosts can run `verify_node_refcounts` to scrub the entire
catalog and detect forgotten refcount mutation sites.

PostgreSQL history commits and session deletion share a session-keyed transaction lock. A retained
deletion marker also fences a stale first commit, where no head row exists to lock; only explicit
session-store recreation clears that marker.

This does not move non-terminal process roots into stored history counts. Processes remain in a
different store family, and their definition/env liveness continues to be computed on demand by
`live_reference_summary()`. Folding process transitions into history-node counters would recreate
the coupling rejected above.
