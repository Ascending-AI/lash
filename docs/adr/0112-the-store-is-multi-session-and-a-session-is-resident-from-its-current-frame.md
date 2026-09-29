# 0112: The store is multi-session, and a session is resident from its current frame

## Status

Accepted 2026-09-29 (FIG-4026). It pins the contract for one cutover that
lands FIG-1280 shape C and FIG-1628 phase one together. Nothing below
describes current behaviour unless it cites today's code. Four
implementation lanes build it against this record (§15).

Sam's rulings of 2026-09-29, on FIG-1280 and FIG-1628, are binding here:

- Shape C and phase one land as one clean cutover: no adapters, no dual
  reads, no transitional APIs.
- Resident memory is proportional to the current frame. It is not capped in
  bytes.
- A frame is the context window by design. Frames change on compaction and on
  `continue_as`, and the two are equivalent.
- Offload (FIG-1643) and pressure reporting (FIG-1644) are deferred.
- Until the 1.0 cut, the version freeze holds (FIG-3846). Stored shapes change
  in place, with no version bumps.
- Hosts never drive a turn (D5).

This record supersedes the byte budget, offload, pressure and version-bump
parts of the FIG-1626 design (`/workspace/notes/lash/fig1626-design.md`). Its
invariants I2, I4, I5 and I6 survive here as §5 to §7. It takes the place of
FIG-1645's ADR for phase one. It amends
[ADR 0057](0057-history-generations-accelerate-edge-authoritative-reads.md):
a window read adds a generation lower bound beside the existing ceilings, and
the head's frame pointer stays an accelerator that is always checked. The
plan it ratifies is `/workspace/notes/lash/tasks/lanes/wholehog-1628.report.md`.
Where today's code shows that plan to be wrong, the section says so.

## Context

Every citation below was read at fork HEAD `a8f9e693c1`.

**The store interface is per-session in name only.** `RuntimePersistence`
composes seven segments (`crates/lash-core-store/src/store/mod.rs:2039`).
Most mutations already name their session. Session reads such as
`load_session`, `load_session_head_meta`, `load_node` and `load_session_meta`
do not, and neither do `committed_turn_exists`, `drain_end_exists` or
`vacuum` (`crates/lash-core-store/src/store/runtime_persistence_decorator.rs:44-147`).
The multi-session interface already exists as `SessionStoreFactory`
(`crates/lash-core-execution/src/runtime/vocabulary.rs:405`), but as a second
class. Several of its operations default to `UnsupportedStoreOperation`
(`:443`, `:464`, `:558`, `:732`, `:753`). It answers `String` errors in two
places (`:429`, `:683`). And it copies operations the store already has:
`root_terminal` (`:549`) and `pending_turn_cancel_closure_pins` (`:621`).

**Only SQLite's handle holds state, and none of it is per-session.** SQLite's
`Store` holds a connection thread, a database keepalive, the fleet format, a
clock, options, a cancellation binding, a publication pause, a commit counter,
a registry flag, test probes and an `Arc<OnceLock<SessionId>>` identity latch
(`crates/lash-sqlite-store/src/lib.rs:195-217`). Its reads infer a sole
session, refuse ambiguity and latch it (`:462-490`). Admission latches inside
its transaction (`crates/lash-sqlite-store/src/persistence/session_commit.rs:1146`).
So do commits and parks (`:342`, `:310`). The factory opens a new connection
for every handle over one shared catalog
(`crates/lash-sqlite-store/src/session_store_factory.rs:190-253`). It also
hands out unbound stores (`:358`), and so does `SqliteStoreSet::open_store`
(`crates/lash-sqlite-store/src/backend.rs:322`). Vacuum refuses an unbound
handle (`crates/lash-sqlite-store/src/persistence/maintenance.rs:6-13`).
PostgreSQL's handle holds a pool, a clock, the fleet format, a required
identity, the cancellation binding and test counters
(`crates/lash-postgres-store/src/lib.rs:676-691`). The factory mints one per
call (`crates/lash-postgres-store/src/postgres/session_factory/store.rs:18`).
Perf decorates the store. It keeps every committed node id in a set that
grows with history (`crates/lash-perf/src/runtime_perf/store.rs:25-29`,
`:212`).

**An active session holds its whole root-to-leaf history.** SQLite's session
read loads the graph from generation zero up to the leaf
(`crates/lash-sqlite-store/src/persistence/session_commit.rs:1237`,
`crates/lash-sqlite-store/src/graph.rs:62-187`). Its SQL has an upper bound
only (`crates/lash-sqlite-store/src/session_sql.rs:354`). Validation starts
at generation zero (`crates/lash-sqlite-store/src/graph.rs:136`). PostgreSQL
does the same (`crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs:1253`,
`crates/lash-postgres-store/src/postgres/session_sql.rs:433`,
`crates/lash-postgres-store/src/postgres/support.rs:798`). The same read
decodes every usage delta before folding them
(`crates/lash-sqlite-store/src/persistence/session_commit.rs:1297-1300`,
`crates/lash-sqlite-store/src/blobs.rs:415`,
`crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs:1297`).
It also decodes every receipt that carries failure evidence
(`crates/lash-sqlite-store/src/persistence/mod.rs:33`,
`crates/lash-store-sql/src/session/turn_commits.rs:50`). Adoption keeps the
graph, derives every frame record and collects every persisted id
(`crates/lash-core-store/src/session_state.rs:1531-1548`). The read cache
keeps the unscoped projection plus a map of frame-scoped ones
(`crates/lash-core-store/src/session_graph_cache.rs:83-99`).
`message_tree` walks every branch
(`crates/lash-core-store/src/session_graph.rs:1524`). A derived read view
clones the whole graph to rewrite its tail
(`crates/lash-core-store/src/session_read_view.rs:126-141`).

**Some of the pieces already exist.** Every node row stores `frame_node_id`,
and generations are unique per session
(`crates/lash-sqlite-store/src/schema.rs:199-210`,
`crates/lash-postgres-store/schema.sql:124`). An admitted-base read already
resolves the frame nearest the admitted leaf and takes that frame's config
(`crates/lash-sqlite-store/src/persistence/session_commit.rs:1271-1287`). The
append fence already runs inside the commit transaction
(`crates/lash-sqlite-store/src/persistence/session_commit.rs:590`,
`crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs:599`).
The only resident ancestor check left is on the storeless path
(`crates/lash-core/src/runtime/session_ops.rs:139-149`). Three paths switch
frames today: explicit compaction
(`crates/lash-core/src/runtime/session_api.rs:827`), context-pressure
compaction and overflow recovery (a context-pressure hook's decision, which
core opens in `LashRuntime::apply_context_pressure`,
`crates/lash-core/src/runtime/turn_loop/prepare.rs`) and
`continue_as` (`crates/lash-core/src/runtime/turn_boundary.rs:494-505`).

## Decision

### 1. One multi-session store

The store is one object per catalog, keyed by session. `RuntimeStore`
replaces `RuntimePersistence`. It is a blanket alias over ten segments in
`lash-core-store`. `DeploymentStore` replaces `SessionStoreFactory`. It lives
in `lash-core-execution` because it names execution types (§2). Backends
implement segments, never the aliases.

```rust
// crates/lash-core-store/src/store/mod.rs
pub trait RuntimeStore:
    FleetFormatStore
    + AttachmentManifest
    + SessionCatalogStore
    + SessionCommitStore
    + SessionHistoryStore
    + TurnInputStore
    + QueuedWorkStore
    + DriveEpochStore
    + RootStore
    + StoreMaintenance
{
}

impl<T> RuntimeStore for T where
    T: FleetFormatStore
        + AttachmentManifest
        + SessionCatalogStore
        + SessionCommitStore
        + SessionHistoryStore
        + TurnInputStore
        + QueuedWorkStore
        + DriveEpochStore
        + RootStore
        + StoreMaintenance
        + ?Sized
{
}
```

**Operations are required.** No production segment method has a default
that answers for the backend. That rules out `UnsupportedStoreOperation`,
`Ok(())`, `Ok(None)`, and an oldest-version answer. A provided method may
remain only when it composes the segment's own required primitives, like
today's `enqueue_pending_turn_input`
(`crates/lash-core-store/src/store/mod.rs:1520`). Every such method stays on
the decorator's self-routed list
(`crates/lash-core-store/src/store/runtime_persistence_decorator.rs:10`).
`StoreError::UnsupportedStoreOperation` stays in the enum for the gated test
hooks and for non-store surfaces. No store or deployment trait default
returns it.

**Every session-scoped operation names its session.** It takes
`session_id: &SessionId` as its first parameter after `&self`, or it takes a
request whose type already carries the id. `RuntimeCommit`, `ClaimAuthority`,
`WorkClaim<_>`, `TurnAddress`, `TurnCancelRequest`,
`TurnCancelClosureAuthorization`, `PendingTurnInputBatch`,
`QueuedWorkBatchDraft`, `AdmitRootRequest`, `TurnParkWrite`, `SessionMeta`
and `AttachmentIntent` all do. An operation that spans the catalog says so
in its doc and takes no session.

**Unchanged segments.** `FleetFormatStore`
(`crates/lash-core-store/src/store/mod.rs:2012`), `AttachmentManifest`
(`crates/lash-core-store/src/store/attachment_manifest.rs:429`),
`DriveEpochStore` (`crates/lash-core-store/src/store/drive_fence.rs:249`) and
`QueuedWorkStore` (`crates/lash-core-store/src/store/mod.rs:1781`) keep their
signatures verbatim, with one exception: `QueuedWorkStore` gains
`has_claimable_queued_work` (below). `AttachmentManifest`'s four
session-free reads (`list_uncommitted`, `forget_aged_uncommitted_intents`,
`has_live_ref_for_id`, `list_all_refs`) are catalog-wide by their existing
contract. `RootStore` (`crates/lash-core-store/src/store/root.rs:328`) keeps
its signatures, and `admit_root` loses its unsupported default.

#### 1.1 `SessionCatalogStore` (new)

```rust
// crates/lash-core-store/src/store/catalog.rs
#[async_trait::async_trait]
pub trait SessionCatalogStore: Send + Sync {
    async fn admit_session(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<SessionAdmission, StoreError>;

    async fn lookup_session(&self, session_id: &SessionId) -> Result<SessionLookup, StoreError>;

    async fn list_sessions(
        &self,
        filter: &SessionListFilter,
    ) -> Result<Vec<SessionSummary>, StoreError>;

    async fn fork_session(
        &self,
        request: &ForkSessionRequest,
    ) -> Result<ForkSessionReceipt, StoreError>;

    async fn pin(&self, node_id: &NodeId) -> Result<ForkPoint, StoreError>;
    async fn unpin(&self, node_id: &NodeId) -> Result<(), StoreError>;
    async fn fork_points(&self) -> Result<Vec<ForkPoint>, StoreError>;

    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> MaintenanceResult<SessionBlobReclaimReport>;
}

// crates/lash-core-store/src/session_store_factory_types.rs
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionLookup {
    /// Durable metadata exists and no deletion tombstone does.
    Live(SessionMeta),
    /// The id carries a permanent deletion tombstone.
    Deleted,
    /// The catalog has never held this id.
    Absent,
}
```

- `admit_session` is the one admission seam. It replaces `create_store`,
  `admit_and_bind_session`, and the open-then-admit pair callers use. It
  makes one transaction, in this order:
  1. Refuse an invalid id with `InvalidSessionId`.
  2. Refuse a deletion tombstone with `SessionDeleted`. This keeps the
     FIG-1282 ordering and holds on every backend.
  3. Insert metadata exactly from the request when none exists: `relation`,
     `pending_observer_intents`, `owning_process_id`. Answer `Created`.
  4. Otherwise check the recorded lineage with `guard_rebind_lineage`. A
     conflict is `SessionRelationMismatch`. Otherwise answer `Rebound` and
     leave the row untouched. `Root` declares no lineage, so it always
     rebinds.

  Nothing is bound: the store has no handle state to bind.
- `lookup_session` makes no writes. It replaces `open_existing_store`,
  `open_existing_store_by_id` and `session_was_deleted`. `Absent` and
  `Deleted` are answers. Failing to answer is `Err`, never `Absent`, which
  keeps the negative-answer rule of
  [ADR 0119](0119-durable-session-and-live-session-are-two-authorities.md).
- `list_sessions`, `pin`, `unpin`, `fork_points` and `delete_session` keep
  today's factory semantics (`crates/lash-core-execution/src/runtime/vocabulary.rs:478`,
  `:691`, `:732-750`). `fork_session` is today's `fork_at` (`:753`). None of
  them has a default.

#### 1.2 `SessionCommitStore` (reshaped)

```rust
// crates/lash-core-store/src/store/mod.rs
#[async_trait::async_trait]
pub trait SessionCommitStore: Send + Sync {
    async fn read_session_state_version(&self, session_id: &SessionId) -> Result<u32, StoreError>;

    /// Provided: `read_session_state_version(&lease.session_id)` classified
    /// under `lease`, exactly as today (`store/mod.rs:1040`).
    async fn admit_session_state(
        &self,
        lease: &ClaimAuthority,
    ) -> Result<SessionStateAdmission, StoreError>;

    async fn load_session_head_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionHeadMeta>, StoreError>;

    async fn retain_admission_base(
        &self,
        lease: &ClaimAuthority,
        base: &SessionHeadRef,
    ) -> Result<(), StoreError>;

    async fn committed_turn_exists(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<bool, StoreError>;

    async fn drain_end_exists(&self, session_id: &SessionId, drain_id: &str)
        -> Result<bool, StoreError>;

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError>;

    /// Provided: the head meta's `pending_follow_on`.
    async fn load_pending_follow_on(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<PendingFollowOn>, StoreError>;

    async fn raise_pending_follow_on_attempts(
        &self,
        lease: &ClaimAuthority,
        follow_on_turn_id: &TurnId,
    ) -> Result<PendingFollowOn, StoreError>;

    async fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError>;
    async fn load_session_meta(&self, session_id: &SessionId)
        -> Result<Option<SessionMeta>, StoreError>;
    async fn load_session_meta_for_commit(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionMeta>, StoreError>;

    async fn record_turn_park(&self, park: &TurnParkWrite) -> Result<TurnPark, StoreError>;
    async fn load_turn_park(&self, session_id: &SessionId) -> Result<Option<TurnPark>, StoreError>;
}
```

Every method keeps today's documented semantics
(`crates/lash-core-store/src/store/mod.rs:1034-1325`), with its session now
explicit. `load_session`, `load_session_at`, `load_node` and
`admit_and_bind_session` are deleted. So is the `AttachmentManifest`
supertrait, because `RuntimeStore` composes both. `read_session_state_version`,
`retain_admission_base`, `committed_turn_exists`, `drain_end_exists`,
`record_turn_park` and `load_turn_park` lose their defaults.

#### 1.3 `SessionHistoryStore` (new)

These are the only store reads that decode graph bodies, usage rows or
receipts. Sections 5 to 8 pin their semantics.

```rust
// crates/lash-core-store/src/store/history.rs
#[async_trait::async_trait]
pub trait SessionHistoryStore: Send + Sync {
    async fn load_session_window(
        &self,
        session_id: &SessionId,
        selector: WindowSelector,
    ) -> Result<Option<SessionWindowRead>, StoreError>;

    async fn load_ancestors(
        &self,
        session_id: &SessionId,
        anchor: HistoryAnchor,
        budget: HistoryBudget,
    ) -> Result<HistoryPage, StoreError>;

    async fn contains_active_ancestor(
        &self,
        session_id: &SessionId,
        node_id: &NodeId,
    ) -> Result<bool, StoreError>;

    async fn load_usage_totals(&self, session_id: &SessionId)
        -> Result<SessionUsageTotals, StoreError>;

    async fn load_usage_ledger_page(
        &self,
        session_id: &SessionId,
        after: Option<&UsageLedgerCursor>,
        limit: NonZeroU32,
    ) -> Result<UsageLedgerPage, StoreError>;

    async fn load_failure_evidence_page(
        &self,
        session_id: &SessionId,
        after: Option<&FailureEvidenceCursor>,
        limit: NonZeroU32,
    ) -> Result<FailureEvidencePage, StoreError>;
}
```

#### 1.4 `TurnInputStore`, `QueuedWorkStore`, `StoreMaintenance` (changed methods)

```rust
// TurnInputStore: the only signature change. The rest stay verbatim
// (store/mod.rs:1367-1715), minus the defaults listed below.
async fn pending_turn_cancel_closure_pins(
    &self,
    session_id: &SessionId,
) -> Result<Vec<TurnCancelClosureAuthorization>, StoreError>;

// QueuedWorkStore: added. Replaces the factory's `Option<bool>` probe.
async fn has_claimable_queued_work(&self, session_id: &SessionId) -> Result<bool, StoreError>;

// StoreMaintenance: replaces `vacuum(&self)`.
#[async_trait::async_trait]
pub trait StoreMaintenance: Send + Sync {
    /// Scoped to `session_id`, including tombstoned rows of already-deleted
    /// sessions that session's deletes retired. Never catalog-wide.
    async fn vacuum(&self, session_id: &SessionId) -> MaintenanceResult<VacuumReport>;
    /// Catalog-wide by definition: blobs no retained root reaches.
    async fn gc_unreachable(&self) -> MaintenanceResult<GcReport>;
}
```

`TurnInputStore` loses the defaults of `pending_turn_cancel_closure_pins`,
`turn_is_committed`, `record_turn_cancel_request`, `turn_cancel_request`,
`turn_cancel_request_intent`, `reconcile_turn_cancel_winner`, `load_run_spec`,
`list_turn_input_applications`, `orphaned_active_turn_ids` and
`repair_orphaned_active_turn_inputs`. `has_claimable_queued_work` answers
`true` when the session has a pending queued batch or a deferred next-turn
input. A store can always answer that, so today's "unknown" arm goes away
(`crates/lash-core-execution/src/runtime/vocabulary.rs:651-676`).

#### 1.5 Typed errors

`StoreError` (`crates/lash-core-store/src/store/error.rs:6`) changes in place.

```rust
// Added
SessionNotFound { session_id: SessionId },
ForeignSessionRequest { view_session_id: SessionId, request_session_id: SessionId },
InvalidWindowAnchor { frame_node_id: NodeId, violation: WindowAnchorViolation },
HistoryAnchorUnavailable { session_id: SessionId, node_id: NodeId, reason: AnchorUnavailable },
HistoryNodeTooLarge { node_id: NodeId, required_bytes: u64, max_bytes: u64 },
CursorForeignSession { cursor_session_id: SessionId, session_id: SessionId },
HistoryCursorLineageChanged { session_id: SessionId },

// Removed, with the binding they described
SessionBindingMismatch { .. },
SessionNotBound,
SessionResolutionAmbiguous { .. },
```

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowAnchorViolation {
    /// The lowest window node is not a `FrameOpen`.
    BaseNotFrameOpen,
    /// The lowest node's id differs from the frame the leaf points at.
    BaseIsNotLeafFrame,
    /// A window row's `frame_node_id` differs from the base.
    ForeignFramePointer,
    /// The base is at generation 0 but has a parent, or above 0 without one.
    ExternalParentShape,
    /// A node other than the base names a parent outside the window.
    InnerParentOutsideWindow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnchorUnavailable {
    /// No live row with this id is readable by the session: absent, owned by
    /// an unrelated session, above a fork ceiling, or physically vacuumed.
    NotReadable,
    /// The row exists and is tombstoned: pruned, unpinned or retired.
    Tombstoned,
}
```

A window read that fails validation returns `InvalidWindowAnchor` or
`StoredDataCorrupt`, never a smaller window. `CurrentFrameNodeMismatch`
(`crates/lash-core-store/src/store/error.rs:381`) also covers a head pointer
that disagrees with its leaf's frame (§5). `SessionDeleted` answers every
history read on a deleted session. `SessionBindingNotMaterialized` stays: it
means corrupt admission state, not a binding.

### 2. Catalog operations replace the factory

`SessionStoreFactory` is deleted. Its operations go where their scope says.

| Factory operation (`vocabulary.rs`) | Replacement |
|---|---|
| `create_store` `:424`, `admit_and_bind_session` (`store/mod.rs:1264`) | `SessionCatalogStore::admit_session` + `SessionStore::new` |
| `open_existing_store` `:429`, `open_existing_store_by_id` `:612`, `session_was_deleted` `:683` | `SessionCatalogStore::lookup_session` |
| `open_unbound_store` `:443` | deleted, with nothing in its place |
| `read_session` `:464` | `store::load_session_read_view(&SessionStore)`, window-bound (§13) |
| `list_sessions` `:478` | `SessionCatalogStore::list_sessions` |
| `fork_at` `:753`, `pin` `:732`, `unpin` `:739`, `fork_points` `:746` | `SessionCatalogStore::{fork_session, pin, unpin, fork_points}` |
| `delete_session` `:691` | `SessionCatalogStore::delete_session` |
| `root_terminal` `:549` | `RootStore::root_terminal` (it already exists there) |
| `pending_turn_cancel_closure_pins` `:621` | `TurnInputStore::pending_turn_cancel_closure_pins(session_id)` |
| `has_claimable_queued_work` `:651` | `QueuedWorkStore::has_claimable_queued_work` |
| the deployment operations below | `DeploymentStore` |

```rust
// crates/lash-core-execution/src/runtime/vocabulary.rs
#[async_trait::async_trait]
pub trait DeploymentStore:
    crate::store::RuntimeStore + crate::AttachmentRootSet + crate::store::ControlIntentStore
{
    fn bind_effect_host(&self, effect_host: &Arc<dyn crate::EffectHost>);
    fn bind_artifact_stores(
        &self,
        process_env_store: Arc<dyn ProcessExecutionEnvStore>,
        process_engines: ProcessEngineRegistry,
    );
    async fn count_unsettled_turns(&self) -> Result<UnsettledTurnCounts, StoreError>;
    async fn list_turn_parks(&self, query: &TurnParkQuery) -> Result<Vec<TurnPark>, StoreError>;
    async fn turn_park_feed(
        &self,
        after: ParkFeedCursor,
        limit: NonZeroUsize,
    ) -> Result<ParkFeedPage<TurnParkTarget>, StoreError>;
    async fn compact_turn_park_feed(&self, through: ParkFeedCursor) -> Result<(), StoreError>;
    async fn non_terminal_roots_page(
        &self,
        after: Option<&crate::engine::RootRef>,
        limit: NonZeroUsize,
    ) -> Result<Vec<crate::engine::RootRef>, StoreError>;
    async fn end_lost_root(
        &self,
        target: &crate::engine::RootRef,
        at_ms: u64,
    ) -> Result<Option<RootTerminal>, StoreError>;
    async fn list_control_intents(
        &self,
        after: Option<ControlIntentId>,
        limit: NonZeroUsize,
    ) -> Result<Vec<ControlIntent>, StoreError>;
    async fn retire_turn_cancel_closure_scope(&self, scope: &ExecutionScope)
        -> Result<(), StoreError>;
    async fn reclaim_retained_evidence(
        &self,
        bound: RetentionBound,
    ) -> MaintenanceResult<RetentionReport>;
}
```

Each operation keeps today's documented semantics (`vocabulary.rs:413-725`).
None has a default, the two `bind_*` methods included. A host, the facade
and the engine hold `Arc<dyn DeploymentStore>`. Runtime code holds a
`SessionStore` (§3), which reaches it as an `Arc<dyn RuntimeStore>` by trait
upcasting. The free helpers `admit_session_state_generation` and
`park_turn_of_refused_group_child`
(`crates/lash-core-execution/src/runtime/vocabulary.rs:783`, `:805`) take
`&dyn RuntimeStore` and call `lookup_session`.

**Decorators.** The operation list in
`crates/lash-core-store/src/store/runtime_persistence_decorator.rs:30-148`
stays the single place signatures are written. It grows to cover every
`RuntimeStore` and `DeploymentStore` operation, and it generates three
things:

- `RuntimeStoreDecorator`, in `lash-core-store`;
- `DeploymentStoreDecorator: RuntimeStoreDecorator`, in `lash-core-execution`;
- the `SessionStore` forwarders (§3).

The `decorator_surface_covers_every_component_trait_method` lint
(`crates/lash-core-store/src/store/tests.rs:895`) extends to all three.
`ConformancePersistence` becomes `ConformanceStore: RuntimeStore +
StoreTestSupport`. `ConformanceSessionStoreFactory`
(`crates/lash-core-execution/src/store/conformance_factory.rs:12`) becomes
`ConformanceDeployment: DeploymentStore + StoreTestSupport`.

### 3. `SessionStore` is the permanent thin view

```rust
// crates/lash-core-store/src/store/session_view.rs
#[derive(Clone)]
pub struct SessionStore {
    store: Arc<dyn RuntimeStore>,
    session_id: SessionId,
}

impl SessionStore {
    /// Validates the id (`validate_session_id`). It does not admit, read or
    /// bind: a view is a value.
    pub fn new(store: Arc<dyn RuntimeStore>, session_id: SessionId) -> Result<Self, StoreError>;
    pub fn session_id(&self) -> &SessionId;
    pub fn store(&self) -> &Arc<dyn RuntimeStore>;
}
```

- The view holds exactly two fields. It has no backend state, no latch, no
  binding mode and no discovery fallback.
- It implements no store trait. Its session-scoped operations are inherent
  methods generated from the decorator operation list, with the same names,
  minus the `session_id` parameter. For example:
  `view.load_session_window(selector)` calls
  `store.load_session_window(&self.session_id, selector)`.
- **A session-local request cannot override the view's identity.** For an
  operation whose request carries a session id (the types listed in §1),
  the generated forwarder compares that id with the view's id before it
  forwards. A mismatch returns `ForeignSessionRequest { view_session_id,
  request_session_id }` and nothing reaches the store.
- Catalog operations (§1.1) and deployment operations (§2) are not on the
  view. A caller that needs them uses `view.store()` or holds the deployment
  store.
- `Session::history_store()`
  (`crates/lash-core-execution/src/session.rs:481`) returns
  `Option<SessionStore>`.

Shape B alone would save the generated forwarders. It would also lose the
session-local call sites, and nothing found in the code overturns the ruling
for C.

### 4. Per-handle resources move to the catalog

- **SQLite.** `SqliteSessionStoreFactory` and `Store` merge into one
  `SqliteStore`, one per durable-core catalog. It keeps the factory's fields
  (`crates/lash-sqlite-store/src/session_store_factory.rs:18-31`) and takes
  the handle's (`crates/lash-sqlite-store/src/lib.rs:195-217`):
  - One writer connection thread for the catalog. WAL allows one writer, so
    per-handle writer threads bought nothing.
  - A fixed pool of read-only connections that serves history reads,
    `list_sessions` and read views. Its size is the new
    `SqliteConnectionPolicy::read_connections: NonZeroUsize`, default 4.
  - The database keepalive (`location`), fleet format, clock and options.
  - The cancellation binding. It is catalog-scoped already
    (`crates/lash-sqlite-store/src/session_store_factory.rs:291-305`), so the
    per-handle copy goes.
  - The publication pause, the commit counter (the enqueue-nonce seed) and
    the registry-attachment flag.
  - The `cfg(test)` checkpoint probes, now counted per catalog.

  `session_id: Arc<OnceLock<SessionId>>` is deleted, along with
  `bind_session`, `selected_session_id`, `resolve_session_id_for_read` and
  `bind_session_lock` (`:451-521`). The handle constructors `open_bound_at`,
  `open_at` and `open_bound_readonly`
  (`crates/lash-sqlite-store/src/lifecycle.rs:30`, `:163`, `:234`) become one
  catalog open. `SqliteStoreSet::open_store`
  (`crates/lash-sqlite-store/src/backend.rs:322`) returns the `SqliteStore`.
- **PostgreSQL.** `PostgresSessionStore`
  (`crates/lash-postgres-store/src/lib.rs:676`) and `store_for`
  (`crates/lash-postgres-store/src/postgres/session_factory/store.rs:18`) are
  deleted. `PostgresSessionStoreFactory` (`crates/lash-postgres-store/src/lib.rs:659`)
  becomes `PostgresStore` and holds everything. The per-handle test counters
  (`:688-691`) become catalog `Arc<AtomicUsize>` fields under `cfg(test)`.
- **Perf.** `RuntimePerfStore` decorates `Arc<dyn DeploymentStore>`. It
  replaces `committed_node_ids: HashSet<NodeId>`
  (`crates/lash-perf/src/runtime_perf/store.rs:27`) with a
  `committed_nodes: AtomicU64`. Each fresh receipt adds its node count, and a
  replayed receipt adds nothing. Measurement bookkeeping never holds ids, and
  it is excluded from residency measurements.
- **S3 and Restate** do not change. S3 is blob storage
  (`crates/lash-s3-store/src/lib.rs:93`). Restate runs roots and holds no
  store (`crates/lash-restate/src/session_driver.rs:1140`).

### 5. `load_session_window`

```rust
// crates/lash-core-store/src/store/history.rs
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowSelector {
    /// The live head.
    Current,
    /// The head a turn was admitted on (FIG-3682).
    Admitted(SessionHeadRef),
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SessionWindowRead {
    pub session_id: SessionId,
    pub head_revision: u64,
    pub config: PersistedSessionConfig,
    /// Equals `window.anchor().map(|a| &a.frame_node_id)`. `None` iff the
    /// window is empty.
    pub current_frame_node_id: Option<FrameNodeId>,
    pub pending_follow_on: Option<PendingFollowOn>,
    pub window: SessionGraph,
    pub checkpoint_ref: Option<BlobRef>,
    pub checkpoint: Option<HydratedSessionCheckpoint>,
    pub usage: SessionUsageTotals,
}

// crates/lash-core-store/src/session_graph.rs
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WindowAnchor {
    /// The window base: the current frame's `FrameOpen`.
    pub frame_node_id: FrameNodeId,
    /// The base's stored generation.
    pub generation: u64,
    /// The base's parent, outside the window. `Some` iff `generation > 0`.
    pub external_parent: Option<NodeId>,
    /// `frame_node_id` of `external_parent`. `Some` iff `generation > 0`.
    pub previous_frame_node_id: Option<FrameNodeId>,
}

impl SessionGraph {
    pub fn from_window(
        nodes: Vec<SessionNodeRecord>,
        leaf_node_id: NodeId,
        anchor: WindowAnchor,
    ) -> Result<Self, StoreError>;
    pub fn anchor(&self) -> Option<&WindowAnchor>;
}
```

`SessionGraphData` (`crates/lash-core-store/src/session_graph.rs:132`) gains
`anchor: Option<WindowAnchor>`. A graph built in memory with no store has
`anchor: None`, and its root has no parent.

**Leaf and frame resolution.**

- `Current` takes the head's `leaf_node_id` and `checkpoint_ref`.
  `Admitted(base)` takes `base.leaf` and `base.checkpoint`. It reports
  `head_revision = base.revision` and `pending_follow_on = None`, because an
  admitted turn never owes one (ADR 0101 §3).
- The window's frame is the `frame_node_id` column of the selected leaf row.
  So admitted replay resolves the frame at the admitted leaf, never at
  today's head.
- For `Current`, the head's `current_frame_node_id` must equal that frame.
  If it does not, the read fails with `CurrentFrameNodeMismatch`. The head
  pointer stays an accelerator that is always checked, per ADR 0057.
- `config` is the head's config when the frame is the head's current frame.
  Otherwise it is the frame's `FrameOpen` config, as today
  (`crates/lash-sqlite-store/src/persistence/session_commit.rs:1271-1287`).
- `usage` is the live session's totals under both selectors, as today's
  admitted read carries the live ledger.

**The range predicate.** Let `f` be the frame row's generation and `l` the
leaf's. The window reads rows with `generation BETWEEN f AND l`. That bound
is conjoined with the unchanged ownership-or-ceiling disjunction: the
session's own rows, plus each ancestor's rows at or below that ancestor's own
`fork_generation`. The per-ancestor ceilings are never collapsed into one
bound. The frame row itself must satisfy the same predicate. A fresh fork
whose current frame is owned by an ancestor therefore gets a window that
spans sessions. The read uses the `readable_sessions` CTE form of
`select_readable_range` (`crates/lash-sqlite-store/src/session_sql.rs:269`,
`crates/lash-postgres-store/src/postgres/session_sql.rs:308`), so each
readable session hits the `UNIQUE (session_id, generation)` index. It runs
in one read transaction: deferred on SQLite, `REPEATABLE READ READ ONLY` on
PostgreSQL. The head, window, checkpoint and usage totals are one snapshot.

**Validation.** Rows stream in ascending generation order, and each row's
body size is checked before its body is decoded (§6).
- The first row must be the frame row: generation `f`, a `FrameOpen`
  payload, and a `frame_node_id` equal to its own id.
- Every later row's parent must be the previous row, its generation must be
  exactly one higher, and its `frame_node_id` must equal the base.
- The last row must be the leaf.
- **Exactly one external parent is allowed, at the `FrameOpen` base.** When
  `f > 0`, the base's parent is `external_parent`, and
  `previous_frame_node_id` is read from that parent's `frame_node_id` column
  in the same transaction. When `f == 0`, the base has no parent.

Any other dangling parent is corrupt, as today
(`crates/lash-core-store/src/session_graph_integrity.rs:68-82`), and fails
with `InvalidWindowAnchor`. `SessionGraph::from_window` enforces the same
rules on the decoded nodes, so a third-party store cannot hand the runtime an
unanchored suffix. A graph with no anchor keeps today's rules.

**Answers.**
- `Ok(None)`: the session has no head row, under `Current` only.
- A head with no leaf returns an empty window with `anchor: None`.
- `Admitted` never returns `None`. A base the store no longer holds is
  `TurnBaseNotRetained`, never another head.
- A deleted session is `SessionDeleted`.

**Decoded rows.** A window read decodes exactly `l - f + 1` node bodies, the
checkpoint components, one aggregate row per `(source, model)`, and one row
per outstanding usage hole (§8). It decodes no receipt and no reported usage
row.

### 6. `load_ancestors`

```rust
// crates/lash-core-store/src/store/history.rs
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryAnchor {
    /// The live head's leaf, inclusive.
    Head,
    /// The named node, inclusive.
    Node(NodeId),
    /// Continue a previous page.
    Cursor(HistoryCursor),
}

/// Both limits are required and non-zero. There is no default and no
/// unbounded value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryBudget {
    pub max_nodes: NonZeroU32,
    pub max_bytes: NonZeroU64,
}

#[derive(Clone, Debug)]
pub struct HistoryPage {
    /// The leaf this paging run is pinned to. `None` only for `Head` on a
    /// session whose head has no leaf.
    pub pinned_leaf: Option<NodeId>,
    /// Descending generation. The first entry is the anchor node.
    pub nodes: Vec<HistoryNode>,
    pub stop: HistoryStop,
    /// `Some` iff `stop` is `NodeBudget` or `ByteBudget`.
    pub next: Option<HistoryCursor>,
}

#[derive(Clone, Debug)]
pub struct HistoryNode {
    pub generation: u64,
    pub owner_session_id: SessionId,
    pub frame_node_id: FrameNodeId,
    pub body_bytes: u64,
    pub record: SessionNodeRecord,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryStop {
    NodeBudget,
    ByteBudget,
    /// The page ends at generation 0. Nothing remains.
    Root,
}

/// Opaque and session-bound. Serializable so a host can hand it through
/// its UI. The store revalidates it on every use, so a forged cursor reads
/// nothing the session could not already read.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HistoryCursor {
    session_id: SessionId,
    pinned_leaf: NodeId,
    lineage: LineageStamp,
    next_node_id: NodeId,
    next_generation: u64,
}

impl HistoryCursor {
    pub fn session_id(&self) -> &SessionId;
    pub fn pinned_leaf(&self) -> &NodeId;
}

/// BLAKE3 (domain `lash-history-lineage/v1`) over the session's
/// `fork_lineage` rows, ordered by `ancestor_session_id`, each encoded as
/// `(ancestor_session_id, fork_generation)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LineageStamp([u8; 32]);
```

**Order and membership.** A page walks one ancestry, the pinned leaf's, in
descending generation order. Pages cross frame boundaries and fork points
with no special mode. Each row carries the ownership-or-ceiling predicate of
§5 under the page's generation range. A session's readable rows form a
single chain: generations are unique per session, and `fork_lineage` holds
one row for every ancestor. So a page is "readable rows at or below the
start generation, descending". Every row is still checked against the
previous row's parent edge and generation, and a gap is `StoredDataCorrupt`.
Timestamps never order anything, which keeps the FIG-1641 witness.

**Budgets.** Sizes come from a new stored column, `graph_nodes.body_bytes`:
the byte length of `node_json`, written at insert (§11).
- The page takes rows in order while the row count is at most `max_nodes`
  and the sum of `body_bytes` is at most `max_bytes`.
- `stop` is `Root` when the last row taken is at generation 0. Otherwise it
  is `NodeBudget` when the count reached `max_nodes`, and `ByteBudget`
  when the next row would overflow `max_bytes`.
- The size is checked before the body is fetched or decoded. The backend
  query bounds the bodies it fetches: a running byte sum in SQL, or a header
  query followed by a body query for exactly the rows taken. It never
  truncates a collection it already holds.
- **`HistoryNodeTooLarge { node_id, required_bytes, max_bytes }`** is the
  answer when the page's first row alone exceeds `max_bytes`. Here
  `required_bytes` is that row's `body_bytes`. It is an error, not an empty
  page that can be resumed. So every page that returns a `next` holds at
  least one node, and paging always makes progress.

**Cursors.**
- A cursor is bound to its session. Using it with another session is
  `CursorForeignSession`.
- It is pinned to its leaf. `Head` pins the head's leaf when paging starts,
  and `Node(id)` pins `id`. Later pages follow that leaf's ancestry even if
  the head has moved.
- It is pinned to its lineage. Each use recomputes `LineageStamp`, and a
  difference is `HistoryCursorLineageChanged`. The lineage rows are written
  once at fork, so this is a guard against state that should be
  unreachable.
- A cursor is stable across appends. Appends only add generations above
  existing rows and never rewrite a row below the pinned leaf, so the pages
  after an append are the pages that would have come without it.
- If the next node is gone, the read fails with `HistoryAnchorUnavailable`,
  whether the anchor was a cursor or `Node(id)`. The reason is `Tombstoned`
  when the row is still present but retired (pruning, unpinning, fork
  retirement), and `NotReadable` otherwise. A deleted session is
  `SessionDeleted`.
- `Head` on a session with no head row is `SessionNotFound`. `Head` on a
  head with no leaf is an empty page with `stop: Root` and no `next`.

**One node replaces `load_node`.** `load_ancestors(s, Node(id),
HistoryBudget { max_nodes: 1, .. })` is today's `load_node`
(`crates/lash-core-store/src/store/mod.rs:1118`). Absence becomes the typed
`HistoryAnchorUnavailable` instead of `Ok(None)`. Its conformance cases port
to this form.

### 7. `contains_active_ancestor`

`contains_active_ancestor(session_id, node_id)` answers `true` iff `node_id`
is a live row readable by the session, with a generation at or below the
generation of the session's current head leaf. Because a session's readable
rows form one chain, that is exactly "on the active path". It uses one
statement that reads the head leaf generation and the candidate row
together. That is `exists_readable_ancestor`
(`crates/lash-sqlite-store/src/session_sql.rs:412`) with `?3` taken from the
head. It decodes no body and returns no content. A session whose head has no
leaf answers `false`. A deleted session is `SessionDeleted`.

It is a predicate, not a history view, and the gate of §13 registers it as
non-materializing. If it ever returns content, it must take a budget.

**It never replaces the commit fence.** It answers at a read snapshot. The
fence in `commit_runtime_state` answers inside the write transaction,
against the append's own parent generation, under the head
compare-and-swap. Between a predicate and a commit, the head can move, and
another writer can branch or append. Only the fence decides whether an
append is stale. A runtime pre-check may use the predicate to answer early,
and a `true` must still survive the fence.

**Sites that must stop asking residency.**
- The append pre-check
  (`crates/lash-core/src/runtime/session_ops.rs:139-149`) already runs only
  without a store. It stays storeless-only.
- The historical-frame refusal is a site the plan missed.
  `append_frame_open_with_id_at` refuses to reopen a frame it finds resident
  (`crates/lash-core-store/src/session_graph.rs:1247`). Under a window, a
  same-session frame below the base is not resident. The async callers
  (`crates/lash-core/src/runtime/session_api.rs:746`,
  `crates/lash-core/src/runtime/turn_commit_draft.rs:286`, `:750`) must ask
  `contains_active_ancestor(session, frame_node_id)` first and answer
  `HistoricalAgentFrameSwitchUnsupported` on `true`. The commit's
  `NodeIdCollision` stays the authority.
- The frame-root lookup (`crates/lash-core/src/runtime/session_manager/mod.rs:195-205`)
  and the turn editor's ancestry walk
  (`crates/lash-core/src/runtime/turn_graph_editor.rs:316-345`) stop at the
  window base, which is the frame node. They are correct by construction.

### 8. Bounded usage and failure-evidence reads

**Usage.** The resident ledger `token_ledger: Vec<TokenLedgerEntry>` becomes
`usage: SessionUsageTotals` on `RuntimeSessionState`, `SessionSnapshot` and
`SessionWindowRead`.

```rust
// crates/lash-core-store/src/usage.rs
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionUsageTotals {
    /// One row per `(source, model)`, sorted and unique.
    pub rows: Vec<UsageTotalRow>,
    /// Holes no `Reconciled` row has filled, sorted by
    /// `(call_id, attempt_ordinal)`.
    pub outstanding: Vec<UnreportedUsageAttempt>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UsageTotalRow {
    pub source: String,
    pub model: String,
    /// Reported plus reconciled counters.
    pub usage: TokenUsage,
    /// Distinct holes ever recorded.
    pub unreported_attempts: u64,
    /// `Reconciled` corrections recorded.
    pub reconciled_attempts: u64,
}

impl SessionUsageTotals {
    /// Folds one staged ledger entry with today's checks
    /// (`store/usage.rs:5-63`). It validates the disposition, adds counters
    /// with `TokenUsageAccountingOverflow` on overflow, and merges holes by
    /// identity, refusing conflicting attribution. A `Reconciled` entry
    /// removes its hole from `outstanding`.
    pub fn fold_checked(&mut self, entry: &TokenLedgerEntry) -> Result<(), StoreError>;
    pub fn report(&self) -> SessionUsageReport;
}

// crates/lash-core-store/src/store/history.rs
pub struct UsageLedgerPage {
    pub rows: Vec<UsageLedgerRow>,
    pub next: Option<UsageLedgerCursor>,
}
pub struct UsageLedgerRow {
    pub seq: u64,
    pub operation_storage_key: String,
    pub entry: TokenLedgerEntry,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UsageLedgerCursor {
    session_id: SessionId,
    after_seq: u64,
}
```

The totals keep what today's fold keeps. The resident folded ledger already
merges reported rows per key. It still grows with `Reconciled` rows, one per
attempt, and with hole lists (`crates/lash-core/src/runtime/session_manager/usage.rs:339`,
`:405`). Reopen derives outstanding holes from it
(`crates/lash-core-store/src/usage.rs:326`,
`crates/lash-core/src/runtime/lifecycle.rs:322`). The totals hold only what
reports and reconciliation need.

To compute totals without decoding history, the hole list moves out of
`usage_disposition_json` and into relational columns, in place (§11):
- `usage_deltas` gains nullable `reconciled_call_id` and
  `reconciled_attempt_ordinal`.
- A new table, `usage_delta_holes (session_id, seq, call_id,
  attempt_ordinal, generation_id)`, keyed on
  `(session_id, seq, call_id, attempt_ordinal)`, holds one row per hole.
- `usage_disposition_json` is dropped. A row's disposition is `Reconciled`
  when `reconciled_call_id` is set, `Unreported` when it has holes, and
  `Reported` otherwise.

`load_usage_totals` and the window's `usage` then take three queries:
- one `SUM`/`COUNT` aggregate grouped by `(source, model)`;
- a distinct-hole count per key;
- the outstanding holes: holes with no matching reconciled row. A hole
  with two different `generation_id`s is `StoredDataCorrupt`.

Counter overflow in SQL is `TokenUsageAccountingOverflow`. The read decodes
no reported row. `load_usage_ledger_page` pages the full rows, which a host
needs for accounting detail, in `seq` order. It fetches `limit + 1` rows to
decide `next`, so `next` is `Some` only when more rows exist. The usage
payload identity (`RuntimeUsageDeltaIdentity`) is computed from the entry at
write time and does not change.

**Failure evidence.** Failure evidence leaves the window read and the read
view. `SessionReadView::turn_failure_settlements`
(`crates/lash-core-store/src/session_read_view.rs:163`) is deleted.

```rust
pub struct FailureEvidencePage {
    pub settlements: Vec<TurnFailureSettlement>,
    pub next: Option<FailureEvidenceCursor>,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FailureEvidenceCursor {
    session_id: SessionId,
    committed_at_ms: u64,
    turn_id: TurnId,
}
```

`runtime_turn_commits` gains `failure_evidence` (`INTEGER`/`BOOLEAN NOT
NULL`), written at insert from the receipt, with a partial index on
`(session_id, committed_at_ms, turn_id) WHERE failure_evidence`. The receipt
is immutable, so the column cannot drift from it. It replaces the `LIKE`
scan (`crates/lash-store-sql/src/session/turn_commits.rs:50`). A page orders
by `(committed_at_ms, turn_id)`, as today, and decodes exactly the receipts
it returns. Usage and failure cursors refuse another session with
`CursorForeignSession`.

### 9. Resident state after the cutover

For a store-backed session, the resident set is always two things: every
pending node, and every durable node from the current frame's `FrameOpen` to
the leaf. That holds at open, at reopen, at refresh and after every commit.
Resident memory is proportional to the current frame, and nothing more is
bounded.

- **Graph.** `RuntimeSessionState::session_graph` is the anchored window
  plus its pending suffix.
- **Trimming.** `RuntimeSessionState::retire_below_current_frame(&mut self)`
  runs after every accepted commit receipt, fresh or replayed. When the
  current `FrameOpen` is not the window base, it rebuilds the graph from the
  base's `Arc` records at and after that `FrameOpen`. The new `WindowAnchor`
  is derived from the old one: the generation plus the path offset, the
  `FrameOpen`'s parent, and that parent's frame. Only nodes that are both
  durable (in `persisted_node_ids`) and below the current `FrameOpen` are
  dropped. **A pending node stays until it is durable.** When the base is
  already current, trimming does nothing.
- **Frame records.** `agent_frames` holds the records of the resident
  `FrameOpen`s only. The first record's `previous_frame_node_id` comes from
  `WindowAnchor::previous_frame_node_id`. Adoption
  (`crates/lash-core-store/src/session_state.rs:1532`) and every
  re-derivation site read the window.
- **Persisted ids.** `persisted_node_ids` is kept a subset of the resident
  ids. Trimming removes the ids it drops, and `pending_graph_commit`
  (`crates/lash-core-store/src/session_state.rs:953`) is unchanged.
- **One shared frame projection.** `SessionGraph::read_model(&self) ->
  SessionReadModel` loses its frame parameter
  (`crates/lash-core-store/src/session_graph.rs:1191`) and cannot fail. It
  projects the active path from the nearest `FrameOpen` ancestor of the
  leaf, which is the current frame, whether it is the base or a later
  pending `FrameOpen`. `SessionGraphCache` keeps exactly one memoized
  `ActiveReadModel`
  (`crates/lash-core-store/src/session_graph_cache.rs:88`). The frame map
  (`:98`), `scoped_read_model` (`:198`) and `project_scoped_read_model`
  (`:215`) are deleted. So is `SessionGraphScopeError`, and
  `rewrite_active_read_tail` loses its frame parameter. `RuntimeSessionState::read_model`,
  `SessionReadView`, and the turn editor's base read model
  (`crates/lash-core/src/runtime/turn_graph_editor.rs:15`) all hand out the
  same `Arc`s, which keeps the rope fast path (`:119`).
- **Replay identity law.** At an admission boundary, the resident graph
  equals `load_session_window(Admitted(base)).window`, node for node. The
  standard-compaction request identity hashes the snapshot graph and ledger
  (`crates/lash-plugin-standard-compaction/src/lib.rs:346-422`). That
  identity is stable across first execution and replay only if this holds.
- **Storeless runtimes** keep what they hold. They have no durable authority
  to page from, and this guarantee covers store-backed sessions.

The plan was wrong about derived views, and this record corrects it. The
whole-graph clone in `SessionReadGraph::Derived`
(`crates/lash-core-store/src/session_read_view.rs:126-141`) is deleted. The
tail rewrite is not deleted: it runs over the window graph. A derived view's
snapshot feeds two consumers. Standard compaction's request identity hashes
the snapshot graph (`crates/lash-plugin-standard-compaction/src/lib.rs:346-422`),
and its turn lookup reads messages rebuilt from that graph (`:255-268`).
Replacing that channel would change an identity family, which is outside
this cutover. The rewrite now costs O(frame).

### 10. Frames change on compaction and `continue_as`

A frame is the context window. Explicit compaction
(`crates/lash-core/src/runtime/session_api.rs:827`), context-pressure
compaction and overflow recovery (a context-pressure hook's decision, which
core opens in `LashRuntime::apply_context_pressure`,
`crates/lash-core/src/runtime/turn_loop/prepare.rs`) and
`continue_as` (`crates/lash-core/src/runtime/turn_boundary.rs:494-505`) all
append a `FrameOpen` through `open_agent_frame_in_state_with_clock`
(`crates/lash-core-store/src/session_state.rs:1676`) and commit it. After the
receipt, §9 trims. None of them reloads from the store. The three are
equivalent, and §14 pins each one.

**A defect is raised here, as Sam directed.** The automatic pressure path
does not start a frame. When prompt usage crosses the compaction threshold,
`StandardCompactionTurnTransform::transform` emits `CompactionNeeded` and
then only cuts the prompt view to a tail (`prompt_tail_window`,
`PromptViewPruned`,
`crates/lash-plugin-standard-compaction/src/lib.rs:773-859`). The durable
frame keeps growing past the context window. The frame stays the anchor.
The fix belongs to the pressure path: at the threshold, it must compact
into a new frame through the same switch that overflow recovery uses. That
fix is outside the four lanes unless Sam folds it into the runtime lane. It
carries its own acceptance test (§14, test 6d).

### 11. Stored shapes change in place

Under the version freeze (FIG-3846), these shapes change with no version
bump. That follows
[ADR 0108](0108-a-process-lives-until-a-scope-its-start-could-reach.md)'s
precedent:

- `graph_nodes` gains `body_bytes`. On decode, a value that differs from
  the body's byte length is `StoredDataCorrupt`.
- `usage_deltas` gains `reconciled_call_id` and
  `reconciled_attempt_ordinal`, and drops `usage_disposition_json`.
- The `usage_delta_holes` table is new.
- `runtime_turn_commits` gains `failure_evidence` and its partial index.
- `SessionGraphData` gains `anchor`.
- `RuntimeSessionState` and `SessionSnapshot` carry `usage:
  SessionUsageTotals` in place of `token_ledger`.

The table DDL is on both backends
(`crates/lash-sqlite-store/src/schema.rs:199`, `:220`, `:307`;
`crates/lash-postgres-store/schema.sql:124`, `:145`, `:242`). The shared
column lists and inserts are in `crates/lash-store-sql/src/session/`, per
[ADR 0098](0098-one-owner-per-sql-table-across-both-stores.md).

Old state is refused with a type, never reinterpreted. A PostgreSQL catalog
from before the change fails the open-time shape check. A SQLite catalog
fails its first query on the missing columns. Either is recreated. The
standard-compaction request identity changes, because the snapshot graph is
now the window and the snapshot ledger is now totals. A journal in flight at
the cutover deploy must drain on the build that wrote it
([ADR 0106](0106-durable-formats-upgrade-by-migration-or-drain.md)).

### 12. What is deleted

These go in the same cutover, with no shim left behind.

- **Root-to-leaf loads.**
  - Traits: `SessionCommitStore::load_session`, `load_session_at` and
    `PersistedSessionRead`
    (`crates/lash-core-store/src/store/mod.rs:1052`, `:1082`, `:447`).
  - SQLite: `load_session`, `load_session_at`, `load_session_read`
    (`crates/lash-sqlite-store/src/persistence/session_commit.rs:96`, `:100`,
    `:1212`); `load_session_graph_from_conn`,
    `load_active_path_session_graph_from_conn`, `load_readable_graph_from_conn`
    (`crates/lash-sqlite-store/src/graph.rs:53`, `:62`, `:74`); and the
    statements `select_readable`, `select_readable_to_generation`,
    `select_readable_range` (`crates/lash-sqlite-store/src/session_sql.rs:337`,
    `:354`, `:269`).
  - PostgreSQL: `load_session`, `load_session_at`, `load_session_read`
    (`crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs:70`,
    `:74`, `:1223`); `load_graph_tx`, `load_whole_graph_tx`,
    `load_readable_graph_tx` (`crates/lash-postgres-store/src/postgres/support.rs:734`,
    `:764`, `:773`); and `select_readable`, `select_readable_to_generation`,
    `select_readable_range`
    (`crates/lash-postgres-store/src/postgres/session_sql.rs:416`, `:433`,
    `:308`).
  - Load helpers: `load_persisted_session`, `load_persisted_session_state`,
    `load_persisted_session_admitted`, `load_persisted_session_read_view` and
    `refresh_persisted_session_state`
    (`crates/lash-core-store/src/store/load.rs:58`, `:97`, `:86`, `:67`,
    `:105`). Their window replacements are `load_session_window_state`,
    `load_session_read_view` and `refresh_session_window`, which take
    `&SessionStore`.
- **Whole-history folds.** `load_usage_deltas_conn`
  (`crates/lash-sqlite-store/src/blobs.rs:415`), `load_usage_deltas_tx`
  (`crates/lash-postgres-store/src/postgres/support.rs:702`),
  `load_turn_failure_settlements_conn`
  (`crates/lash-sqlite-store/src/persistence/mod.rs:33`), PostgreSQL's
  settlement loop
  (`crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs:1300`),
  and `merge_token_ledger_entries_checked`
  (`crates/lash-core-store/src/store/usage.rs:66`).
- **`load_node`.** The trait method
  (`crates/lash-core-store/src/store/mod.rs:1118`), SQLite
  (`crates/lash-sqlite-store/src/persistence/session_commit.rs:169`, with
  `select_lookup` at `crates/lash-sqlite-store/src/session_sql.rs:371`),
  PostgreSQL
  (`crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs:155`,
  `select_lookup` at `crates/lash-postgres-store/src/postgres/session_sql.rs:450`),
  and the forwarders in the decorator list and perf
  (`crates/lash-core-store/src/store/runtime_persistence_decorator.rs:53`,
  `crates/lash-perf/src/runtime_perf/store.rs:230`).
- **The SQLite identity latch and unbound opens.**
  - The `session_id` field and `bind_session`, `selected_session_id`,
    `resolve_session_id_for_read`, `bind_session_lock`
    (`crates/lash-sqlite-store/src/lib.rs:207`, `:451`, `:455`, `:462`,
    `:499`).
  - `select_sole_bound_session_id`
    (`crates/lash-sqlite-store/src/session_sql.rs:298`), and the binds in
    admission, commit and park
    (`crates/lash-sqlite-store/src/persistence/session_commit.rs:1146`,
    `:342`, `:310`).
  - `open_unbound_store` on the trait and on SQLite
    (`crates/lash-core-execution/src/runtime/vocabulary.rs:443`,
    `crates/lash-sqlite-store/src/session_store_factory.rs:358`), and every
    forwarder of it in `crates/lash-core/src/testing/`, `crates/lash-perf/`
    and `crates/lash-sim/`.
  - `SessionCommitStore::admit_and_bind_session`
    (`crates/lash-core-store/src/store/mod.rs:1264`) and its backend impls
    (`crates/lash-sqlite-store/src/persistence/session_commit.rs:1117`,
    `crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs:1025`).
- **The factory.** `SessionStoreFactory`
  (`crates/lash-core-execution/src/runtime/vocabulary.rs:405`),
  `PostgresSessionStore` and `store_for` (§4).
- **The duplicate scoped and unscoped projections.**
  `SessionGraphCache::frame_read_model`, `scoped_read_model` and
  `project_scoped_read_model`
  (`crates/lash-core-store/src/session_graph_cache.rs:98`, `:198`, `:215`),
  the frame parameter of `SessionGraph::read_model`
  (`crates/lash-core-store/src/session_graph.rs:1191`), and
  `SessionGraphScopeError` (`:523`).
- **`message_tree`.** `SessionGraph::message_tree`,
  `nearest_message_ancestor`, `build_tree` and `build_tree_children`
  (`crates/lash-core-store/src/session_graph.rs:1524`, `:1553`, `:1612`,
  `:1623`); `SessionMessageTreeNode` (`:487`) and its facade re-export
  (`crates/lash/src/lib.rs:609`); and `SessionReadView::message_tree`
  (`crates/lash-core-store/src/session_read_view.rs:197`).
- **Derived-graph materialization.** The whole-graph clone in
  `SessionReadGraph::Derived`
  (`crates/lash-core-store/src/session_read_view.rs:126-141`). The tail
  rewrite stays over the window (§9).
  `from_persisted_state_with_relation_and_failures` (`:65`) and
  `turn_failure_settlements` (`:163`) go too.
- **Whole-graph helpers.** `SessionGraph::trim_to_active_path` and
  `try_trim_to_active_path`
  (`crates/lash-core-store/src/session_graph.rs:1430`, `:1444`), and
  SQLite's `Store::load_session_graph`
  (`crates/lash-sqlite-store/src/graph.rs:189`).
- **Growth kept for measurement.** Perf's `committed_node_ids` (§4).

### 13. Reader inventory and the gate

Every reader that materializes history is window-bound (**W**), explicitly
paged (**P**), a predicate (**Q**) or deleted (**D**). The last column names
the lane that ports it.

| Reader today | Disposition | Lane |
|---|---|---|
| SQL session and admitted-base loaders (`crates/lash-sqlite-store/src/persistence/session_commit.rs:96`, `:100`; `crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs:70`, `:74`) | W: `load_session_window` | SQLite, PostgreSQL |
| Shared load, state, read-view and refresh helpers (`crates/lash-core-store/src/store/load.rs:32-119`) | W: window helpers over `&SessionStore` | runtime |
| Runtime build, parked resume, init reopen (`crates/lash-core/src/runtime/builder.rs:263`, `crates/lash-core/src/runtime/lifecycle.rs:793`, `crates/lash-core/src/runtime/session_manager/session_init.rs:590`) | W | runtime |
| Refresh and admitted replay (`crates/lash-core/src/runtime/session_api.rs:421`, `:484`); append-replay and snapshot refresh (`crates/lash-core/src/runtime/session_ops.rs:358`, `crates/lash-core/src/runtime/session_manager/current.rs:34`) | W: `Current` / `Admitted(base)` | runtime |
| Facade open and send (`crates/lash/src/session.rs:509`, `:613`, `crates/lash/src/send.rs:121`) | W | runtime |
| Factory inspection (`crates/lash-sqlite-store/src/session_store_factory.rs:375`, `crates/lash-postgres-store/src/postgres/session_factory.rs:839`); public durable read (`crates/lash/src/durable_session.rs:429`) | W for the current frame. P through new facade `DurableSession::history(anchor, budget)` and `failure_evidence(after, limit)` | runtime (facade); SQLite and PostgreSQL delete their impls |
| Node reads (`crates/lash-sqlite-store/src/persistence/session_commit.rs:169`, `crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs:155`) and forwarding (`crates/lash-core-store/src/store/runtime_persistence_decorator.rs:53`, `crates/lash-perf/src/runtime_perf/store.rs:230`) | P: one-node page | backends; readers (perf) |
| SQLite whole-graph escape (`crates/lash-sqlite-store/src/graph.rs:189`) | D | SQLite |
| Graph cache and derived graph (`crates/lash-core-store/src/session_graph_cache.rs:83`, `crates/lash-core-store/src/session_read_view.rs:123`) | W: one projection; tail rewrite over the window | runtime |
| Trees (`crates/lash-core-store/src/session_graph.rs:1524`, `crates/lash-core-store/src/session_read_view.rs:197`) | D | runtime |
| Snapshots (`crates/lash-core-store/src/session_state.rs:771`) and the turn editor (`crates/lash-core/src/runtime/turn_graph_editor.rs:15`) | W | runtime |
| Workbench state reads (`examples/agent-workbench/src/main_sections/state_reads.rs:43`, `:96`) | W | readers |
| Workbench tree route (`examples/agent-workbench/src/main_sections/routes.rs:57`) and failure evidence (`:87`) | P: pages of turn-input nodes; failure pages | readers |
| Workbench chat projection (`examples/agent-workbench/src/main_sections/chat_projection.rs:395`) | W: current frame; P for earlier frames | readers |
| Slack boundary traversal (`examples/slack-clone/src/bot/threads.rs:453`) | P: pages from the head until the application node, or `contains_active_ancestor` for a leaf-only check | readers |
| Runbook tree (`runbooks/restate-postgres-workers/src/bin/context_overflow_recovery.rs:102`) | P | readers |
| Compaction snapshots (`crates/lash-plugin-standard-compaction/src/lib.rs:346`, `:755`, `:903`) | W; identity per §9 and §11 | runtime |
| Tool snapshots (`crates/lash-llm-tools/src/lib.rs:385`, `crates/lash-protocol-rlm/src/control_tools.rs:235`) | W | runtime |
| Export and current snapshots (`crates/lash/src/admin.rs:318`, `crates/lash-core/src/runtime/session_api.rs:222`, `crates/lash-core/src/runtime/session_manager/current.rs:51`, `crates/lash-core/src/runtime/turn_boundary.rs:339`) | W | runtime |
| Usage and failure folds (`crates/lash-sqlite-store/src/persistence/session_commit.rs:1297-1305`, `crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs:1296-1320`) | W totals; P ledger and failure pages | SQLite, PostgreSQL |
| Conformance lineage (`crates/lash-conformance/src/conformance/lineage.rs:63`), simulator (`crates/lash-sim/src/sqlite_faults.rs:623`), perf probe (`crates/lash-perf/src/runtime_perf/measurement/checkpoint.rs:249`), runbook assertion (`runbooks/restate-postgres-workers/src/bin/runner/process_assertions.rs:835`) | W or P per assertion; obsolete reader contracts removed | readers |
| Append pre-check (`crates/lash-core/src/runtime/session_ops.rs:139-149`) | storeless only | runtime |
| Historical-frame refusal (`crates/lash-core-store/src/session_graph.rs:1247`) | Q: `contains_active_ancestor` (§7) | runtime |

**The gate.** It has two parts, and both run in CI.

1. **The trait-surface test.** `history_reads_are_budgeted_or_predicates`
   lives in `crates/lash-core-store/src/store/history_gate_tests.rs`. It
   reads the operation list and fails when any operation returns
   `SessionWindowRead`, `HistoryPage`, `UsageLedgerPage`,
   `FailureEvidencePage`, `SessionNodeRecord` or `SessionGraph`, unless it
   passes one of two checks:
   - It is `load_session_window`, which is bounded by the frame.
   - It takes a `HistoryBudget` or a `NonZeroU32` limit.

   `contains_active_ancestor` and `load_usage_totals` sit on a named
   non-materializing list, each with a reason. Adding a new entry to that
   list is a reviewed change.
2. **The call-site allowlist.** `scripts/history-reader-allowlist.txt`, with
   a `.count` file, is checked by `scripts/check-history-readers.sh` in the
   format of `scripts/drive-store-allowlist.txt`. It pins every non-test
   call of `load_session_window`, `load_ancestors`,
   `load_usage_ledger_page` and `load_failure_evidence_page` in `crates/`,
   `examples/` and `runbooks/`, with the tag `WINDOW` or `PAGED`.
   - An unlisted call fails, and so does a stale count.
   - `WINDOW` calls may only occur under turn execution, reopen, refresh
     and read-view construction.
   - The script also denies the deleted names outright: `load_session(`,
     `load_session_at(`, `load_node(`, `message_tree(`,
     `load_session_graph(`, `open_unbound_store`,
     `select_readable_to_generation` and `load_whole_graph_tx`.

### 14. Acceptance tests

Each test names its target. Conformance cases live in
`crates/lash-conformance/src/conformance/session_history.rs`, and every
backend runs them: `//crates/lash-sqlite-store:conformance__test`,
`//crates/lash-sqlite-store:conformance_memory__test` and
`//crates/lash-postgres-store:conformance__test`. Fixtures build sessions
with several frames, forks, usage rows and failure receipts through the
public commit path. `StoreTestSupport` gains three required hooks:

```rust
fn decoded_row_counts_for_testing(&self) -> DecodedRowCounts;
async fn corrupt_graph_row_for_testing(
    &self,
    node_id: &NodeId,
    corruption: GraphRowCorruption,
) -> Result<(), StoreError>;
async fn set_head_current_frame_for_testing(
    &self,
    session_id: &SessionId,
    frame: Option<FrameNodeId>,
) -> Result<(), StoreError>;

pub struct DecodedRowCounts {
    pub graph_node_bodies: u64,
    pub usage_rows: u64,
    pub usage_holes: u64,
    pub turn_receipt_bodies: u64,
}

pub enum GraphRowCorruption {
    DeleteRow,
    SetParent(Option<NodeId>),
    SetFramePointer(NodeId),
    SetBodyBytes(u64),
}
```

The existing hooks that act on a handle gain `session_id`.

1. **SQL decoded-row counts** (conformance). Build a session with frames of
   40, 30 and 12 nodes, 500 usage rows with 3 outstanding holes, and 50
   failure receipts. A `Current` window read must decode exactly 12 node
   bodies, 3 usage holes, the per-key aggregates, 0 reported usage rows and
   0 receipt bodies. Adding 1,000 nodes to earlier frames changes none of
   those counts.
2. **Fork ceilings** (conformance).
   - Fork B from A at a node in A's second frame. Then A appends past the
     fork point and opens a new frame. B's window spans A's frame base up
     to the fork node and contains no A row above the ceiling.
   - B appends and switches frame. B's window is anchored at B's own
     `FrameOpen`.
   - Fork C from B inside a frame A owns. C's window honours both ceilings.
   - `contains_active_ancestor` answers `true` for B's and C's inherited
     rows, and `false` for A rows above the ceiling.
3. **Corrupt anchors** (conformance, plus
   `//crates/lash-core-store:lash-core-store__unit_test` for
   `SessionGraph::from_window`). Each case fails typed and never returns a
   smaller window:
   - a base that is not a `FrameOpen`;
   - a foreign frame pointer inside the window;
   - a middle row deleted;
   - a generation-0 base with a parent;
   - a base above generation 0 with no parent;
   - `body_bytes` that disagrees with the body;
   - a head pointer that disagrees with the leaf's frame
     (`CurrentFrameNodeMismatch`).
4. **Admitted replay** (conformance, plus
   `//crates/lash-core:runtime_turns__test`). Admit a turn in frame F1 whose
   own commit switches to F2 through `continue_as`.
   - `load_session_window(Admitted(base))` is anchored at F1, carries F1's
     config, and has no pending follow-on.
   - A replay's resident graph at the admission boundary equals that window
     node for node (the replay identity law of §9).
   - The standard-compaction request id computed on first execution equals
     the one computed on replay.
5. **Paging budgets and cursor stability** (conformance).
   - A `NodeBudget` stop and a `ByteBudget` stop each return the exact
     prefix.
   - A first node larger than `max_bytes` fails with `HistoryNodeTooLarge`,
     and `required_bytes` equals its `body_bytes`. Retrying with that
     budget succeeds.
   - Every page with a `next` is non-empty.
   - Paging to `Root` from `Head` yields the full ancestry exactly once, in
     descending generation, across frame and fork boundaries.
   - Appends and a head move between pages leave the later pages
     unchanged.
   - A foreign-session cursor fails with `CursorForeignSession`.
   - A tombstoned anchor fails with `HistoryAnchorUnavailable { reason:
     Tombstoned }`, and a vacuumed one with `NotReadable`.
   - A deleted session fails with `SessionDeleted`.
   - A one-node page matches the ported `load_node` fixtures.
   - `Head` on a headless session fails with `SessionNotFound`. On a head
     with no leaf it returns an empty `Root` page.
   - Siblings with skewed timestamps come back in generation order.
6. **Compaction and `continue_as` each start a new frame without a reload**
   (`//crates/lash-core:runtime_turns__test`, and
   `//crates/lash-plugin-standard-compaction:lash-plugin-standard-compaction__unit_test`
   for recovery). The cases are explicit compaction (6a), overflow recovery
   (6b) and `continue_as` (6c). After each commit:
   - `current_frame_node_id` is the new frame;
   - the window base is the new `FrameOpen`;
   - the resident node count equals the new frame's nodes;
   - `persisted_node_ids` is a subset of the resident ids;
   - `agent_frames` has one record, whose previous frame is the old one;
   - a counting `RuntimeStoreDecorator` saw zero `load_session_window`
     calls.

   6d is the pressure-path fix of §10: crossing the compaction threshold
   starts a frame. It lands with that fix.
7. **Shared projection identity**
   (`//crates/lash-core-store:lash-core-store__unit_test`,
   `//crates/lash-core:lash-core__unit_test`).
   - Two `read_model()` calls with no append between them return `Arc`s
     that are `ptr_eq` (messages, events, render cache).
   - `RuntimeSessionState::read_model`, `SessionReadView::from_persisted_state`
     and the turn editor's base share those `Arc`s.
   - An append folds once.
   - A pending `FrameOpen` moves the projection's start to the new frame
     before its commit.
   - `message_delta_if_current_preserved` takes the rope fast path.
8. **Fixed-frame memory and latency stay flat as earlier history grows**
   (`//crates/lash-perf:lash-perf__bin` with `dhat-heap`, new measurement
   `crates/lash-perf/src/runtime_perf/measurement/frame_residency_curve.rs`,
   budgets in `scripts/perf_guard_budgets.json`). Fix the current frame at
   64 nodes and vary the earlier history across 0, 1,000, 8,000 and 32,000
   nodes, on SQLite and PostgreSQL. Across that sweep:
   - resident heap after reopen differs by at most 1% plus 64 KiB;
   - decoded rows at reopen are identical;
   - median turn-commit latency at 32,000 is at most 1.2 times the latency
     at 0.

   The run publishes its command and counts.

### 15. Lanes and file ownership

There are four lanes, cut from `main` after FIG-3946 lands. They integrate
once, with no adapters or dual reads, onto the integration branch
`fig-1628/cutover`. A lane edits only the files it owns. A change another
lane needs goes to that lane's owner.

**Contract first.** The runtime lane's first commit, **C0**, contains every
shared Rust type and trait:
- the segment traits and the `RuntimeStore` alias;
- `history.rs` and `catalog.rs`;
- `session_view.rs`;
- the error variants;
- the decorator operation list and its three generated surfaces;
- the `StoreTestSupport` hooks and `SessionLookup`;
- `SessionGraph::from_window`, `WindowAnchor` and the anchored integrity
  rules;
- `SessionUsageTotals` and `fold_checked`;
- `DeploymentStore` and the conformance aliases;
- the `mod` line for the gate test file.

C0 must pass `kiln clippy` for `//crates/lash-core-store` and
`//crates/lash-core-execution`. The rest of the workspace stays red until
integration.

The SQLite lane's first commit, **S1**, stacks on C0. It holds the shared
column lists and inserts in `crates/lash-store-sql`. The PostgreSQL lane
stacks on S1. The readers lane's first commit, **K1**, is the conformance
suite `session_history.rs` and its fixtures. Both backend lanes rebase onto
K1 to verify. All four lanes then work in parallel.

| Lane | Owns |
|---|---|
| **SQLite** | `crates/lash-sqlite-store/**` (BUILD included); `crates/lash-store-sql/src/session/{graph_nodes,usage_deltas,turn_commits,fork_lineage}.rs`; new `crates/lash-store-sql/src/session/usage_delta_holes.rs`; `crates/lash-store-sql/src/session.rs` |
| **PostgreSQL** | `crates/lash-postgres-store/**` (`schema.sql`, `schema_shape/`, `migrate.rs` and BUILD included) |
| **Runtime and window** (C0 first) | `crates/lash-core-store/**` except `src/store/history_gate_tests.rs`; `crates/lash-core-execution/**`; `crates/lash-core/src/**` except `src/testing/**`; `crates/lash/src/**` and its BUILD (facade, including `DurableSession::history` and `failure_evidence`); `crates/lash-restate/**`; `crates/lash-restate-test/**`; `crates/lash-plugin-standard-compaction/**`; `crates/lash-llm-tools/**`; `crates/lash-protocol-rlm/**`; `crates/lash-protocol-standard/**`; `crates/lash-core-worker/**`; `crates/lash-remote-protocol/**`; `crates/lash-s3-store/**` |
| **Readers, perf and conformance** (K1 first) | `crates/lash-conformance/**`; `crates/lash-sim/**`; `crates/lash-perf/**`; `crates/lash-core/src/testing/**`; `crates/lash/tests/**`; `crates/lash-core-store/src/store/history_gate_tests.rs`; `examples/**`; `runbooks/**`; `scripts/history-reader-allowlist.txt` and `.count`; `scripts/check-history-readers.sh`; its wiring in `scripts/ci_plan.py`; `scripts/perf_guard_budgets.json` |

Each lane regenerates the BUILD files of the crates it owns. The final
integration merge, and the one full gate on it, belong to the orchestrator.
Offload (FIG-1643) and pressure reporting (FIG-1644) stay deferred.

## Consequences

- There is one store object per catalog, and it is the multi-session
  interface. Session identity is always an explicit argument or a checked
  field of a request. The FIG-885 class, where two representations of one
  identity can disagree, is gone.
- A host keeps `Arc<dyn DeploymentStore>`, and runtime code keeps
  `SessionStore`. A capability that is missing no longer compiles as a
  default that answers `Unsupported` at runtime.
- An active session's memory follows its current frame. Earlier frames cost
  nothing until something pages them. What bounds a frame is compaction.
  §10's defect means the automatic path does not bound it yet.
- History readers pay visibly, one page at a time in a loop they write.
  There is no whole-graph escape hatch, and the gate of §13 keeps it that
  way.
- The public facade changes. `SessionMessageTreeNode`,
  `SessionReadView::message_tree` and `turn_failure_settlements` go away,
  and `DurableSession` gains paged reads. The facade lane adds
  `//crates/lash:ui_fixtures` to its checks.
- Deploying the cutover changes stored shapes in place and changes the
  standard-compaction identity inputs. In-flight journals drain on the
  build that wrote them (§11).

## Amendment (FIG-4125, 2026-09-29)

Item 19: The `fig-1628-int-merge` target composes ten operational store
segments plus `FleetFormatStore` in `RuntimeStore`. It has no
`bind_artifact_stores` method. `DriveEpochStore` carries the drive fence,
and the drive owns turn authority. This integration branch has not landed on
main; the earlier §1 and §2 sketches describe the old baseline, not current
implementation.
