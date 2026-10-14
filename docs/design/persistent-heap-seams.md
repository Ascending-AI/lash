# Persistent semantic heap: the frozen seams

Status: frozen design for the FIG-6187 arc, delivered by FIG-6195 on branch
`lanes/hp-seams`. The decision it implements, in its end state, is
[ADR 0145](../adr/0145-kernel-execution-and-its-checkpoints-share-one-persistent-heap.md);
where this document and the ADR differ, the ADR states the landed design and
§7 below records the integration rulings. Source anchors are at `195ed4db93` (main when the arc
fanned out). It does not repeat the arc's specification; read it with the
four design reports (the decision and cost contract, representation and
identities, page access and publication, exact memory and collection). Where
this document and those reports differ, this document wins: §2 records each
difference.

The arc replaces whole-state checkpointing with one persistent semantic heap.
Execution and checkpoints use the same paged heap. A checkpoint freezes the
changed pages and atomically publishes a root. A fresh worker loads only the
pages its next operation needs. Five layer lanes build on this branch in
parallel. One integrator lands the arc as a single clean cutover:

| Lane | Ticket | Outcome |
| --- | --- | --- |
| hp-heap | FIG-6188 | The unified heap and the execution records |
| hp-memory | FIG-6189 | Accounting: the census, reservation and exact refinement |
| hp-paging | FIG-6190 | Worker demand reads and private staging |
| hp-publish | FIG-6191 | SQL publication, leases and reclamation |
| hp-life | FIG-6192 | Cells, frozen results and saved functions (stacked on hp-heap) |
| hp-integrate | FIG-6193 | Deletions, callers, fixtures, ADRs and end-to-end laws |

## 1. Rules every lane keeps

- The seam declarations are frozen. They live in the files listed in §3 and
  change only by an orchestrator ruling. A ruled change lands on
  `lanes/hp-seams` first and every lane rebases onto it. A lane implements a
  seam in its own files and never edits another lane's seam or files (§4).
- Declarations that replace a current type sit in a `heap` or `paged` module
  under their final names: `lash_kernel_doc::heap::{Value, NativeHeap, ..}`
  and `lash_kernel_vm::paged::Machine`. At the cutover the owning lane
  changes the crate-root re-export to them and deletes the old item. Nothing
  is renamed, and there is no adapter between the old and new types.
- Error taxonomy, the same at every seam:
  - a **guest error** is a raised value whose kind the operation's semantics
    define (`type_error`, `index_out_of_range`, an illegal key);
  - a **guest bound** is `Bound::Memory`, and only exact refinement (S7)
    produces it;
  - an **infrastructure failure** is `HeapFault`, `PageRefusal`,
    `StageFault` or a `DurableError`. It is never catchable, never a guest
    value and never a guest bound. The activation ends and resumes from its
    last committed root.
- Old-format images are refused, and there are no dual readers, no flags and
  no migrations. No live data is deleted.

## 2. Decisions

Each decision settles a question where the reports disagree or leave a choice
open that two lanes would otherwise settle differently.

- **D-SEAL.** The kernel never hashes. `PageSource::seal` frames a logical
  body in the generic envelope, computes its digest and keeps it readable. The
  kernel crates stay free of the store and of blake3, and a page has one
  digest function: the store's.
- **D-LOGICAL.** The kernel sees logical page bodies only, with child
  references written as full digests (`TypedBody::logical`). The embedder
  converts between logical bodies and the envelope's child-table slots.
  Envelope framing stays the store's.
- **D-ENV.** Semantic pages use the existing generic envelope
  (`"LASH" | kind | child table | body`) with the blob-domain blake3 digest
  `LASH_BLOB_DOMAIN_VERSION`. The registry's reserved range 1000–1999 becomes
  the semantic page range. Kinds 1000–1008 are declared in
  `paged::source::PageKind`. All of them are direct-bodied and node-bounded
  except `CHECKPOINT_ROOT`, which is root-bounded (4096 bytes). hp-heap owns
  the canonical body encodings. hp-publish registers the range in the node
  registry and points `ExecRoot.state` and `ProcessState::Kernel` at
  `CHECKPOINT_ROOT`. `check_envelope` already accepts any nonzero kind, so
  install needs no registry change before then.
- **D-IMM.** Two rules govern an immutable payload (text, bytes, integer
  digits). (1) It is `Payload::Resident` when it is at most
  `IMMUTABLE_INLINE_MAX` (256) bytes or was built in this activation and not
  yet placed in pages. Otherwise it is `Payload::Paged`. (2) Payload trees
  are **canonical**: greedy leaves of at most `PAYLOAD_LEAF_BYTES` (4096)
  that end on UTF-8 scalar boundaries, with fanout-32 indexes built bottom-up
  and carrying byte, character and UTF-16 counts. Two paged payloads are
  therefore equal exactly when their root digests are. Equality of canonical
  payloads and comparison of already-built normal forms need no page read;
  building a long value's normal form may read its payload once. Equal
  payloads share pages. This
  rejects the representation report's "root-preserving partial chunks". A
  concatenation or slice re-chunks its output in O(output), which is already
  the cost of today's `Arc` copy.
- **D-EQ.** The new `Value` has no Rust `PartialEq`, `Ord` or `Hash`. Guest
  equality and key normalization are `SemanticHeap::equal` and `normal_key`,
  which are fallible because a tuple or a resident-against-paged comparison
  can read pages. No I/O hides behind an infallible `Ord`.
- **D-KEY.** A key's normal form (`NormalKey`) holds its canonical
  normalized key tokens when they are at most 256 bytes. A longer key is the
  domain-separated SHA-256 of its full normalized tokens. The lookup index
  orders keys by `NormalKey`. That order is internal: iteration follows
  insertion sequence (`K-ITER-003`). Normalizing a paged key can fault, but
  comparing two normal forms never does.
- **D-CAP.** Start values, which measurement tunes later: index fanout 32;
  64 cells per sequence leaf; 64 rows per keyed leaf; leaves packed up to
  8 KiB (`PAGE_TARGET_BYTES`) under the absolute 64 KiB node bound;
  object bodies inline in directory rows up to 256 token bytes; payload
  leaves of 4 KiB; read batches of 64 pages and 4 MiB. Host budgets per
  worker: 64 MiB clean residency, 32 MiB dirty, 8 MiB pinned. Staging quota:
  1 GiB per activation. The worker settings (`WorkerTuning`) and protocol
  bounds expose the host budgets. The format constants are the `paged`
  modules' `const`s.
- **D-GRANT.** The heap stores a 48-byte opaque `PageGrant` beside each
  `PageId` in its resident links and passes it back unread. On a worker the
  grant is the parent's `PageCapability`: authority (8 bytes: a root or
  stage lease), activation epoch (8) and a MAC (32). The MAC is keyed blake3
  over (page, authority, epoch), and its key never leaves the parent. Reading
  a page yields grants for exactly the children in its envelope's child
  table. Grants are never persisted: a persisted link is a bare `PageId`, and
  every activation derives fresh grants from its leased root. In-process
  embedders use `PageGrant::NONE`.
- **D-LEASE.** A root lease is a root-catalog owner row
  `(id, owner, root, actor, epoch, expires_at)`, acquired in one transaction
  that checks the owner's current root under the actor's fence. The store's
  clock decides expiry. After expiry, every read and renewal under the lease
  is refused (`LeaseLost`), but the row still protects its root until the
  owner releases it or a retirement step removes it. Reads are transactional
  and check the lease inside the same transaction that returns the bytes. A
  candidate's commit protects what it reuses with its own key-share locks,
  so lease expiry needs no grace period. The parent renews a live
  activation's lease at a third of its ttl (start value 60 s). Expiry fences
  the activation.
- **D-SPOOL.** A spool lives exactly as long as its stage lease: the
  activation plus the resolution of its last publication. On an
  authoritative commit it is settled and deleted. On an authoritative
  refusal (fence or revision conflict) it is discarded with the activation.
  An ambiguous publication (transport loss, timeout) keeps the spool. The
  parent then reads `PageStore::candidate_state`. If the execution row holds
  the candidate's root and commit identity, the candidate committed. If the
  row still holds the base, the parent retries the exact candidate, which is
  idempotent by commit identity, or discards it. Worker disconnection alone
  never settles a candidate. A spool does not survive its parent process:
  that activation is lost and resumes from the durable root.
- **D-RET.** Replacing or retiring a root no longer peels synchronously in
  the publication transaction. The transaction moves the old root to a
  retirement hold, a new root source in the catalog. Each bounded step takes
  one hold, deletes in digest order the nodes under it that no owner, lease
  or inbound edge keeps, and installs holds on their surviving children, all
  in one transaction. Steps run under the shared graph gate and use row
  locks, so they never stop publication. Hosts apply backpressure from
  `retirement_backlog` and staging usage.
- **D-ADM.** One semantic admission is recorded per published root, keyed by
  the root, in the publishing transaction:
  `(root, kernel, page_format, validator, commit)`. The validator string
  names the kernel version, the page format and the digest of the validating
  build's page schema. Unchanged pages inherit admission. Changed pages are
  validated by the producing worker before sealing. A root admitted under
  another validator, or a foreign image, enters the trusted population only
  after the full validation pass. It
  never inherits an incompatible attestation.
- **D-WAITS (amended): table root digest; no XOR summary.** The waits table
  is canonical; its root digest is its identity. There is no separate summary.
- **S1/S4 (amended): heap-header page; CheckpointRoot references it.** A
  KEYED_LEAF-tagged heap-header page encodes HeapRoot. The checkpoint names
  that page instead of embedding a second copy. Unchanged heaps reuse it.
- **S7 (amended).** CensusGraph exposes persistent root cost, nested value
  edges and value cost, plus ascending ended-task enumeration. SweepSet names
  both unreachable entities and ended-task results; installation is bounded
  and a failure ends the activation. The census traces in memory only.
- **S1 (amended): census injection.** Machine::start takes an additional
  `census: Box<dyn AllocationCensus>`. Composition constructs the concrete
  census; the VM never chooses or duplicates accounting policy.
- **S1/S8 (amended): authoritative membership and lazy heads.** The heap
  checks its persisted membership runs before any override or frozen-base
  read and installs sweep tombstones. FamilyBase has no member method;
  `base_object(entity, frozen, pages)` returns `Option<ObjectHead>`. A live
  member with no frozen body is corrupt-family state. ObjectHead exposes
  bounded collection roots with authenticated lengths and its family.
- **S1/S8 (amended): TupleRef/link family context.** Paged tuples and tree
  links carry optional family context. Relative occurrences have tag 19,
  distinct from absolute references. Missing or mismatched context is
  corruption; a context may not interpret an absolute reference to its own
  family. Copied leaves encode absolute identities and clear their context.
- **Ownership amendment (2026-10-14).** hp-heap deletes the kernel-state
  whole-format modules and the VM's old session/parked paths needed for the
  Value cutover. hp-life owns the new outcome-delivery/instantiate call
  sites as well as lifecycle implementation. It starts on hp-heap's paged
  heap milestone and merges subsequent hp-heap commits.
- **S8 (amended).** SessionRoot.heap is the heap-header PageRef. Frozen
  occurrence heads are indexed once at freeze in an immutable keyed tree;
  delivery opens its root and never rebuilds or scans that index.
- **S8 (amended): paged binding carry.** CarryRoots holds binding and function
  KeyedRoots. classify_carry takes a binding KeyedRoot and returns a carried
  KeyedRoot plus dropped names. adopt returns and attaches a KeyedRoot. Bindings
  use Tree<BindingRecord>; zero-forbidden classification reads no leaf. The
  100k-separate-bindings no-op law counts O(1) pages. No Vec-based API remains.
- **D-FRAME.** Persisted frames hold semantic coordinates only: the pending
  statement's `Site`, bindings by declaring site, loops (a map or set
  position is a `TableCursor`), `finally` departures and held values. This
  fits the flat-KIR machine (FIG-6161). Its `Frame { code, slots, control,
  awaiting, library, inline }` keeps the same control shape. Its registers
  belong to a slice and are cleared before any park. An inline frame runs to
  its end inside its expression. So neither registers nor inline frames
  exist at a checkpoint, and compiled `CodeId`/`StmtId`/slot indexes are
  rebuilt caches.
- **D-SEALTIME.** A seal happens at a park, and dirty eviction seals the
  pages it evicts. A `Slice` step does not seal. Pressure refinement never
  creates a checkpoint opportunity mid-statement.
- **D-FAM.** `FamilyId::PRIMARY` (0) is a heap's ordinary allocation. Its
  next ordinal is in `HeapRoot`, and a session's next cell continues it. Each
  lazy instantiation takes `HeapRoot::next_family`. Entity `(f, n)` is the
  n-th mutable occurrence, in preorder, of family f's frozen base, so equal
  subtrees at distinct positions are distinct entities. Membership is a
  keyed run of `MembershipRun`s: a swept ordinal is absent, an edit is an
  override row in the same directory, and a lookup checks membership before
  it reads the base. A fork shares pages and owns its directory version from
  then on. Two forks may later mint the same `EntityId`, which is harmless
  because they never share a directory again.
- **D-NATIVE.** `NativeHeap` reads return `Result<_, HeapFault>`, and
  `NativeError` gains `Heap(HeapFault)`. A call that faulted is never resumed
  as a result. Prepared native entries read no page: a paged operand makes
  them return `None`, which takes the general call.
- **D-METRIC.** `AccountingMetric::LocalCostV1` is today's pricing
  (`OBJECT_BYTES` 32, `VALUE_BYTES` 16, payload bytes at each holding
  location, shared entities once per identity), unchanged. hp-heap moves it
  into page summaries. hp-memory owns U, reservation, refinement and the
  certificate. `MIN_ENTITY_COST` is 32 bytes.
- **D-ORDER.** No guest-visible order derives from entity identities; only
  insertion order and value order are visible. hp-heap verifies this when it
  ports `lash-kernel-lib/src/keys.rs`.
- **D-RES.** Resident pages are interior, single-threaded state, so heap
  reads take `&self` and a visit can lend elements of a pinned page.
- **D-MSG.** The page exchange's wire types live in `lash-vm-protocol`
  (`page.rs`), not `lash-vm-worker`. The parent (`lash-vm-client`,
  `lash-vm-broker`) must decode them, and it cannot depend on the worker
  crate. `host.rs` implements the worker side. This differs from the unit
  notes for this reason only.
- **D-FREEZE.** The protocol version, store format and schema stay one 1.0
  baseline: changes are made in place, and nothing bumps a version constant.
- **D-ADMIT.** `AdmittedWait` and `AdmittedRequest` survive unchanged.
  Whoever deletes `lash-kernel-state/src/whole.rs` (the integrator) moves
  them into `paged/root.rs`.

## 3. The seams

Every seam below states its declaration file, owning crate and implementing
lane, its invariants, its errors and the law that pins it. A law named here
is a contract: the implementing lane writes it at the stated path and tier.
Tiers follow `docs/agents/test-backends.md`. "Kernel unit" means the crate's
`__unit_test` target with an in-memory test `PageSource`; store tiers are
SQLite memory, SQLite file and PostgreSQL.

### S1 `SemanticHeap` and the native contract

Declarations:

- `crates/lash-kernel-state/src/paged/heap.rs`: `SemanticHeap`,
  `ObjectInit`, `ObjectKind`, `ClosureView`, `FieldName`, `NormalKey`,
  `TableCursor`, `EditEpoch`, `SealBudget`, `SealedHeap`,
  `HeapWriteError`.
- `crates/lash-kernel-doc/src/heap.rs`: `Value`, `Object`, `Element`,
  `Chunk`, the immutable descriptors (`TextRef`, `BytesRef`, `IntRef`,
  `TupleRef`, `ErrorRef`, `Payload`, the summaries), `NativeHeap`,
  `NativeCall`, `NativeFunction`, `PreparedNative`, `NativeError`,
  `HeapFault`, `HostResource`.
- `crates/lash-kernel-vm/src/paged.rs`: `Machine`, `CellStart`,
  `SealedRoot`, `Finished`, `MachineError`, `OpenError`, `SealError`.

```rust
pub trait SemanticHeap: NativeHeap {
    fn open(root: &HeapRoot, source: Box<dyn PageSource>, epoch: EditEpoch) -> Result<Self, HeapFault>;
    fn fresh(source: Box<dyn PageSource>, epoch: EditEpoch) -> Self;
    fn kind(&self, entity: EntityId) -> Result<Option<ObjectKind>, HeapFault>;
    fn table_after(&self, table: EntityId, after: Option<TableCursor>) -> Result<Option<(TableCursor, Value)>, HeapFault>;
    fn allocate_object(&mut self, init: ObjectInit) -> Result<EntityId, HeapWriteError>;
    fn list_write(&mut self, list: EntityId, index: u64, value: Value) -> Result<(), HeapWriteError>;
    fn map_write(&mut self, map: EntityId, key: Value, value: Value) -> Result<(), HeapWriteError>;
    fn record_write(&mut self, record: EntityId, field: FieldName<'_>, value: Value) -> Result<(), HeapWriteError>;
    fn equal(&self, a: &Value, b: &Value) -> Result<bool, HeapFault>;
    fn normal_key(&self, value: &Value) -> Result<Option<NormalKey>, HeapFault>;
    fn reserve_logical(&mut self, bytes: LogicalBytes, transient: &TransientRoots<'_>) -> Result<(), HeapWriteError>;
    fn seal(&mut self, budget: SealBudget) -> Result<Option<SealedHeap>, HeapFault>;
    fn evict(&mut self) -> Result<(), HeapFault>;
    // and the remaining views and edits in the declaration
}
```

- Owning crates: `lash-kernel-state` (the paged heap), `lash-kernel-doc`
  (value and native contracts), `lash-kernel-vm` (the machine).
- Implementing lane: hp-heap. Consumers: hp-memory (through `CensusGraph`),
  hp-life (through `HeapLifecycle` and `FamilyBase`), and every native crate.

**How natives and the VM reach the heap after KIR.** `KernelMachine` owns
one `PagedHeap: SemanticHeap` and reaches it monomorphically. KIR ops call
the checked edits: `write_index` becomes `list_write`, `map_write` or
`set_write`, and `remove_index` becomes `list_remove` or `table_remove`. A
native call passes `&mut PagedHeap` as `&mut dyn NativeHeap` in a
`NativeCall`. The VM builds `TransientRoots` from its register window, its
pins, the call's arguments and its fresh allocations for every reservation.
The native memory retry in today's `call_lib`
(`machine/ops.rs::call_lib`, the `NativeError::Memory` collect-and-retry
loop) is deleted: refinement happens inside `reserve_logical` and
`NativeHeap::reserve`, so no native is re-executed. A page miss blocks inside
the port with the Rust stack intact.

Invariants:

- No `&mut` object, `&mut Vec<Value>` or mutating iterator escapes the heap.
- Before an edit is readable, it has updated together: the collection path,
  the directory head, the logical-cost delta and the identity-edge summary.
- Alias writes through any `EntityId` are seen by every holder. Equal but
  distinct objects stay distinct.
- A point update touches one bounded leaf and its index path. Unchanged
  children keep their sealed links without being rescanned.
- An editable page belongs to one epoch and changes in place until the next
  seal. A sealed page never changes.
- A length read touches the descriptor, never the payload.
- Map and record lookup and insertion sequence change together. First-written
  key spelling, live iteration and reinsertion at a new sequence hold
  (`K-KEY-003`, `K-ITER-003`).
- Physical page sharing never merges identities.

Errors: guest errors are `HeapWriteError::Raised { kind, message }`, the
guest bound is `HeapWriteError::Memory` and `NativeError::Memory`, and
infrastructure is `HeapFault` (in `MachineError::Heap`, `OpenError::Heap`
and `SealError::Heap`).

Laws:

- Kernel unit, `//crates/lash-kernel-state:lash-kernel-state__unit_test`, in
  `crates/lash-kernel-state/src/paged/laws/heap.rs`:
  - `a_point_write_in_a_huge_list_seals_one_leaf_path`
  - `a_tiny_update_among_many_objects_seals_one_directory_path`
  - `aliases_see_one_mutation_across_seal_and_open`
  - `cycles_seal_as_a_page_dag`
  - `map_iteration_survives_removal_and_reinsertion_across_seal`
  - `a_text_length_read_touches_no_payload_page`
  - `equal_payloads_share_one_canonical_root`
- Kernel unit, `//crates/lash-kernel-vm:lash-kernel-vm__unit_test`, in
  `crates/lash-kernel-vm/src/laws/paged.rs`:
  - `a_page_fault_mid_native_neither_reruns_nor_recharges`
  - `a_missing_page_is_infrastructure_never_guest_absence`
  - `every_charge_is_the_same_on_the_paged_heap`, the KIR charge-bound
    differential re-run under even, odd and body layouts

- **Ownership amended (2026-10-14, owner message 328022).** hp-heap owns
  the running Machine's CellStart `adopt`, terminal `carry`, and frozen
  outcome `freeze_datum`/`frozen_charge`/`instantiate` call sites alongside
  its running-Machine rewrite. hp-life completes S8 lifecycle, frozen
  saved functions and their layer laws; no VM callsite handoff is needed.
  The integrator owns prepared saved-capture loading and composed reproof.

### S2 Identities

Declaration: `crates/lash-kernel-doc/src/heap.rs`, with `FamilyId`,
`EntityId { family, ordinal }`, `PageId([u8; 32])`, `PageGrant`, `PageRef`,
`LogicalBytes` and `Identity { Object(EntityId), Task(TaskId) }`. The owning
crate is `lash-kernel-doc`, and hp-heap implements it.

- `ref(x)` is `Value::Ref(Identity::Object(id))` for the entity `x` names,
  and `same` compares two `EntityId`s for equality. As a key, a ref
  normalizes to its identity's canonical tokens (D-KEY).
- No integer conversion or debug number of an entity is guest-visible. A
  diagnostic that needs numeric labels assigns ranks while it produces
  itself.
- A `PageId` names immutable bytes, and an `EntityId` names a mutable object.
  No API converts one into the other. A `PageId` is never evidence of
  durable ownership.
- `PageId` is the same 32 bytes as the store's `NodeId`. The conversions
  (`From`) live in `lash-core-store`, which depends on `lash-kernel-doc`.

Laws, kernel unit (`lash-kernel-vm`, `laws/paged.rs`):

- `ref_same_and_key_equality_follow_entity_ids`
- `equal_frozen_subtrees_instantiate_distinct_entities`
- `no_guest_observation_depends_on_ordinals`, a differential that runs one
  program over two allocation histories

### S3 `PageSource` and the page exchange

Declarations:

- `crates/lash-kernel-state/src/paged/source.rs`: `PageSource`,
  `PageBody`, `PageAnswer`, `PageKind` and the capacity constants.
- `crates/lash-vm-protocol/src/page.rs`: `PageExchangeId`,
  `PageAuthority`, `PageCapability`, `PageEnvelope`, `PageRequest`,
  `ReadPage`, `PageReply`, `PageRefusal`.

```rust
pub trait PageSource {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault>;
    fn seal(&mut self, page: PageBody) -> Result<PageRef, HeapFault>;
}
```

The implementing lane is hp-paging: `WirePageSource` in
`crates/lash-vm-worker/src/host.rs`, the parent page service and spool under
`crates/lash-vm-broker/src/kernel/pages/`, and an in-process source for
embedders without a parent. hp-paging also adds two message variants to
`crates/lash-vm-protocol/src/message.rs`:

- `WorkerMessage::Pages { id: PageExchangeId, request: PageRequest }`
- `ParentMessage::Pages { id: PageExchangeId, reply: PageReply }`

The worker sends a `Pages` request mid-slice under the run's fence and reads
exactly one reply, exactly as `Wire::read` does for a host read. That
exchange is the one beside `Wire::read` in `host.rs`. `Start.from`'s
`ParkedGraph` becomes a checkpoint reference (root digest, the root's grant,
admissions). `Parked` carries the candidate root and its header in place of a
`SealedGraphState`.

Invariants:

- A request names at most `READ_BATCH_PAGES` pages and `READ_BATCH_BYTES`
  bytes.
- Every answered envelope is re-hashed and re-framed by the worker before
  the kernel decodes it. Every sealed envelope is re-hashed by the parent.
- The parent authorizes each child of a sealed page by a capability or an
  earlier page in the same batch.
- Reading page P yields capabilities for exactly P's envelope children.
- The parent's memory per activation stays bounded: it keeps the lease, the
  MAC key and the spool index, never a set of visited pages.
- The page service runs in the parent's store and spool I/O lane and never
  checks out another VM worker (a one-worker pool still serves its child).
- Prefetch and eviction change latency only, never values or charges.
- One absolute activation deadline covers every page exchange. Progress
  frames do not extend it.

Errors: `PageRefusal` on the wire, `HeapFault` in the kernel. All of them
are infrastructure.

Laws:

- Worker unit, `//crates/lash-vm-worker:lash-vm-worker__unit_test`, in
  `crates/lash-vm-worker/src/page_laws.rs`, over a real worker process with
  a SQLite memory store behind the parent:
  - `a_forged_foreign_or_stale_capability_reads_nothing`
  - `a_fault_mid_native_resumes_on_the_same_stack`
  - `a_one_worker_pool_serves_pages_without_another_worker`
  - `a_fresh_worker_reads_the_header_and_touched_pages_only` (counted pages)
- Wire codec bounds in `crates/lash-vm-protocol/src/codec/tests.rs`:
  `page_exchanges_respect_batch_bounds`.

### S4 The execution root and its records

Declaration: `crates/lash-kernel-state/src/paged/root.rs`, with
`CheckpointRoot`, `HeapRoot`, `KeyedRoot`, `SequenceRoot`, `RootStatus`,
`EndedRecord`, `SavedMeters`, `WaitsSummary`, `TaskRecord`,
`TaskStateRecord`, `JoinerRecord`, `IncomingRecord`, `EndedTask`,
`FrameRecord`, `BindingRecord`, `BoundRecord`, `LoopRecord`,
`FinallyRecord`, `EnteredRecord`, `HeldRecord`, `WaitRecord`,
`WaitKindRecord`, `WaitStateRecord`, `CommittedOutcomeRecord` and
`ListJoinRecord`. The owning crate is `lash-kernel-state`.

The canonical encodings belong to hp-heap, which writes the `TokenCodec`
impls beside the paged heap in `lash-kernel-state`. The envelope belongs to
the store (`lash-core-store/src/checkpoint_graph/node.rs`), and its kind
registration to hp-publish.

The **page envelope** is the generic node envelope
`"LASH" | kind (1000–1008) | child_count | sorted child digests | body_len | body`.
Its digest is the blob-domain blake3. The body is graph tokens in which a
child reference is a slot of the child table. An envelope is at most 64 KiB,
and a `CHECKPOINT_ROOT` at most 4096 bytes.

Invariants:

- A root is complete. The machine needs nothing else to resume: no earlier
  root, no delta replay and no worker cache.
- An unbounded table is a tree root, so a fresh worker reads the root page,
  then only the paths it touches.
- A frame persists semantic coordinates only (D-FRAME).
- A wait persists its identity and kind, never its arguments: the parent
  supplies admissions at open.
- A committed outcome is a `FrozenRef`, delivered as a fresh family (S8).
- The waits table root is its digest (D-WAITS amended).
- `release` and `owner` are the existing ReleaseManifest and OwnerBinding
  nodes. The parent reads them from this header and never decodes the heap
  to find a release.

Errors: `OpenError::{DocumentMismatch, KernelVersion, Malformed, Heap}` and
`SealError`. A root of a superseded format (a `KernelWhole`) is refused, not
read.

Laws:

- Kernel unit, `crates/lash-kernel-state/src/paged/laws/root.rs`:
  - `every_record_round_trips_canonically`
  - `a_checkpoint_root_fits_its_root_bound`
  - `a_kernel_whole_root_is_refused`
- Kernel unit, `crates/lash-kernel-vm/src/laws/paged.rs`:
  - `a_frame_resumes_at_its_semantic_site_in_every_layout`
  - `every_step_seals_the_same_root_in_every_layout`, the KIR image law
    carried over to roots
- Integrator, SQLite: `//crates/lash-durable-test:format_fixtures__test` and
  `//crates/lash-durable-test:vertical_crash_proof__test`, re-recorded once
  on the integrated tree.

### S5 Staging: the spool and the candidate manifest

Declaration: `crates/lash-core-store/src/checkpoint_graph/stage.rs`, with
`StageSpool`, `CandidateManifest`, `ManifestEntry`, `ManifestSummary`,
`StageLeaseId`, `StageUsage` and `StageFault`. The owning crate is
`lash-core-store`: the seam is generic, carries no kernel types and is
reachable from both the broker and the stores. hp-paging implements the
spool in `lash-vm-broker`. hp-publish consumes the manifest.

```rust
pub trait StageSpool: Send {
    fn seal(&mut self, page: EncodedNode, protected: &dyn Fn(&NodeId) -> bool) -> Result<(), StageFault>;
    fn read(&self, page: &NodeId) -> Result<Arc<[u8]>, StageFault>;
    fn candidate(&mut self, root: NodeId, base: Option<NodeId>, commit: CommitId)
        -> Result<Box<dyn CandidateManifest>, StageFault>;
    fn settle(&mut self, commit: CommitId) -> Result<(), StageFault>;
    fn discard(self: Box<Self>);
}
pub trait CandidateManifest: Send {
    fn summary(&self) -> ManifestSummary;
    fn entries(&mut self, after: Option<NodeId>, limit: usize) -> Result<Vec<ManifestEntry>, StageFault>;
}
```

Invariants:

- The spool is content-addressed and disk-indexed with bounded buffers and a
  quota. It is never an unbounded parent map.
- A staged page's children are protected committed pages or pages staged
  earlier, so the staged graph is acyclic by construction.
- `candidate` walks the root's staged DAG only and stops at protected
  references. It drops unreachable staged pages and yields one ascending
  digest stream of `External` and `Fresh` entries. The stream is
  restartable by cursor, because the store reads it once for nodes, once for
  edges and again on a retry.
- Spilling neither publishes nor admits.
- Spool lifetime follows D-SPOOL.

Errors: `StageFault`. All of them are host outcomes; none is a guest memory
bound.

Laws: unit, `//crates/lash-vm-broker:lash-vm-broker__unit_test`, in
`crates/lash-vm-broker/src/kernel/pages/spool_laws.rs`:

- `a_manifest_is_the_reachable_staged_dag_in_digest_order`
- `staging_refuses_an_unprotected_child`
- `an_ambiguous_publication_keeps_its_spool_until_resolved`
- `a_quota_failure_is_a_host_outcome`

### S6 Store publication

Declaration: `crates/lash-durable/src/domain/pages.rs`, with `PageStore`,
`RootLeaseId`, `RootLeaseRequest`, `RootLease`, `StoredPage`,
`SemanticAdmission`, `CandidatePublication`, `CandidateReceipt`,
`RetirementBudget`, `RetirementProgress` and `RetirementBacklog`. The owning
crate is `lash-durable`, beside `graph_commit.rs`. hp-publish implements it
for the SQLite and PostgreSQL durable stores.

```rust
#[async_trait] pub trait PageStore: DurableStore {
    async fn acquire_root_lease(&self, request: RootLeaseRequest) -> Result<RootLease, DurableError>;
    async fn renew_root_lease(&self, lease: &RootLease) -> Result<RootLease, DurableError>;
    async fn release_root_lease(&self, lease: RootLeaseId) -> Result<(), DurableError>;
    async fn read_pages(&self, lease: &RootLease, pages: &[NodeId]) -> Result<Vec<StoredPage>, DurableError>;
    async fn commit_candidate(&self, tx: ActorTx, candidate: CandidatePublication, label: CommitLabel)
        -> Result<CandidateReceipt, DurableError>;
    async fn admission(&self, root: NodeId) -> Result<Option<SemanticAdmission>, DurableError>;
    async fn candidate_state(&self, exec: &ExecKey) -> Result<Option<(NodeId, CommitId)>, DurableError>;
    async fn retire_step(&self, budget: RetirementBudget) -> Result<RetirementProgress, DurableError>;
    async fn retirement_backlog(&self) -> Result<RetirementBacklog, DurableError>;
}
```

`commit_candidate` runs the existing fenced owner transaction, in the
existing `LockStep` order:

1. The graph gate is taken shared, then the fence.
2. The candidate's manifest is merged with the `ActorTx`'s own small staged
   nodes (ExecRoot, HostRoot, admissions) in global digest order. Externals
   are key-share locked and existence-checked, and fresh nodes are inserted
   in batches (canonical bytes are compared on conflict, as today).
3. Edges are inserted after every node row exists.
4. The domain writes (execution root, effect admissions, waits, host and
   session rows) and the `SemanticAdmission` are written.
5. A replaced root moves to a retirement hold (D-RET).

The store reuses `check_graph`'s direct-reference rule: it validates each
supplied envelope and its exact child table, and checks immediate external
references only. It never walks an old closure. Effects dispatch only after
the receipt confirms the commit.

New catalog sources are `RootSource::ReadLease` and
`RootSource::RetirementHold`. New tables (`root_leases`,
`retirement_holds`, `semantic_admissions`) go straight into both 1.0
baselines. Normal activation stops using `DurableReads::hydrate` for heap
roots; hydrate stays the generic reader of host roots (§7); the
integrator deletes it.

Errors: `DurableError::OwnershipLost` for a stale epoch; the graph refusals
`RevisionConflict`, `RootRetired`, `MissingNode`, `ContentConflict` and
`HashMismatch`; and a lease past expiry, which hp-publish adds to
`GraphRefusal` as `LeaseLost`. Each is infrastructure for the activation.

Laws:

- Store laws in `crates/lash-durable/src/laws/pages.rs`, mounted from
  `crates/lash-sqlite-store/src/graph_store_tests.rs` (SQLite memory and
  file, `//crates/lash-sqlite-store:lash-sqlite-store__unit_test`) and
  `crates/lash-postgres-store/src/postgres/graph_store_tests.rs`
  (PostgreSQL, service gate `pg-store`):
  - `a_candidate_installs_only_new_nodes_and_direct_references`
  - `publication_is_atomic_with_admissions_and_semantic_admission`
  - `a_leased_root_survives_concurrent_retirement`
  - `an_expired_lease_reads_nothing`
  - `retirement_advances_in_bounded_steps_and_reclaims_everything_unowned`
  - `the_lost_receipt_resolves_by_commit_identity`
  - `publication_keeps_global_digest_lock_order`
- The storage differential:
  `//crates/lash-sim:cross_backend_store_differential__test`, where the new
  methods join the sweep and `store_trait_surface_is_fully_gated`.
- Crash cuts, run by the integrator in `vertical_crash_proof`: after a
  spill, after candidate sealing, after node insertion, after root
  publication, and a lost receipt.

### S7 The memory census

Declaration: `crates/lash-kernel-state/src/paged/census.rs`, with
`AllocationCensus`, `CensusGraph`, `AccountingCertificate`,
`AccountingMetric`, `EntityCost`, `LogicalPermit`, `PendingReservation`,
`ReservationDecision`, `RefinedDecision`, `TransientRoots`, `CensusFault`
and `MIN_ENTITY_COST`. The owning crate is `lash-kernel-state`. hp-memory
implements `AllocationCensus`; hp-heap implements `CensusGraph` and the
persisted summaries.

```rust
pub trait AllocationCensus {
    fn restore(certificate: AccountingCertificate, bound: LogicalBytes) -> Result<Self, CensusFault>;
    fn added(&mut self, cost: LogicalBytes) -> Result<(), CensusFault>;
    fn changed(&mut self, before: LogicalBytes, after: LogicalBytes) -> Result<(), CensusFault>;
    fn reserve(&mut self, bytes: LogicalBytes) -> ReservationDecision;
    fn refine(&mut self, graph: &mut dyn CensusGraph, transient: &TransientRoots<'_>,
              pending: PendingReservation) -> Result<RefinedDecision, CensusFault>;
}
```

The **certified upper bound** U is the census's local costs plus the
persistent roots' costs. `reserve` grants when checked `U + bytes <= bound`.
Otherwise, and on overflow, it returns `NeedsPrecision`.

The **exact refinement trace** freezes mutation. It traces from
`CensusGraph::persistent_roots` and from `TransientRoots`: registers, pins,
native arguments, fresh entities and outstanding reservations. It follows
`entity_edges` (edge summaries, never payload bytes) and counts
`task_result` only through reachable handles. It sweeps what it did not
reach, sets U to the exact total and decides again. Only
`RefinedDecision::Refused` becomes `Bound::Memory`.

The **persisted summary** is `HeapRoot::accounting`: the metric, U, the
entity count and the refinement count. U is labelled an upper bound and is
never reported as live bytes.

Invariants:

- No collection runs to park, resume or release a worker.
- No trace state is ever persisted, and there is no concurrent collector.
- Losing a reference never subtracts its referent.
- Overflow forces precision. It never wraps and never grants.
- An unreachable cycle never causes a refusal.
- Eviction and residency never change a verdict.
- A refinement that runs out of host work, or faults, is `CensusFault`
  (infrastructure), never `Bound::Memory`.
- An admitted certificate is trusted inductively. One from a foreign image
  is validated in full before it can grant anything (D-ADM).

Laws:

- Kernel unit, `crates/lash-kernel-state/src/paged/laws/census.rs`:
  - `overflow_refines_and_never_grants`
  - `an_undercounted_certificate_is_refused_before_it_grants`
  - `last_and_nonlast_alias_removal_keep_u_an_upper_bound`
- Kernel unit, `crates/lash-kernel-vm/src/laws/census.rs`:
  - `an_unreachable_cycle_never_fails_an_allocation`
  - `the_verdict_is_the_same_warm_evicted_and_reopened`
  - `refinement_counts_registers_native_args_and_reservations`
  - `a_large_partially_shared_detach_frees_only_the_dead_part`
  - `an_interrupted_refinement_is_infrastructure`
  - `parks_never_collect`
  - `census_garbage_stays_within_the_logical_bound`

### S8 Frozen families and cell carry

Declaration: `crates/lash-kernel-state/src/paged/family.rs`, with
`HeapLifecycle`, `FamilyBase`, `FrozenRef`, `FrozenSummary`, `FamilyRecord`,
`MembershipRun`, `SessionRoot`, `DataOnlyCertificate`,
`CarryClassification` and `CarryRoots`. The owning crate is
`lash-kernel-state`. hp-life implements `HeapLifecycle` and `FamilyBase`.
hp-heap calls `FamilyBase` on a directory miss and `instantiate` when it
delivers an outcome.

```rust
pub trait HeapLifecycle {
    fn freeze(&mut self, value: &Value, data_only: bool) -> Result<FrozenRef, HeapWriteError>;
    fn instantiate(&mut self, frozen: &FrozenRef) -> Result<Value, HeapWriteError>;
    fn classify_carry(&mut self, bindings: &KeyedRoot) -> Result<CarryClassification, HeapFault>;
    fn carry(&mut self, roots: CarryRoots) -> Result<SessionRoot, HeapFault>;
    fn adopt(&mut self, session: &SessionRoot) -> Result<KeyedRoot, HeapFault>;
}
pub trait FamilyBase {
    fn base_object(&mut self, entity: EntityId, frozen: &FrozenRef,
        pages: &mut dyn PageSource) -> Result<Option<ObjectHead>, HeapFault>;
}
```

The **cell-carry API**:

- **Carry roots.** A terminal cell selects `CarryRoots`: its bindings and its
  saved-function bundle nodes. `carry` seals them into a `SessionRoot`, which
  holds the heap root, the binding table, the function table and the
  certificate.
- **Next cell.** The next cell opens `SessionRoot.heap` with
  `SemanticHeap::open` and calls `adopt`. Entity identities are kept, and
  nothing is renamed, loaded or copied.
- **Data-only certificate.** A `DataOnlyCertificate` of zero closures and
  zero task edges over the whole census makes classification read nothing.
  Otherwise one union traversal with shared visited state classifies every
  candidate binding (`K-SES-003`).
- **Fork isolation.** A fork shares the session root's pages and owns its own
  directory version. Edits never cross forks (D-FAM).
- **Saved functions.** A saved function's captures are frozen once, at the
  semantic boundary, into a `FrozenRef` inside its bundle. They are never a
  pointer into the live directory.

Invariants:

- Equal frozen subtrees at distinct positions instantiate distinct entities.
- A mutation never changes the frozen base.
- A swept ordinal stays absent and never falls back to its frozen body.
- Membership is checked before the base is read.
- Every held identity, including a ref used as a key, is a trace edge.
- An instantiation reserves its whole logical cost and identity range up
  front, without building entities.
- Freezing rejects a cycle or a non-data value where `K-EFF-002` requires
  data.

Errors: `HeapWriteError` (`Raised` for non-data, `Memory` for reservation)
and `HeapFault`.

Laws:

- Kernel unit, `crates/lash-kernel-vm/src/laws/lifecycle.rs`:
  - `a_noop_next_cell_copies_nothing`
  - `a_fork_isolates_later_mutations`
  - `a_frozen_capture_ignores_later_live_writes`
  - `a_swept_family_ordinal_never_resurrects`
  - `a_lazy_result_length_read_instantiates_nothing`
  - `data_only_carry_reads_no_binding`
- Kernel unit, `crates/lash-kernel-dialect/src/saved/laws.rs`:
  `a_saved_function_bundle_keeps_its_frozen_capture`.
- SQLite, `//crates/lash-durable-test:rlm_session_globals__test`: carried
  aliases survive cell boundaries and worker resets.

## 4. Layer ownership

Each lane edits only the files in its row. A row's "new files" are created
by that lane. Seam declaration files (§3) are read-only for every lane.

| Lane | Owns and edits | Implements | Consumes |
| --- | --- | --- | --- |
| hp-heap | `lash-kernel-doc/src/{value,native,lib,graph_codec,graph_codec_forms}.rs`; new `lash-kernel-state/src/paged/{tree,directory,payload,codec,heap_impl,laws/heap,laws/root}.rs`; `lash-kernel-vm/src/{heap,interface,data,costs,functions,compile*,lib}.rs`; `lash-kernel-vm/src/machine/{mod,exec,ops,stream,actions,steps,observing}.rs`; new `lash-kernel-vm/src/machine/paged.rs`; `lash-kernel-vm/src/laws/{paged,stream,forms,bounds,tasks,embedder,fast,observe,layout}.rs`; `lash-kernel-lib/**`; `lash-ext-*/**`; `lash-vm-library/**`; the `Value` uses in `lash-kernel-exec`, `lash-kernel-check`, `lash-dialect-typescript` and `lash-dialect-python`; `lash-kernel-conformance` machine adapters | S1, S2, S4 encodings, `CensusGraph` | S3 (as `PageSource` test double), S7 (`AllocationCensus`), S8 (`FamilyBase`, `instantiate`) |
| hp-memory | new `lash-kernel-state/src/paged/census_impl/**` (U ledger, reservation, tracer, sweeper, mark state with spill, certificate validation); new `lash-kernel-state/src/paged/laws/census.rs`; new `lash-kernel-vm/src/laws/census.rs` | S7 `AllocationCensus` | S1 (`CensusGraph`, `TransientRoots`) |
| hp-paging | `lash-vm-worker/src/{host,worker,entry,service}.rs`; new `lash-vm-worker/src/page_laws.rs`; `lash-vm-protocol/src/{message,codec,bounds,outcome}.rs` (variants, bounds, refusals); `lash-vm-client/src/{machine,pool,wire,ipc,config}.rs`; new `lash-vm-broker/src/kernel/pages/**` (page service, capability MAC, spool, `spool_laws.rs`); `lash-core-store/src/checkpoint_graph/node.rs` (a public framing function for direct semantic kinds) | S3, S5 `StageSpool` | S6 (`acquire_root_lease`, `read_pages`) |
| hp-publish | `lash-durable/src/domain/{graph,graph_commit,mod}.rs`; `lash-durable/src/{tx,port}.rs` (only if `commit_candidate` needs `ActorTx` accessors); new `lash-durable/src/laws/pages.rs`; `lash-core-store/src/checkpoint_graph/{registry,roots,bodies,refusal,commit}.rs`; `lash-sqlite-store/src/durable/**`, `lash-sqlite-store/src/schema.rs`, `lash-sqlite-store/src/graph_store_tests.rs`; `lash-postgres-store/src/postgres/durable/**`, its schema, `migrate.rs`, `schema_shape.rs`, `schema-shape.txt`, `graph_store_tests.rs`; `lash-sim/tests/cross_backend_store_differential/**`; `lash-vm-broker/src/kernel/store.rs` (lease acquisition, `commit_candidate`, `pin_release` from the root header) | S6, D-RET, D-ADM | S5 (`CandidateManifest`) |
| hp-life | new `lash-kernel-state/src/paged/family_impl/**`; `lash-kernel-vm/src/machine/session.rs`; new `lash-kernel-vm/src/laws/lifecycle.rs`; `lash-protocol-rlm/src/executor/session.rs`, `session/graph.rs`; `lash-kernel-dialect/src/saved.rs`, `saved/graph.rs`, new `saved/laws.rs` | S8 | S1 |
| hp-integrate | `lash-vm-broker/src/kernel/{state,run,graph,ledger,value}.rs`; `lash-vm-runtime/src/process/**`, `formats.rs`; `lash-core-execution/src/runtime/actor/process/state_graph.rs`; `lash-kernel-migrate/**`; `lash-kernel-conformance/src/{migration,smith}/**`; `lash-vm-protocol/src/graph_state*.rs`; `lash-durable/src/domain/mod.rs` (`DurableReads::hydrate`) and every implementation; `lash-durable-test/**` fixtures and crash laws; `lash-perf/**`; `examples/**`;  `docs/kernel/**`, `docs/adr/**`; facade and schema exports | end-to-end wiring of activation (lease → page service → seal → candidate → commit) | all |

**Shared touch points** and who resolves each:

1. **Module declaration lines.** These are `lash-kernel-state/src/paged/mod.rs`,
   `lash-kernel-vm/src/laws/mod.rs` and the crate `lib.rs` files. Each lane
   adds only its own `mod` lines. The integrator takes the union.
2. **Generated files.** These are `BUCK` files and
   `tools/buck2/target-inventory.json`. Each lane regenerates them with
   `kiln sync`. The integrator regenerates them after merging and never
   hand-merges.
3. **Native memory retry** (`machine/ops.rs::call_lib`). hp-heap deletes the
   retry and calls `reserve_logical`; hp-memory never edits `ops.rs`.
4. **Outcome delivery** (`machine/actions.rs`). hp-heap calls
   `HeapLifecycle::instantiate`, and hp-life implements it.
5. **Directory miss.** hp-heap calls `FamilyBase`, and hp-life implements
   it.
6. **Broker activation** (`lash-vm-broker/src/kernel/run.rs`). Only the
   integrator wires paging and publication together. Each lane proves its
   half with its own laws.
7. **Node registry** (`registry.rs`). hp-publish adds the semantic range. The
   integrator deletes `KERNEL_WHOLE`, `TERMINAL_WHOLE` and
   `SESSION_EXECUTION`.
8. **`message.rs`.** Only hp-paging edits it. The integrator deletes
   `ExportGraph`/`ExportedGraph` and `SealedGraphState` with their callers.
9. **Fixtures and committed images.** Only the integrator re-records them,
   once, on the integrated tree rebased on current main.
10. **Kernel rules and ADRs.** The integrator edits them. Each lane's report
    names the rule text its change needs: the `K-MACH-001` clarification,
    `K-BND-003`'s upper-bound wording, demand-validation timing and the
    saved-state shape.
11. **The seams.** Changes to seam files need an orchestrator ruling and land
    here first.

## 5. Deletion inventory

The anchors are at `195ed4db93`. "I" marks a deletion only the integrator
can make, because its callers span lanes. The deleting lane removes the
symbol together with every caller in its own files.

| Path : symbol | Deleted by |
| --- | --- |
| `lash-kernel-vm/src/heap.rs`: `Table` (105), `Obj` (265), `Held` (342), `Chunk` (356), `Heap` (392) and `Heap::{get_mut,restore,written,collect}` (457, 502, 559, 606), `NativeView` and its `NativeHeap` impl (668, 688) | hp-heap |
| `lash-kernel-vm/src/machine/ops.rs`: `get_mut` writes in `write_index` (271), `write_record_field` (335), `remove_field` (347), `remove_index` (358); the native memory retry around `NativeView` (527) | hp-heap |
| `lash-kernel-vm/src/machine/exec.rs:572`: the `Obj::Variable` `get_mut` write | hp-heap |
| `lash-kernel-doc/src/native.rs`: infallible `NativeHeap`, `NativeCall`, `NativeError`, `Element`; `value.rs`: `Value`, `Object`, `ObjectId`, `Identity`, `ClosureObject` (replaced by `heap::*`) | hp-heap |
| `lash-kernel-vm/src/machine/session.rs`: `load` (145), `reach_closures` (250), `reach` (286), `carried` (328), `named` (348), `rename` (366) | hp-life |
| `lash-protocol-rlm/src/executor/session.rs`: `SLOT_OBJECTS` (51), `adopt` (406), `data` (475), `binding_data` (504), `check_reached` (535), `value_objects` (559), `object_objects` (579), `rename_value` (599), `rename_object` (622) | hp-life |
| `lash-protocol-rlm/src/executor/session/graph.rs`: `SessionExecutionBody` (32), `AliasRow` (49) and their codecs | hp-life |
| `lash-kernel-dialect/src/saved.rs`: `SavedFunction::captures` as `Datum` (48); `saved/graph.rs`: `Bundle::stage`/`assemble` Datum flattening (132, 164) | hp-life |
| SQLite `durable/graph.rs::reclaim` (663) and PostgreSQL `durable/graph/reclaim.rs::reclaim` (83): synchronous last-owner peel on commit (replaced by holds and `retire_step`) | hp-publish |
| `lash-vm-broker/src/kernel/store.rs::pin_release` (942): decoding a `KernelWhole` to find the release | hp-publish |
| `lash-vm-broker/src/kernel/store.rs`: `hydrate` calls (975, 1024) on normal activation | I |
| Kept by ruling (§7): `lash-durable/src/domain/mod.rs:503`: `DurableReads::hydrate` and `HydratedGraph`; SQLite `durable/graph.rs::hydrate_in` (561); PostgreSQL `durable/graph.rs::{hydrate_in, hydrate_owner}` (661, 1053) | I |
| `lash-kernel-state/src/schema.rs`: `ParkedRun` and the whole JSON schema; `fragments.rs`: `Baseline`, `Saved`, `save`, `load`; `bin/parked_schema_generator.rs` | hp-heap |
| `lash-kernel-state/src/whole.rs`: `KernelWhole` (66), `WholeTask`, `WholeWait`, `capture` (196), `restore` (253); `terminal.rs`: `TerminalWhole` (21) | hp-heap |
| `lash-kernel-vm/src/whole.rs`: `export_whole` (69), `import_whole*` (112, 129), `terminal_whole` (181), `terminal_end` (273) | hp-heap |
| `lash-kernel-vm/src/machine/parked.rs` and `parked/{capture,restore}.rs`: `export` (419), `save` (440), `import` (458); `machine/mod.rs`: `type Parked` (879), `save` (481), `import_with_*` (430, 460), `Machine::{export,import}` (946, 950) | hp-heap |
| `lash-vm-broker/src/kernel/state.rs`: `WholeState` (177), `export_parked` (287), `export_whole_state` (303), `OpenedRun` (333), `open_whole` (349), `capture_whole` (408), `import_parked` (487), `import_whole_state` (519), `export_end*` (640, 655), `read_end*` (688, 698) | I |
| `lash-vm-protocol/src/graph_state.rs`: `SealedGraphState`, `GraphNodeBytes`, `GraphRoot`; `message.rs`: `StartFrom::ParkedGraph`, `ParkedGraph`, `ParentMessage::ExportGraph`, `WorkerMessage::ExportedGraph`, `Parked.state` | I |
| `lash-vm-client/src/machine.rs`: `export` returning `WholeState` (482), `resume_sealed` (320); `pool.rs::export_graph` (650) | I |
| `lash-vm-worker/src/worker.rs`: `export_graph` (767) and the `staged` `SealedGraphState` (46, 402) | I |
| `lash-vm-runtime/src/process/state.rs`: `SealedRun` (101), base64 whole material; `process/migrate.rs` whole carry | I |
| `lash-core-execution/src/runtime/actor/process/state_graph.rs`: `stage_engine_state`, `write_engine_state`, `read_engine_state`, `load_engine_state` for kernel processes (36, 91, 119, 144); opaque foreign-engine bytes stay | I |
| `lash-kernel-migrate/src/carry.rs::carry` (31) over `ParkedRun`: deleted; the paged parked-run migrator is FIG-6233 (§7) | I |
| `lash-core-store/src/checkpoint_graph/registry.rs`: `KERNEL_WHOLE` (16), `TERMINAL_WHOLE` (27), `SESSION_EXECUTION` (29) rows. `WHOLE_STREAM`, `STRUCTURE_BLOCK` and `STREAM_INDEX` stay: other enveloped kinds use them. | I |
| `lash-core-store/src/checkpoint_graph/encode.rs`: the FIG-6157 encoding cache stays (it is generic, used by other domains); only the kernel's whole-input encode path through it goes | I |
| Tests, fixtures and docs whose subject is the removed shapes: `lash-kernel-state/src/tests.rs`; `lash-kernel-vm/src/laws/{parked,whole}.rs`; `lash-vm-worker/src/graph_laws.rs`; `lash-vm-protocol/src/graph_state_tests.rs`; `lash-vm-broker/src/kernel/graph_tests.rs`; `lash-durable-test` format fixtures; `docs/kernel/parked-state.md`. Each deleted test is listed in the commit with the law that now covers its property. | I |

## 6. Risks to independent building

- **hp-heap is the critical path.** Every other lane compiles against the
  declarations alone, but its laws need a working paged heap. hp-memory
  tests against a scripted `CensusGraph`. hp-life is stacked on hp-heap.
  hp-paging tests the exchange with a scripted `PageSource` client until a
  machine runs on the paged heap.
- **The `Value` cutover is large.** About 18 `lash-kernel-lib` files, the
  three `lash-ext-*` crates, both dialects, the VM and the conformance
  adapters move to fallible, descriptor-based values in one lane.
- **The first end-to-end proof is late.** Paging and publication meet only
  in the integrator's `run.rs` wiring, so the first end-to-end proof is the
  integrator's.
- **Declared signatures can prove wrong.** A trait signature may turn out
  wrong once implemented: for example, `SemanticHeap::open` as a trait
  constructor, or reads through `&self` with interior residency. Such a
  change is a seam ruling, not a local edit.

## 7. Integration rulings

The integrator's end state, as ADR 0145 states it:

- **Host graph reads.** `DurableReads::hydrate` and `HydratedGraph` stay the
  generic reader of host roots: turn phases, release pins, process engine
  state, execution control nodes and session manifests. A hydrate never
  descends into a semantic page: a host node's semantic child is returned as
  itself, and a read asked to start at one is `GraphRefusal::SemanticPage`.
  Heap pages are read only under a root lease through the page exchange.
- **One publication identity (FIG-6197 F1).** `check_candidate` relates the
  owner write (`CandidateOwner::ExecRootPut` or `RunFactAppend`), the
  manifest summary's root and base, the semantic admission and the commit;
  `candidate_state` and resolution use the same pair. A mismatch is
  `GraphRefusal::PublicationMismatch`.
- **Spool child provenance (FIG-6197 F2).** A staged page's child that was
  committed and reused is read and sealed under the root lease's authority;
  a staged child under the stage lease's (`StageSpool::contains`).
- **Checked tree roots (FIG-6197 F3).** External `KeyedRoot` and
  `SequenceRoot` are `Empty | NonEmpty { page, len, cost }`, refused invalid
  at construction and decode, before any `Tree::open`.
- **Kernel process publication (P1).** A `kernel_run` step outcome owns its
  candidate: `KernelRunOutput::Parked` names the `CheckpointRoot` page, the
  outcome's `RunFact` record holds it, `commit_candidate` runs in the
  outcome's transaction, and `ProcessStateBody::Kernel { root, bytes }`
  names it from the advance on.
- **Retirement.** Multi-hold steps bounded by a node budget; a node
  maintenance task runs them beside lease renewal; backpressure on
  `retirement_backlog`; every limit in `DurableSettings.retirement` with a
  default and a hard maximum. Locks follow one global order: digest order,
  with root-catalog and hold rows before a step's peel.
- **Parked-run carry.** `lash-kernel-migrate`'s whole-image `carry` is
  deleted. A parked run whose kernel version or helper release a build does
  not interpret is refused with `ParkedRefusal::StateNotCarried`, kept, and
  resumable on a build that retains it; the paged migrator is FIG-6233.
  Session bindings across a release change are covered by `classify_carry`
  and `adopt`.

### S1/S4/S8 (amended): descriptor summaries and lifecycle attachment

Tuple and error descriptors retain inherited family context at every immutable
nesting depth. Errors store their data as a bounded resident value or a one-cell
sequence tree; their depth, local cost, identity-edge flag and task-edge count
are descriptor metadata. Reading error data is fallible. Tuple and tree summaries
carry exact task-edge counts, validated against each loaded canonical page.
Closure captures are name-keyed variable identities in ascending name order.

The heap header persists the conservative `DataOnlyCertificate`; allocation,
replacement, session-root attachment and sweeping maintain it. A direct task
binding contributes even when the directory is empty. Machine open, like start,
receives the admitted `Box<dyn AllocationCensus>`. Lifecycle installs an admitted
`Tree<BindingRecord>` through `PagedHeap::set_session_root`, with one holding-cost
change and no rewriting of its leaves. Precision sees the proposed binding root.

### S8 (amended): membership owns its live aggregate

Family membership carries its own live-ordinal aggregate, alongside subtree
length and cost. Membership edits recompute it from canonical runs; loaded pages
validate their aggregate locally. Sweeping a live ordinal decrements it once;
sweeping a dead ordinal changes nothing. When it reaches zero the same heap edit
removes the family row, frozen base reference and conservative population
contribution. An empty family registers no row. The family allocator remains
monotonic, and pressure enumeration includes untouched live family members.

### S1 (amended): native equality

Structural `equal(a, b) -> Result<bool, HeapFault>` belongs to `NativeHeap`.
`SemanticHeap` inherits it; first-party natives use the same cycle-safe
implementation as machine operations, with storage faults propagated intact.

### S4 (amended): one execution-record grammar

Public `FrameRecord` and `TaskRecord` are the paged working records. Frames hold
binding, loop, finally, iterated, departing and optional argument trees. Tasks
hold joiner, occurrence and frame trees, plus bounded state and obligation
metadata. The `result_retained` flag distinguishes retired ended-result payloads.
The former Vec-snapshot declarations/codecs and `HeldRecord` are deleted. Sealing
prepares these same working trees; it never assembles another frame/task image.

`ListJoinRecord.members` is a `Tree<TaskId>`, with the canonical tree codec;
the former Vec grammar is deleted. Other unbounded S4 record vectors likewise
become working trees, rather than whole records rebuilt when parking.

Statement roots' local holding costs and outstanding native reservations are
included in U before conservative admission. Installing a value/object/family
consumes its reservation credit instead of charging it twice. Under pressure,
a directory or root replacement traces the proposed private graph, including
its fresh target; it does not retain a reference solely because the overwritten
body held it. Infrastructure failures still resume from the committed root.
Resident immutable descriptors validate local cost, depth and edge aggregates
against their canonical members before their summaries may serve as oracles.

### S1 (amended): native key normalization

`NormalKey` belongs to `lash_kernel_doc::heap`, with the state layer re-exporting
it. `NativeHeap` owns `normal_key(&Value) -> Result<Option<NormalKey>, HeapFault>`;
`SemanticHeap` inherits it. First-party natives and certification heaps share
the heap's normalization path, using its bounded tokens/digest as their host
index key. The Value-backed library Key and its infallible hash/equality are
deleted. Insertion-sequence entries retain the first guest spelling.

### Ownership amendment (2026-10-14): lifecycle call sites

hp-life completes the lifecycle implementation; hp-heap merges its final head
and owns the running-Machine adopt, carry and frozen outcome/argument delivery
call sites. The deleted rename/reach/copy session paths are not rebuilt.

### S1 (amended): descriptor field names

`FieldName<'a> { Static(&'a str), Text(&'a TextRef) }` belongs to `doc::heap`, with
state re-exporting it. `NativeHeap::record_get` takes this selector. Both forms
use the same normalization and index; a dynamic field lookup materializes no
text and reserves no guest memory. Moving an existing declaration onto the
native/doc seam with its existing algorithm for this purpose is pre-approved.

S4 amendment (hp-heap, approved 328942): `Program.release: PageId` holds the
release selected by composition; the prepared library has no release identity.
`CellStart.owner: PageId` holds the activation owner supplied by composition.
Opening a checkpoint retains its admitted release and owner; sealing copies both
exact identities into the next checkpoint without inference or placeholders.

S1 amendment (hp-heap, approved 329218): `NativeHeap.payload_visit` also reads
canonical little-endian magnitude bytes of paged integers, preserving the sign
in the descriptor and validating the high digit against its bit summary.
Outbound Datum copy-out uses read-only payload traversal and creates no guest
reservation (`K-EFF-002`). Native foreign codecs retain explicit materializers.

S4/S7 amendment (hp-heap, approved 329341): `FrameRecord.iterated` is a
`Tree<IteratedRecord>`. This role wrapper uses the canonical Value codec and
retains normal identity/task edges. It charges only tuple members for tuple
cursors and zero for list/map/set identity cursors, preserving K-BND-003's
existing iterator holding metric without a tuple/identity Value header.

S4 amendment (hp-heap, approved 329496, correcting 329476): `WaitRecord`
is keyed by wait id and holds `task: TaskId`, `kind: WaitKindRecord`, and
`phase: WaitPhase`. `WaitPhase` is `Issued { request: FrozenRef }`, `Admitted`,
or `Committed(CommittedOutcomeRecord)`. Issuing freezes the request once without
guest entities; seal preserves it without dispatch or admission. Reporting and
admission are one fused park commit. Delivered frozen outcomes remain committed
until semantic resume removes the wait. The redundant wait id, identity, and
`WaitStateRecord` declaration/grammar are deleted; admitted requests remain in
the external committed admission record. Other transitions are corrupt records.

S4 companion amendment (hp-heap, approved 329563): `IncomingRecord::Outcome(WaitId)`
names a wait in `WaitPhase::Committed`. Opening a resuming task requires that wait
and phase; other states or a missing wait are corrupt records. Instantiating and
installing an outcome and removing its wait are one edit before any seal.

S1 relocation (hp-heap, approved 329783): `SemanticHeap.canonical_value(Value)`
returns a bounded descriptor through the existing immutable constructor algorithm.
It creates no entity or holding-cost change; checked publication still owns that
accounting. Existing paged immutable descendants are retained without reads or
resealing, including ErrorRef.data. Frame and session bindings call this before
publishing a resident native-created value.
## S7 (amended, 2026-10-14)

The orchestrator approved these additions for exact accounting:

- `CensusGraph::root_cost(&mut self) -> Result<LogicalBytes, HeapFault>`
  supplies persistent root/control holding costs, excluding entity referents
  and ended-task results.
- `value_cost(&mut self, &Value) -> Result<LogicalBytes, HeapFault>` supplies
  each transient holding location's LocalCostV1 cost.
- `value_edges(&mut self, &Value, &mut dyn FnMut(Identity) -> Result<(), HeapFault>) -> Result<(), HeapFault>`
  reads nested tuple/error identity summaries, including paged tuples,
  without assembling scalar payload bytes.
- `sweep(&mut self, &SweepSet) -> Result<(), HeapFault>` replaces the
  entity-only input. `SweepSet { entities: Vec<EntityId>,
  ended_task_results: Vec<TaskId> }` retires exactly the proved-unreachable
  entity memberships and ended-result payload references together. Task
  obligation metadata remains. A reached result is never discarded.
- `ended_tasks_after(&mut self, Option<TaskId>, usize) -> Result<Vec<TaskId>, HeapFault>`
  enumerates retained ended-result payloads, unique and ascending.

hp-memory defines the additions; hp-heap implements the real graph and
paged-tuple summary access. Sweep installs in bounded batches only after
marking completes. A sweep failure ends the activation without a guest
verdict or certificate publication; that activation must be discarded.

Trace state stays resident and bounded. There is no scratch writer, spill
mechanism or scratch page kind. The concrete census exposes a host-configurable
`trace_memory_budget`: default 8 MiB, hard maximum 64 MiB. Compressed identity
bitmaps and a block work-list retain pending edges without expanding an
identity queue or recursing through the heap. Exceeding the memory budget is
`HostResource::TraceBudget`, never a guest bound. A configurable host-work
limit similarly returns `HostResource::HostWork`. Neither limit changes a
completed exact decision. The earlier ownership-table mention of mark spill
is superseded by this ruling.

`AllocationCensus::added(&mut self, cost: LogicalBytes, entities: u64)`
replaces the one-entity signature. Its checked minimum is
`entities * MIN_ENTITY_COST <= cost`; multiplication overflow is a fault.
Ordinary allocation passes 1, and lazy instantiation passes the validated
`FrozenSummary`'s cost/count in one operation without constructing entities.

All identity visitors in `value_edges`, `entity_edges` and `task_result`
return `Result<(), HeapFault>`. A visitor error stops summary traversal
immediately and propagates unchanged, matching `persistent_roots`. The
census can therefore end traversal at its host-work or memory limit without
loading the rest of a large edge summary.

S4 amendment (hp-heap, approved 330736): CheckpointRoot.target is CellTargetKind
{ Main, Entry }; functions is KeyedRoot. Start retains the target and adopted
saved-function root; seal copies both and open attaches without tree reads.

S8 read-only projection (hp-heap, approved 330606): PagedHeap.frozen_datum
(&FrozenRef) -> Result<Datum, HeapFault> decodes the existing frozen grammar for
host reporting, without guest reservation or family allocation.

S1 transient refresh (approved 330825): set_transient_roots replaces
set_statement_roots. It takes borrowed value, pinned-value and fresh-identity
slices, reusing heap-owned buffers. Fresh identities are zero-cost census seeds;
release_statement_roots clears both buffers. Fast admission copies no roots.

S1 fused publication (approved 330913): set_execution_roots(tasks, joins,
registers: &[Value], pinned: &[Value], fresh: &[EntityId]) is the sole task, join
and transient root publication. Proposed records and zero-cost fresh seeds are
visible together before refinement. Supersedes set_task_root, set_join_root and
set_transient_roots; no other production caller needs those entry points.
FIG-6196 step counters are an integrator gate on 27c22f1672, not this lane.

D-IMM clarification (approved 331092): normalized keys are cached on resident
immutable descriptors only, outside the canonical grammar; reload starts an
empty cache. No persisted per-string or per-tuple digest is introduced.

Name representation (approved 331194): Name internally shares immutable text
through Arc<str>. Public methods, serde/schema/token text and content eq/hash
stay the same. Cloning a Value now allocates no name string during root refresh.
No cheap scoped allocation counter exists in these layers; buffer reuse and
Value clone behavior are verified by inspection.

S4 charge/typed-cause preservation (approved 331580): CommittedOutcomeRecord
adds Rejected(FrozenRef). It instantiates the frozen effect_result error on
semantic resume with zero inbound charge, while Failed retains its deep-size
charge. OpenError.MissingFunction preserves the typed FunctionId cause.

### S4 amendment: canonical nested trees (331503)

Every nested `Tree<T>` uses one root grammar: empty, an inline canonical
leaf body of at most 256 token bytes, or a page reference with its body byte
count. Small bodies always inline; oversized inline bodies and paged nested
roots at or below the threshold are refused. Tree cells and their cost,
task-edge and membership summaries are identical in both forms. External
`KeyedRoot` and `SequenceRoot` descriptors always name pages. Inline bodies
retain only their own authenticated child references, including when frozen
relative tokens acquire a receiving family's context. The list-specific
`Vec<Value>` body and its grammar are deleted.

### S4 amendment: bounded semantic metadata (332329)

`NameRef` is a bounded descriptor over `TextRef`, with short names inline
under the canonical 256-byte threshold and long names paged. `SiteRef` holds
semantic unit/path coordinates, never compiled offsets. `TaskIdentityRef`
is a content-addressed parent-reference node: spawning adds one node and
shares prior lineage. These descriptors belong to `lash-kernel-state::paged`;
the VM resolves them against admitted code and materializes full identities
only for explicit diagnostics. Closure metadata signatures follow these
descriptors. Laws cover 1 MiB binding/capture names, bounded per-task writes
along a 10k serial spawn chain, and exact diagnostic lineage.

### S5 amendment: bounded park diagnostics (332782)

A park reports `live: u64`, `outstanding: u64` and the sealed task-table
`KeyedRoot`. It neither enumerates tasks nor constructs ancestry. The task
tree's membership summary counts outstanding tasks; the saved live-task
meter supplies the live count. `Machine::tasks(root, filter, after, limit)`
explicitly examines at most `limit` rows (1–64), returns matching task IDs
and `TaskIdentityRef`s, and a continuation cursor. The limit bounds examined
rows even when none match. `Machine::task_identity(TaskIdentityRef)` resolves
full ancestry for an explicit diagnostic. Terminal task/deadlock errors keep
full identities. An issued intent freezes only its `AdmittedRequest`; request
projection resolves effect identity from the waiting task's retained ancestry,
semantic action site, occurrence and loop records at the host boundary.

Nested paged tree descriptors also carry a leaf/index hint, authenticated
against the root page kind on first read. A short index over a large tree
remains paged; the 256-token-byte inline rule applies to the complete leaf
body, and cannot mistake index bytes for the whole collection.

### S1 amendment: bounded function values (333044)

`Value::Function(TextRef)` holds the canonical name as a bounded text
descriptor, built with `NativeHeap::text`. Calls resolve it against admitted
code and use the existing missing-function error path. Function equality
compares canonical text; key normalization keeps tag 8 plus exact name text,
so resident and paged forms have identical keys. `Datum::Function(Name)` and
`Literal::Function(Name)` keep the boundary/code grammar. A 1 MiB function
name must fit in bounded binding, tuple and frozen row cells and round-trip
exactly through the datum boundary.

D-CAP/D-SEALTIME implementation: one weak dirty-page registry spans the
activation's trees. At 32 MiB of conservative physical accounting it seals
oldest working pages through PageSource, down to 24 MiB. A shared dirty node
becomes an immutable staged reference; copy-on-write reifies only the edited
path. Checkpoint sealing promotes staged references without reserializing them.
Stage quota and transport failures remain HeapFaults and leave the committed
root intact. Clean token and decoded-node residency share one 64 MiB allowance.
Decoded-node keys include cell type, family context and tree kind; inline keys
retain their token Arc so pointer reuse cannot alias cached bodies. Long names
and sites remain descriptors, with only bounded metadata and normal-form caches.

S4 metadata residency: admitted-name reuse retains the input Name Arc to guard
pointer identity; diagnostic-name reuse keys the payload page and its complete
TextSummary. Both tables draw from the same 64 MiB clean allowance. Site matching
uses bounded unit descriptors and semantic path cells, without sealing pages.

S5 amendment (hp-heap, approved 339533): `End::Finished(Box<Finished>)`.
Only terminal Rust layout changes: the bounded SessionRoot remains in
`Finished.carry` with the same fields/codecs. One allocation per finished run
keeps every `Step` from retaining 384 bytes. Guest results, carry and charges
are unchanged.

Final hp-heap measurements (real S7 census, FIG-6188): fanout 32; 64
cells/rows per leaf; 8192 target bytes; 4096 payload bytes; 256-byte inline
immutable/small-body threshold. A point update in a 100k-element list sealed
6 pages and 5105 bytes; its cold lookup read 5 pages. A point update among
100k small objects sealed 5 pages and 6896 bytes; its cold lookup read 4
pages. A 16 MiB text length loaded no payload pages. Counts include the
changed directory path and heap header. Full paths:
`paged::laws::heap::{a_point_write_in_a_huge_list_seals_one_leaf_path,
a_tiny_update_among_many_objects_seals_one_directory_path,
a_text_length_read_touches_no_payload_page}`.
