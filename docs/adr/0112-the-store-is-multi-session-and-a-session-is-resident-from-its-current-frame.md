# 0112: The store is multi-session, and a session is resident from its current frame

## Status

Accepted.

## Context

A catalog serves many sessions through one backend object. Runtime calls
still need a convenient session-local view whose identity cannot disagree
with a request. An active session needs its current frame and pending
changes; reading earlier history needs explicit paging costs.

A frame is the context window. Compaction and `continue_as` open a frame.
Resident memory follows that frame, rather than a fixed byte cap. The
pre-1.0 version freeze applies to durable shapes.

## Decision

### 1. One multi-session store

`RuntimeStore` is a blanket alias composing `FleetFormatStore`,
`AttachmentReferrers`, `SessionCatalogStore`, `SessionCommitStore`,
`SessionHistoryStore`, `TurnInputStore`, `QueuedWorkStore`, `ShiftEpochStore`,
`RunStore` and `StoreMaintenance`. Backends implement the component traits.
A session-scoped operation takes the session id or a request that carries
it. Catalog-wide operations state their scope.

Backend operations are required. Provided methods compose required
primitives, such as head reads or input admission; they do not invent
unsupported, empty or oldest-version backend answers. The operation list
generates decorators and the session view's forwarders. Execution authority is the session actor's epoch fence; the actor's owner runs
turns ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §3).

Evidence: `crates/lash-core-store/src/store/mod.rs:1063`, `:1790`, and
`crates/lash-core-store/src/store/runtime_store_decorator.rs`.

#### 1.1 `SessionCatalogStore`

`admit_session` is one transaction. It validates the id, refuses a permanent
deletion tombstone, and inserts missing metadata from the request as
`Created`. An existing session checks rebind lineage, returns `Rebound` on
agreement and leaves metadata untouched. A conflicting relation refuses as
`SessionRelationMismatch`. A root relation declares no lineage to override.

`lookup_session` is read-only and answers `Live(SessionMeta)`, `Deleted` or
`Absent`. Failure to establish the answer is an error. `list_sessions`
returns durable `SessionView` rows, including visible deletion tombstones.
The same segment owns forks, pins, retained fork-point enumeration and
physical storage deletion. A new fork creates a head at a retained point
without copying graph nodes.

Evidence: `crates/lash-core-store/src/store/catalog.rs:18`,
`crates/lash-sqlite-store/src/catalog.rs`, and
`crates/lash-postgres-store/src/postgres/session_catalog.rs`.

#### 1.2 `SessionCommitStore`

This segment owns head commits, checkpoint hydration, metadata, parks and
turn-commit idempotency. Its atomic commit also settles the admitted ingress
and applied commands it names. Every session read takes a session id;
a mutation's request carries its session identity or the owner's epoch.
History reads belong to §1.3.

Evidence: `crates/lash-core-store/src/store/mod.rs:1040`.

#### 1.3 `SessionHistoryStore`

Materializing reads are `load_session_window`, `load_ancestors`,
and `load_failure_evidence_page`. The first is
frame-bound. The others take explicit budgets or limits. The segment also
owns `contains_active_ancestor`, which returns a predicate rather than
historical payloads.

Evidence: `crates/lash-core-store/src/store/history.rs:362`.

#### 1.4 Inputs, queued work and maintenance

Turn-input and queued-work operations carry the session explicitly.
`has_admissible_queued_work` returns a boolean rather than an unknown probe.
`vacuum(session_id)` is session-scoped, including that session's retired
rows. `gc_unreachable` is catalog-wide.

Evidence: `crates/lash-core-store/src/store/mod.rs` and
`crates/lash-core-store/src/store/runtime_store_decorator.rs`.

#### 1.5 Typed errors

A foreign request refuses as `ForeignSessionRequest`. History errors include
`SessionNotFound`, `SessionDeleted`, `InvalidWindowAnchor`,
`HistoryAnchorUnavailable`, `HistoryNodeTooLarge`, `CursorForeignSession`
and `HistoryCursorLineageChanged`. The current frame is derived from the
head leaf row; neither the head payload nor a commit claims a second pointer.

An invalid window never silently becomes a smaller valid window. Its
anchor, parent-edge or stored-data violation remains a typed error.

Evidence: `crates/lash-core-store/src/store/error.rs` and
`crates/lash-core-store/src/session_graph_window.rs:58`.

### 2. Deployment operations span the catalog

`DeploymentStore` composes `RuntimeStore`, `AttachmentRootSet` and
`ControlIntentStore`. It requires effect-host binding, artifact-frame
retention checks, unsettled-turn accounting, park listing and feeds,
non-terminal-run paging and lost-run completion, control-intent listing,
cancel-scope retirement and evidence reclamation. It has no
`bind_artifact_stores` operation.

Hosts, the facade and the engine hold `Arc<dyn DeploymentStore>`. Runtime
code reaches the same object through an `Arc<dyn RuntimeStore>` in its
`SessionStore` view. A live-session lookup combines `lookup_session` with
view construction; a failed lookup remains an error.

`DeploymentStoreDecorator` forwards deployment operations and builds on
`RuntimeStoreDecorator`. Conformance uses `ConformanceStore` and
`ConformanceDeployment`, which add `StoreTestSupport` to the production
contracts.

Evidence: `crates/lash-core-execution/src/runtime/vocabulary.rs:425`,
`crates/lash-core-execution/src/runtime/deployment_store_decorator.rs:60`,
and `crates/lash-core-store/src/store/mod.rs:1786`.

### 3. `SessionStore` is the permanent thin view

`SessionStore` holds exactly an `Arc<dyn RuntimeStore>` and a `SessionId`.
Construction validates the id and performs no admission or read. The view
has no backend state, identity latch or discovery fallback and implements
no store trait.

Generated inherent methods supply the view's id to session-scoped
operations. For a request that carries an identity, the forwarder checks
that identity against the view before reaching the backend. A mismatch is
`ForeignSessionRequest`. Catalog and deployment operations require the
underlying store, not the view.

Passing an explicit id on every runtime call would avoid forwarders, but
would discard convenient session-local calls and their request-identity
check. A backend-bound handle would duplicate session identity and backend
resources. The thin view retains one backend identity source.

Evidence: `crates/lash-core-store/src/store/session_view.rs:25`, `:41`, `:120`, `:135`.

### 4. Backend resources belong to the catalog

`SqliteStore` holds a writer connection and fixed reader pool, the database
keepalive, clock, options, cancellation binding and catalog counters.
It has no selected-session latch. SQLite memory uses the same SQL store
with a keepalive for its database. `PostgresStore` holds its pool, catalog id,
writer fence, clock, cancellation binding and test counters; it is not a
session-bound handle.

`RuntimePerfStore` decorates the deployment store and counts committed
nodes with an atomic counter rather than retaining every committed node id.
Blob storage remains a separate backend responsibility.

Evidence: `crates/lash-sqlite-store/src/lib.rs:195`,
`crates/lash-sqlite-store/src/lifecycle.rs:236`,
`crates/lash-postgres-store/src/lib.rs:668`, and
`crates/lash-perf/src/runtime_perf/store.rs:30`, `:209`.

### 5. `load_session_window`

`WindowSelector::Current` selects the live head. `Admitted(base)` selects
the recorded admission's leaf, checkpoint and revision, and carries no
pending follow-on. The selected leaf's stored `frame_node_id` identifies
the window base and, under `Current`, the session's current frame.
An admitted leaf must also remain reachable from the session head.

The window spans the base's `FrameOpen` through the selected leaf. SQL
conjoins the frame-to-leaf generation range with the session's own rows and
each ancestor's individual fork ceiling. Ceilings select candidates;
parent edges remain ancestry authority under
[ADR 0057](0057-history-generations-accelerate-edge-authoritative-reads.md).
A fork can read a frame whose base belongs to an ancestor.

Head, graph, checkpoint and usage totals share one read snapshot. SQLite
uses its read transaction; PostgreSQL uses repeatable-read, read-only
transactions. Config comes from the head for its current frame and from the
selected frame's `FrameOpen` otherwise. Usage totals are the live session's
accounting under either selector.

Rows have ascending, contiguous generations. The first row is the frame's
`FrameOpen` with its own frame pointer; later rows point to that frame and
the preceding node. The last row is the selected leaf. Stored `body_bytes`
must match the encoded body's length before decoding.

`WindowAnchor` records the base id and generation, its external parent and
the previous frame. Generation zero has no external parent; a later base
has exactly one. No other dangling parent is valid. `SessionGraph::from_window`
checks the decoded chain as well, so a third-party backend cannot hand the
runtime an arbitrary suffix.

`Current` answers `None` only without a head row. A head with no leaf has
an empty, unanchored window. `Admitted` never substitutes another head;
an unavailable base returns `TurnBaseNotRetained`. Deleted sessions return
`SessionDeleted`. Window reads decode frame nodes and checkpoint components without decoding
failure-receipt bodies.

Evidence: `crates/lash-sqlite-store/src/history.rs:243`,
`crates/lash-postgres-store/src/postgres/runtime_persistence/history.rs:24`, `:273`,
and `crates/lash-core-store/src/session_graph_window.rs:58`.

### 6. `load_ancestors`

A `HistoryAnchor` is `Head`, `Node(id)` or `Cursor(cursor)`. A page walks one
ancestry in descending generation, across frames and fork points. Each
neighbor must match the preceding node's parent and generation. Timestamps
do not order the chain. Named anchors also pass a parent-edge reachability
check against the live head, in addition to their fork-ceiling selection.

`HistoryBudget` requires nonzero node and byte limits. The backend first
reads bounded headers, accounts for stored `body_bytes`, and fetches bodies
only for the accepted prefix. It does not load full bodies and truncate them
afterward. A first row exceeding the byte limit returns
`HistoryNodeTooLarge` with its required size. A resumable page is nonempty.
`HistoryStop` is `Run`, `NodeBudget` or `ByteBudget`; only a budget stop
has a continuation cursor.

Cursors carry the session, pinned leaf, lineage stamp and next node and
generation. Another session refuses as `CursorForeignSession`; a changed
lineage stamp refuses as `HistoryCursorLineageChanged`. The stamp hashes the
ordered ancestor-id and fork-generation pairs in `lash-history-lineage/v1`.
Appending leaves continuation stable. A missing or unreadable next node
returns `HistoryAnchorUnavailable`, with `Tombstoned` or `NotReadable`.
Head movement that makes the anchor unreachable can also refuse the read.

`Head` without a head row is `SessionNotFound`. A head without a leaf
returns an empty run page. A deleted session is `SessionDeleted`.
A one-node budget at `Node(id)` provides a single-node read through the same
membership and size checks.

Evidence: `crates/lash-core-store/src/store/history.rs:94`, `:106`, `:147`, `:218`,
`crates/lash-sqlite-store/src/history.rs:567`, and
`crates/lash-postgres-store/src/postgres/runtime_persistence/history.rs:499`.

### 7. `contains_active_ancestor`

This predicate answers whether the live head reaches a candidate node.
It selects a live candidate under the session's ownership or individual
fork ceilings, then confirms membership through parent edges. The
`HeadPathProbe` walks owner boundaries without decoding graph bodies;
a ceiling alone does not prove active ancestry. A head without a leaf
answers false. A deleted session refuses as `SessionDeleted`.

The read does not replace the write transaction's append fence or head
compare-and-swap. A successful predicate can become stale before a commit.
Runtime historical-frame checks use this predicate instead of assuming
all prior frames are resident. `NodeIdCollision` remains a commit refusal.

Evidence: `crates/lash-sqlite-store/src/history.rs:455`,
`crates/lash-postgres-store/src/postgres/runtime_persistence/history.rs:712`,
and `crates/lash-core-store/src/store_backend_support/head_path.rs:93`.

### 8. Bounded failure-evidence reads

Failure evidence is separate from the resident read view. Stored
`failure_evidence` marks relevant receipts, with a partial index on session,
commit time and turn id. `load_failure_evidence_page` orders by commit time
and turn id and decodes selected receipts. Its cursor checks session identity.
Model usage is data on the recorded model result, as
[ADR 0127](0127-usage-is-result-data-hosts-meter-spend.md) specifies.

### 9. Resident state

A store-backed runtime adopts the selected frame window and keeps pending
nodes until they become durable. Marking an accepted commit's node ids
persisted calls `retire_below_current_frame`. Trimming anchors at the latest
durable `FrameOpen`, removes older durable nodes and their persisted ids,
and derives resident frame records. It waits while a pending node still
depends on a node it would drop.

The current frame's read model is shared through one memoized projection.
A pending `FrameOpen` selects the new frame before its commit. Read views
and the turn editor use the same projection; a derived view rewrites the
tail over its resident graph. Standard compaction's request identity uses
that snapshot, so the admission-boundary resident graph must equal the
admitted window on execution and resume.

Resident graph cost follows the current frame and pending state. Usage
cost also includes grouped keys and outstanding holes. This is not a byte
cap or a guarantee that a frame never grows. A runtime with no backing
store cannot page durable history and retains what it holds.

Evidence: `crates/lash-core-store/src/session_state.rs:964`,
`crates/lash-core-store/src/session_graph_window.rs:129`,
`crates/lash-core-store/src/session_graph_cache.rs:200`, and
`crates/lash-core-store/src/session_read_view.rs:118`.

### 10. Frames change on compaction and `continue_as`

Explicit compaction, pressure compaction, overflow recovery and
`continue_as` open and commit a `FrameOpen`. An accepted receipt then trims
resident history through §9. The pressure hook returns a frame-opening
decision; core persists that frame before the model call. Cutting only the
prompt view would leave durable residency growing, so frame-opening
compaction is the boundary for both prompt context and resident history.

Evidence: `crates/lash-core/src/runtime/turn_loop/context_pressure.rs:116`, `:263`,
`crates/lash-core/src/runtime/turn_boundary.rs`, and
`crates/lash/src/tests/standard_compaction_persistence.rs`.

### 11. Stored shapes

Graph rows carry `body_bytes`. Turn-commit rows carry `failure_evidence`.
Graph windows carry an anchor. Model results carry reported usage and attempt
history in the model call's committed phase.

Both SQL backends store these facts. Decoding validates body size and the
window's anchor. The pre-1.0 version freeze applies; durable-format admission,
writer fences and drain by release follow
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md).
A compaction's identity includes its window and token inputs, so resume
must preserve those inputs.

Evidence: `crates/lash-store-sql/src/session/graph_nodes.rs`,
`crates/lash-store-sql/src/session/usage_delta_holes.rs`,
`crates/lash-store-sql/src/session/turn_commits.rs`, and
`crates/lash-core-store/src/session_graph_window.rs:12`.

### 12. Window load helpers

`load_session_window_state`, `load_session_read_view` and
`refresh_session_window` take `SessionStore` and adopt window reads. Runtime
open, reopen, refresh and resume use these frame-bound helpers. Earlier
history is available through pages, not a whole-history store load or a
message-tree escape.

Evidence: `crates/lash-core-store/src/store/window_load.rs:32`, `:71`, and
`crates/lash/src/durable_session.rs:357`.

### 13. Reader inventory and the gate

History readers are frame-window reads, explicit pages or predicates.
The trait test `history_reads_are_budgeted_or_predicates` checks the
operation list's materializing result types. A materializing operation must
be `load_session_window` or take `HistoryBudget` or `NonZeroU32`. It also
pins the predicate and aggregate signatures.

`scripts/check-history-readers.sh` checks call sites against
`scripts/history-reader-allowlist.txt` and its count file. `WINDOW` entries name turn,
reopen, refresh or read-view context; `PAGED` entries name explicit page
reads. It also rejects unbounded load and tree entry points. A new history
reader requires a reviewed entry and matching count.

Evidence: `crates/lash-core-store/src/store/history_gate_tests.rs:4` and
`scripts/check-history-readers.sh:14`.

### 14. Executable evidence

Store laws run on SQLite file, SQLite memory and PostgreSQL. The shared
history suite is `crates/lash-conformance/src/conformance/session_history.rs`.
Runtime laws run the production runtime over a fault-injecting store with
labelled commits, a virtual clock and `SimNodes` (ADR 0132 §14). Upgrade proofs use the synthetic-next tier.
The following numbered groups identify the law each citation refers to.

#### 14.1 Frame-bounded decoded rows

`history_window_is_frame_bounded` checks decoded graph, usage and receipt
counts while earlier history grows.

#### 14.2 Fork ceilings and edge authority

`history_fork_respects_ceiling` checks inherited windows.
`inflated_fork_ceiling_cannot_expose_post_fork_source_nodes` checks that
ceilings cannot authorize rows outside the parent chain.

#### 14.3 Corrupt anchors

`history_window_rejects_corrupt_anchors` and the window-construction unit
tests reject malformed bases, pointers, parent edges and stored body sizes.

#### 14.4 Admitted resume

`crates/lash-conformance/src/conformance/frame_open_redrive/adversarial.rs`
and `crates/lash-core/tests/runtime/tests/turns/frame_residency.rs` exercise
admitted frame state across resume and frame switches. The resident graph
and compaction identity must agree with the admitted window.

#### 14.5 Paging and cursor stability

`history_pages_are_bounded_and_pinned` exercises byte and node budgets,
cursor continuation, appends, unavailable anchors and session checks.
`history_selection_and_confirmation_share_one_snapshot` exercises selection
and ancestry confirmation within one snapshot.

#### 14.6 Compaction and `continue_as` residency

`crates/lash-core/tests/runtime/tests/turns/frame_residency.rs` exercises
explicit compaction, context-pressure frames and `continue_as`, checking
frame switches and receipt-time trimming without a history reload. The
facade test `overflow_recovery_starts_a_frame_without_a_reload` covers
overflow recovery. `pressure_compaction_opens_a_summary_frame_the_turn_continues_in`
covers pressure compaction in that same facade test file,
`crates/lash/src/tests/standard_compaction_persistence.rs:209`, `:642`.

#### 14.7 Shared projection identity

`crates/lash-core/src/runtime/turn_commit_draft.rs` and the graph-cache tests
check the shared frame projection and the message-delta fast path.

#### 14.8 Frame residency measurement

`crates/lash-perf/src/runtime_perf/measurement/frame_residency_curve.rs`
measures fixed-frame residency as earlier history grows. The configured
budgets in `scripts/perf_guard_budgets.json` use 64 current-frame nodes,
earlier-history points 0, 1,000, 8,000 and 32,000, a heap-growth allowance of
1% plus 64 KiB, and a maximum commit-median ratio of 1.2.

## Consequences

One catalog object owns backend resources and serves multiple sessions.
The thin view checks request identity before forwarding. Current-frame
reads make earlier history a cost the caller chooses through explicit pages.
Compaction bounds a frame's growth through a durable frame switch.

Whole-history reads would make reopen and resident memory follow all prior
frames. A byte cap would require a separate eviction and hydration policy.
Frame windows reuse the runtime's context boundary, while node and byte
budgets make historical reads explicit. Fork ceilings accelerate selection;
checked parent edges preserve ancestry authority.

## Model usage

Usage is data on the model call's recorded result. Hosts meter spend at the
`Provider` seam under [ADR 0127](0127-usage-is-result-data-hosts-meter-spend.md).
Lash has no accounting ledger or delivery dependency.
