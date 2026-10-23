# 0145: Kernel execution and its checkpoints share one persistent heap

Recovery and external-effect guarantees follow [What survives a crash](../../README.md#what-survives-a-crash).

## Status

Accepted. The seams, their owning crates and the laws that pin each one are
in [the persistent heap seams](../design/persistent-heap-seams.md) (S1 to
S8, and the D-rules). The kernel rules this decision constrains are
`K-MACH-001`, `K-MACH-007`, `K-MACH-008` and `K-BND-003` in
[the kernel semantics](../kernel/semantics.md); the saved-state shape is
[kernel design §6](../kernel/design.md#6-parked-runs).
[ADR 0123](0123-model-code-runs-in-resettable-worker-processes.md) owns the
worker boundary and [ADR 0132](0132-durability-is-state-first-over-the-lash-store.md)
§8 the park transaction this decision fills in.

## Context

A kernel run, a session's bindings and a process body can hold far more
state than one statement, one park or one cell changes. When a checkpoint
is an image of the whole run, every park and every cell end costs work and
bytes in the retained state H, not in the change D: a no-op next cell over
100,000 retained objects rewrites them, a fresh worker decodes all of them
before it runs one statement, and reclaiming an old image walks all of it
inside the publishing transaction. Encoding the image better moves the
constant, not the proportionality.

The run's mutable state is the kernel's heap and its task records. If that
heap is itself persistent, a checkpoint is the set of pages a stretch of
execution changed, and a resume reads only what the next steps touch.

## Decision

### 1. One heap executes and checkpoints

The kernel's machine (`lash_kernel_vm::paged::Machine`) runs on one paged
`SemanticHeap` (`lash-kernel-state`'s `paged` module). Its durable objects live in
an object directory keyed by `EntityId`; every durable collection, payload
and execution table is a canonical tree of immutable pages. The machine's
resident arena holds execution values and projects changed state at a save. An edit copies only
the touched leaf and its index path. A park seals the dirty pages and
writes one `CheckpointRoot`; a resume opens a root and reads pages on
demand. There is no second representation of the run: no whole image,
no fragment set and no export or import step.

- **Pages.** A page is a generic `LASH` node envelope of a semantic kind in
  the range 1000 to 1999 (`NodeKind::SEMANTIC`): `CheckpointRoot` (1000,
  at most 4096 bytes), sequence and keyed index and leaf pages, payload
  leaf and index pages, `FrozenRoot` and `SessionRoot`. Every other page is
  at most 64 KiB. Its identity is the store's blob-domain digest; the
  kernel never hashes (`PageSource::seal` frames and digests).
- **Canonical shape.** Leaves pack up to 8 KiB, indexes fan out 32 ways,
  immutable text, bytes and integer digits over 256 bytes are greedy 4 KiB
  payload leaves under fanout-32 indexes. Two paged payloads are equal
  exactly when their root digests are, and equal payloads share pages.
  Physical sharing never merges guest identities.
- **Tree roots.** An external `KeyedRoot` or `SequenceRoot` is `Empty` or
  `NonEmpty { page, len, cost }`, checked at construction and at decode: a
  page-less root with rows, or a paged root without, is refused before any
  tree opens it. A nested tree's root is empty, an inline canonical leaf of
  at most 256 token bytes, or a page reference with its body length.
- **The heap header.** A `KEYED_LEAF`-tagged page holds the `HeapRoot`:
  directory, families, the next primary ordinal and family, the accounting
  certificate, the data-only population certificate and the last collection's
  entity-population estimate. A checkpoint root
  and a session root name this page; an unchanged heap reuses it.
- **Sealing.** A seal happens at a park, and dirty-page eviction seals what
  it evicts: past 32 MiB of dirty pages the oldest are sealed down to
  24 MiB. A fuel slice never seals. Clean pages and decoded nodes share a
  64 MiB residency allowance; pinned pages are bounded at 8 MiB. Residency
  and eviction preserve values, order and charge. Memory refusal follows the
  capacity contract of §5 and `K-BND-003`, including its cold-resume exception.

Laws: `lash-kernel-state`'s `paged::laws::heap`
(`a_point_write_in_a_huge_list_seals_one_leaf_path`,
`a_tiny_update_among_many_objects_seals_one_directory_path`,
`aliases_see_one_mutation_across_seal_and_open`,
`cycles_seal_as_a_page_dag`, `a_cold_text_length_observation_reads_no_payload_leaf`)
and `paged::laws::root` (`saved_meters_preserve_a_nonzero_next_join_on_decode`,
`a_checkpoint_root_fits_its_root_bound`).

### 2. The saved state is a checkpoint root

A `CheckpointRoot` is complete: the machine needs nothing else to resume,
not an earlier root, not a delta, not a worker cache. It holds the kernel
version, the document identity, `HelperLibraryId`, the execution closure (exact function
identities and their definitions) and the execution's `OwnerBinding` node,
the cell target (`Main` or
`Entry`), the adopted saved functions, the run's status (parked, or ended
with its frozen result and carry), the heap header, and tree roots for
tasks, the ready order, waits, list joins, withdrawn waits and the ones the
next park reports, read-bound effects and a session cell's bindings, plus
its exact meters. The table is [kernel design §6](../kernel/design.md#6-parked-runs).

- **Host binding admission (FIG-6499, S1).** The canonical host-name list
  is at most 3067 bytes, including UTF-8 names and token headers. This
  bounds both names and their count, even for empty names. The 4096-byte
  root reserves 773 bytes for every other field at its maximum encoded
  width and 256 bytes for framing. `CellStart` refuses excess metadata
  before opening the heap; worker clients check before starting a worker.
  Refusals retain the encoded limit and actual byte count in
  `StartError::HostMetadataTooLarge`. RLM host data uses package Read tools;
  continue-as seed entries are ordinary writable session variables.


- A frame persists semantic coordinates only: its pending statement's site
  and, at an action nested in it, that action's site, phase and operand
  roles by site (`K-EVAL-008`), bindings by name and declaring site, loops,
  `finally` departures (an entry's run-root cleanup is one, at its body's
  site, `K-TASK-019`), values a loop iterates and a library call's
  arguments. Registers and compiled code, statement and slot numbers are
  rebuilt caches and are never at a checkpoint.
- A wait is keyed by its number and holds its task, its kind and its phase:
  `Issued` with its frozen request, `Admitted`, or `Committed` with its
  frozen outcome. Issuing freezes the request once; the park that reports
  it moves it to `Admitted` in the same commit that admits it; delivery
  moves it to `Committed`; semantic resume instantiates the outcome and
  removes the wait in one edit.
- Task identity is a content-addressed parent-linked descriptor; a spawn
  writes one node. Names and sites are bounded descriptors.
- The execution closure and owner sit in the root's header, so the parent
  pins and checks them without decoding the heap.

A root of a retired format is refused, never read. The whole-image kinds
16, 27 and 29 are unregistered and their numbers are never reused: a
graph reader that meets one refuses it with a typed `GraphRefusal` and
decodes nothing under it, and the heap refuses a page of any kind but the one it expects, a
checkpoint root that is not kind 1000 included, as `HeapFault::Corrupt`
(`paged::laws::root::a_kernel_whole_root_is_refused`). The first release images are recorded fresh at the 1.0 cut; pre-1.0
crash/resume laws record and resume within one run.

### 3. Pages are read on demand and validated when read

The machine reaches storage only through its embedder's `PageSource`
(`read` and `seal`). The machine still does no I/O of its own (`K-MACH-001`);
the port may block synchronously, and a page miss blocks inside the port
with the statement and native stack intact. The port blocks without a durable park. A native view that encounters a
cold arena chunk returns the pure retry described in §5; it creates no
admission or guest-visible effect. Page transfer adds host work, not guest
charge.

In lash the port is the worker's `WirePageSource`. A worker asks its parent
with `WorkerMessage::Pages` mid-slice, in batches of at most 64 pages and
4 MiB, and sealed pages cross in batches the same way. The parent answers
from its activation's spool or, under the activation's root lease, from the
store. Every page crosses with a capability: the authority it is read under
(the root lease, or the stage lease for a page this activation staged), the
activation epoch and a keyed MAC whose key never leaves the parent. Reading
a page yields capabilities for exactly its envelope's children. A staged
page's child that was committed and reused keeps the root lease's authority;
a staged child keeps the stage lease's. Capabilities are never persisted: a
stored link is a bare page id, and every activation derives fresh ones.

A seal batch also rides on the hand-over frame that needs it: `Parked`,
`Sealed` and `Ended` carry the pages sealed since the last batch crossed,
so a park adds no page exchange (FIG-6288). The parent admits the batch
before it handles the frame and names the candidate by its digest; the
stage capability of each admitted page comes back on the next `Run`
(`sealed`), so the worker holds those pages under grants before its next
slice reads them. A page is named on the wire by the store's content
address of its envelope, so each side derives the name from the bytes.

A start carries `warm`: the pages its carried session or checkpoint root
names, and beneath them those the session's last activation read, warmed
or sealed, within 8 KiB, each checked as a read reply. Prefetch changes
latency only: a page it cannot carry is left to the worker's own read,
which meets any fault where it is typed. A process keeps the committed
pages it fetched in one cache bounded by bytes (64 MiB, least recently used
first), keyed by content address and shared by every reader in the
process; what a read may ask for is still decided by its capability or
lease (FIG-6321).

Full payload projections fetch demanded siblings together after their parent
has authenticated their references. `PageSource::read` and the wire return
per-page results in request order. The consumer decodes in traversal order
and surfaces the first typed fault; a later page fault cannot discard a
successful prefix or poison an earlier subtree's outstanding reads. Lease
and deadline refusals remain activation-wide. Visitors that can stop early
retain their prefix-only demand path (FIG-6595).

Validation is inductive and on demand:

- The worker hashes every answered envelope as it decodes it, once, and
  re-frames it before the kernel decodes it (FIG-6932), and checks a page's local shape, summaries and child
  membership when the page is first read. A bad page is refused before its
  contents are used.
- Changed pages are validated by the worker that produced them before they
  are sealed.

Corruption in a page the run never touches is therefore not detected before
the run's next effect. Detecting it would mean reading every page at every
resume. The trade is deliberate: a bad page is caught when it is read, and
never used.

Laws: `crates/lash-vm-worker/src/page_laws.rs`
(`a_fresh_worker_reads_the_header_and_touched_pages_only`,
`a_fault_mid_native_resumes_on_the_same_stack`,
`a_one_worker_pool_serves_pages_without_another_worker`,
`every_forged_foreign_or_stale_capability_is_refused_at_its_own_slot`,
`a_missing_or_corrupt_page_is_infrastructure_never_absence`,
`sealed_pages_cross_in_one_batch`,
`bulk_projection_batches_only_demanded_siblings`,
`bulk_projection_preserves_the_first_typed_fault`,
`a_transported_page_keeps_its_blob_address_and_token_vector`).

### 4. Infrastructure failures are never guest outcomes

Three classes stay apart at every seam:

- a **guest error** is a raised value whose kind the operation defines;
- a **guest bound** includes `Bound::Memory`, produced only by the
  capacity decision after sweeping and recounting (§5);
- an **infrastructure failure** is a `HeapFault`, `PageRefusal`,
  `StageFault`, `CensusFault` or `DurableError`, and the host resources
  `HostResource::TraceBudget` and `HostResource::HostWork`.

An infrastructure failure is never catchable, never a guest value and never
a guest bound. Native views report cold chunks through `Retry::Cold`. The
machine warms those chunks through its fallible page path before retrying the pure call;
a page failure stops the activation without fabricating a native result. The failed
activation ends, the worker is discarded, and the run resumes from its last committed
root. Staging quota and transport failures leave the committed root
intact.

### 5. Memory counts resident capacity and sweeps before refusing

The machine meters `AccountingMetric::Capacity`: its arena's resident value
storage and its control frames and slots. A value slot is 16 bytes; long text
and bytes use the arena's storage slots. The census records entity membership,
not a price for every object reachable only on disk. `AccountingCertificate`
persists the capacity held at sealing, its entity count and sweep generation.
A reopened machine uses that recorded capacity as a floor until its first
sweep recounts what it now holds.

The hot path checks growth against a 64 KiB capacity mark, or the configured
bound when it comes first. A verdict point, where all current holdings are
roots, decides the crossing: a statement end, an effect or a pinned native
answer. Before refusing an allocation or reservation, the machine sweeps and
recounts, first retaining ended task results, then retiring results no handle
reaches. If the request still does not fit, an installed `MemoryLimiter` may
grow the limit; otherwise the run ends with `Bound::Memory`. A grown limit is
saved with the run. Charge has its separate `ChargeLimiter`.

A cold run can refuse later than a warm run because page-only values count as
they are read. `K-BND-003` permits this memory-verdict timing difference;
`K-MACH-008` otherwise preserves values, errors, effects and charge. Memory
counts neither every reachable immutable payload nor whole-process RSS. A
page, staging or census fault is infrastructure, never a guest memory result.

Ordinary parks and resumes do not collect. A cell's carry collects when its
classification must inspect closure entities or task edges, or when entity
growth reaches `max(ceil(last live population / 2), 1)`. The population estimate
is persisted in `HeapRoot::collection_entities`, seeded at first carry
without a walk. A no-op data-only carry creates no pacing debt. There is no
byte-growth carry trigger or persisted `collection_live` field.

The machine keeps live slots and call stacks mutable between publication
points. Projection supplies semantic sites and roots when a save or census
needs them; it does not publish every call that returns between saves.
Natives use typed, call-scoped arena views and build through a `Nursery`.
A cold chunk returns `Retry::Cold`; the machine warms it and invokes the pure
native again. `Retry::Room` asks the machine to reserve more room, sweep or
refuse, then retry. The abandoned nursery is discarded, and the final
invocation's work counter supplies the guest charge. A native's transient
holding remains subject to the run's memory bound.

Implementation: `crates/lash-kernel-state/src/paged/census.rs`,
`crates/lash-kernel-vm/src/machine/limits.rs`,
`crates/lash-kernel-vm/src/machine/ops.rs`,
`crates/lash-kernel-doc/src/arena/native.rs`,
`crates/lash-kernel-state/src/paged/publication.rs` and
`crates/lash-kernel-vm/src/machine/checkpoint.rs`.
Laws live in `crates/lash-kernel-vm/src/laws/bounds.rs`, `resident.rs` and
`limiters.rs`, including `the_memory_bound_holds_inside_a_native_call`,
`the_memory_bound_refuses_a_native_reservation_before_the_allocation`,
`a_pressure_refinement_publishes_nothing` and
`the_memory_limiter_is_asked_with_what_the_run_holds`.

### 6. A park publishes one candidate with its admissions

A park that requests something seals a candidate root into its
activation's spool, a content-addressed staging area bounded by a quota
(1 GiB per activation by default): in the parent's memory up to 64 MiB of
it, disk-indexed past that (FIG-6902). The spool lives as long as
its stage lease: the activation plus the resolution of its last
publication. The parent builds the candidate's manifest by walking only the
staged DAG, stopping at committed pages, and streams it in ascending digest
order.

`PageStore::commit_candidate` publishes the candidate inside the owner's
existing fenced transaction, in global digest order: committed pages it
names are key-share locked and checked present, fresh pages are inserted,
edges follow once every row exists, then the domain writes (the execution
root, effect admissions, waits, host and session rows). Effects dispatch
only after the receipt confirms the commit.

A candidate has one publication identity
(`lash_durable::domain::graph_commit::check_candidate`). The commit names
exactly one owner write that publishes it: an execution root
(`CandidateOwner::ExecRootPut`) or a process run fact
(`CandidateOwner::RunFactAppend`, §7). That write's root reaches the
candidate's `CheckpointRoot` through supplied nodes, the manifest's base is
the root the row holds now, and the row and the manifest name the same root
and commit. Anything else is
`GraphRefusal::PublicationMismatch`, and nothing is written.

An authoritative commit settles the spool and deletes it. An authoritative
refusal (a lost fence, a revision conflict) discards it. An ambiguous one
(transport loss, timeout) keeps it, and the parent reads
`PageStore::candidate_state`: if the execution row holds the candidate's
root and commit, it committed; if it holds the base, the parent retries the
exact candidate, which is idempotent by commit identity, or discards it. A
worker's disconnection alone never settles a candidate, and a spool never
outlives its parent process.

Laws: `crates/lash-durable/src/laws/pages.rs`, on SQLite memory and file
and PostgreSQL (`a_candidate_installs_only_new_nodes_and_direct_references`,
`publication_is_atomic_with_its_effect_admissions`,
`the_lost_receipt_resolves_by_commit_identity`,
`publication_keeps_global_digest_lock_order`), and
`crates/lash-vm-broker/src/kernel/pages/spool_laws.rs`
(`a_manifest_is_the_reachable_staged_dag_in_digest_order`,
`an_ambiguous_publication_keeps_its_spool_until_resolved`,
`a_quota_failure_is_a_host_outcome`).

### 7. A kernel process's step outcome owns its checkpoint

A kernel process body parks through its `kernel_run` step. The step's
outcome owns the candidate it publishes: `KernelRunOutput::Parked` names the
sealed `CheckpointRoot` page, the outcome's `RunFact` record holds it as a
child, and `commit_candidate` runs in the transaction that appends that
outcome. The process's engine state is a `ProcessState` node whose body is
`ProcessStateBody::Kernel { root, bytes }`: the engine's own bytes and the
checkpoint root they stand on, which the process root owns from the advance
that names it. A crash after the outcome commits advances from it; a crash
before reruns the step and leaks nothing; pruning the run's facts releases
exactly their roots.

### 8. Roots are leased while read and retired in bounded steps

An activation reads under a **root lease**: a root-catalog owner row
acquired in one transaction that checks the owner's current root under the
actor's fence. The store's clock decides expiry. A read checks its lease in
the transaction that returns the bytes; after expiry every read and renewal
is `GraphRefusal::LeaseLost` and the activation is fenced, but the row
protects its root until it is released or reaped. The parent's authority
check runs on every page exchange, whether the cache holds the page or not:
it renews the lease once a third of its time to live has passed since the
last renewal, and a failed renewal refuses the exchange as
`PageRefusal::LeaseLost`. Within that third of a confirmed renewal the
store's expiry is still two thirds of a ttl away; past it, the store decides.
A held activation renews on the same schedule while it waits on a tool. The
cache answers only for immutable bytes, never for authority. An activation's
span bounds the host work its exchanges take, not wall time.

Replacing or releasing a root never reclaims inside the publishing
transaction. The old root becomes a **retirement hold** (a
`retirement_holds` row), a root source in the catalog beside owners and
leases (`RootSource::ReadLease`, `RootSource::RetirementHold`). A
retirement step is one transaction. It first reaps expired leases, whose
roots join its **frontier**. It then continues the frontier, oldest first,
before it takes the oldest hold, and peels level by level in digest order
until its node budget is spent, deleting the nodes that no owner, lease,
hold or inbound edge keeps. Whatever it did not reach, and every node its
peel skipped, goes back on the frontier (the `retirement_frontier` table)
in the same transaction. The frontier is no root of the catalog and has no
foreign key: only steps write or take it, and a frontier node that is gone
meanwhile is passed over.

Steps run one at a time and never stop publication. On PostgreSQL a step
takes the transaction advisory lock keyed `lash.pages.retirement`, the
only lock it ever waits for, and takes holds, expired leases and peel
locks `FOR UPDATE SKIP LOCKED`, so it passes over whatever a publication
holds. Every transaction takes its locks in one global order, each class
in digest order: the retirement key (steps only) or the owner's fence row;
blob rows; the root catalog's owning rows and the semantic admission; hold
rows; a step's peel locks; its frontier rows. A publication that meets a
row a step locked therefore waits for a transaction that waits on nothing
(`publications_and_retirement_on_two_nodes_never_deadlock`). On SQLite
every write runs behind the database's one writer, so a step never
interleaves with a publication.

Each durable node runs a retirement task beside its lease renewal. A sweep
runs up to `steps_per_sweep` steps of at most `batch_nodes` nodes and stops
early once a step finds nothing. When the store's `retirement_backlog` is
above `backlog_limit`, the node claims no new actors and sweeps back to
back until it is under the limit again. Every limit is host configuration
with a documented default and hard maximum
(`DurableSettings.retirement`, [durable hosting](../operations/durable-hosting.md#page-retirement)).

Laws: `crates/lash-durable/src/laws/pages.rs`
(`a_leased_root_survives_concurrent_retirement`,
`an_expired_lease_reads_nothing`,
`retirement_advances_in_bounded_steps_and_reclaims_everything_unowned`).

### 9. A session carries page roots between cells

A terminal cell selects its carry: its bindings and its saved functions,
each a keyed tree root. `carry` seals them, with the heap header and the
data-only certificate, into a `SessionRoot`; the session's checkpoint names
that page. The next cell opens the same heap and `adopt`s the carry: entity
identities are kept, and nothing is renamed, loaded or copied, so two
bindings that shared an object still share it (`K-SES-002`).

- `classify_carry` decides which bindings carry (`K-SES-003`). When the
  data-only certificate shows no closure entity and no task edge, it
  returns the root without reading a leaf; otherwise one union traversal
  with shared visited state classifies every candidate.
- A committed effect outcome and a cell's result are frozen once into
  immutable pages and delivered as a fresh identity family: entity
  `(family, ordinal)` is the ordinal-th mutable occurrence, so equal
  subtrees at distinct positions are distinct objects. Membership is the
  heap's persisted family record; a swept ordinal stays absent. The family
  is reserved in one census operation and read lazily; a length read
  instantiates nothing.
- A fork shares the session's pages and owns its own directory version, so
  edits never cross forks.
- A saved function holds its document code, its captures as frozen
  references by name, and its execution closure. Each
  invocation binds every capture as a fresh family over its frozen
  reference; nothing flattens a capture back into the code.

Laws: `lash-kernel-state`'s `paged::family_impl::laws`
(`a_noop_next_cell_copies_nothing`, `a_fork_isolates_later_mutations`,
`a_frozen_capture_ignores_later_live_writes`,
`a_lazy_result_length_read_instantiates_nothing`,
`data_only_carry_reads_no_binding`), `paged::laws::lifecycle::a_swept_child_never_resurrects_while_its_family_stays_live`
and `lash-kernel-dialect`'s `saved::laws`.

### 10. Host graph reads stop at heap pages

`DurableReads::hydrate` stays the generic reader of a host root's graph:
turn phases, execution closures, process engine state, execution control nodes
and session manifests. It reads every node the root reaches, checked, within
its decode limits, and it never descends into heap pages. A host node's
semantic child is returned as itself, a header the host may read, and a
read asked to start at a semantic page is refused
(`GraphRefusal::SemanticPage`). Heap pages are read only under a root lease
through the page exchange, a page at a time.

### 11. Kernel-version migration and package adoption

`lash-kernel-migrate` carries documents and definitions. Until a paged
kernel-version migrator ships (FIG-6233), a parked run in a kernel version
the build does not interpret is refused with
`ParkedRefusal::StateNotCarried`, keeps every row and remains resumable on a
build that interprets it. That migrator must land before the first breaking
kernel version after 1.0.

Package changes use identity retention and explicit adoption. A parked run
pins its Link DAG and Execution Environment; publishing another Package does
not replace those identities. Document adoption refuses a changed operation contract;
parked adoption refuses `state_not_carried` because heap pages are not
carried. A session relinks its saved functions against each cell's
Namespaces while preserving resolved pins; package values check their shape on use. The
[host-package guide](../host-packages.md) gives the host-facing rules.

## Consequences

- A park costs the changed pages and their index paths; a no-op next cell
  costs a constant number of pages; a cold continuation reads the header and
  the touched paths. The counted laws in `crates/lash-perf` hold these
  shapes flat in H at fixed D and W.
- Checkpoint storage is shared: unchanged pages, equal payloads and frozen
  results are written once and referenced by every later root.
- Corruption in an untouched page is found when the page is first read,
  not at resume.
- Memory pressure can cost a census sweep and resident recount. Ordinary
  parks do not collect; cold resume loads state on demand.
- Reclamation lags publication by the retirement backlog, which host
  backpressure bounds.
- The first release images are recorded fresh in the paged format at the 1.0 cut.

## Rejected alternatives

- **A whole image per park, encoded better.** Binary encoding, shared large
  leaves and per-root fragments shrink the constant, but every park and
  cell end still costs H, and a fresh worker still decodes everything.
- **Collecting at every park.** It would make every park cost the reachable
  graph even when the resident capacity already fits.
- **Concurrent, snapshot-at-the-beginning or reference-counting
  collection.** None decides an exact verdict on its own, and each adds
  write barriers to every edit; lash already has quiescent pressure points.
- **Reclaiming in the publishing transaction.** A retired root's closure can
  be as large as H, and a peel inside publication serializes it with
  every writer.
- **Eager validation of every page at resume.** It restores the old
  detection timing at the old cost.
