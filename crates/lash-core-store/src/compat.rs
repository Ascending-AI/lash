//! The component compatibility descriptor (ADR 0115 §1).
//!
//! Every versioned stored component (the PostgreSQL schema, the SQLite
//! database file, each Restate object family) carries a durable stamp
//! `{version, min_reader}`. A build declares which stamps it opens and which
//! versions it produces in one [`CompatDescriptor`] per component, and every
//! open runs [`admit`] on the stamp before it takes traffic. A refusal is a
//! typed [`CompatRefusal`] whose message names the `lashctl` remedy.
//!
//! The stamps themselves are written and read by the backends; this module
//! owns only the vocabulary and the rule, so both stores and the Restate
//! objects answer the same way.

use serde::{Deserialize, Serialize};

pub use lash_sansio::VersionRange;

/// One versioned stored component: a PostgreSQL schema, a SQLite database
/// file, or one Restate object family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ComponentId(&'static str);

impl ComponentId {
    /// The PostgreSQL schema; its stamp is `lash_schema_versions` row
    /// `lash-postgres-store`.
    pub const POSTGRES: Self = Self("postgres");
    /// A SQLite deployment's one database file; its stamp is its
    /// `lash_compat` row.
    pub const SQLITE_CORE: Self = Self("sqlite-core");
    /// Every `LashDurableWaitIndex` object; its stamp is the object's `_compat`.
    pub const RESTATE_DURABLE_WAIT_REGISTRY: Self = Self("restate-durable-wait-registry");

    /// The component's stable name, as refusals and `lashctl version` print it.
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for ComponentId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

/// What this build declares about one component.
#[derive(Clone, Copy, Debug)]
pub struct CompatDescriptor {
    pub component: ComponentId,
    /// The stamps this build opens: `[oldest it still reads, newest it knows]`.
    pub reads: VersionRange,
    /// The versions its migrations or encoders can produce.
    pub writes: VersionRange,
}

/// The PostgreSQL schema's version: the one number `schema.sql` seeds into
/// `lash_schema_versions`, the migration ledger records, the shape artifact
/// names and the release stamp carries. Every reader takes it from the
/// PostgreSQL descriptor below; no backend restates it.
///
/// version_guard(
///     file(
///         path = "crates/lash-postgres-store/schema.sql",
///         cover(
///             "CREATE TABLE IF NOT EXISTS lash_schema_versions",
///             "CREATE TABLE IF NOT EXISTS lash_blobs", "CREATE TABLE IF NOT EXISTS lash_session_head",
///             "CREATE TABLE IF NOT EXISTS lash_graph_nodes",
///             "CREATE TABLE IF NOT EXISTS lash_session_meta",
///             "CREATE TABLE IF NOT EXISTS lash_runtime_turn_commits",
///             "CREATE TABLE IF NOT EXISTS lash_queued_work_batches",
///             "CREATE TABLE IF NOT EXISTS lash_pending_turn_inputs",
///             "CREATE TABLE IF NOT EXISTS lash_processes",
///             "CREATE TABLE IF NOT EXISTS lash_process_events",
///             "CREATE TABLE IF NOT EXISTS lash_process_wake_deliveries",
///             "CREATE TABLE IF NOT EXISTS lash_trigger_subscriptions",
///             "CREATE TABLE IF NOT EXISTS lash_trigger_occurrences",
///             "CREATE TABLE IF NOT EXISTS lash_trigger_deliveries",
///             "CREATE TABLE IF NOT EXISTS lash_lashlang_artifacts",
///         ),
///     ),
///     shapes(
///         path = "crates/lash-core-execution/src/runtime/effect/envelope.rs",
///         cover(
///             RuntimeEffectInvocation, RuntimeEffectEnvelope, RuntimeEffectCommand,
///             RuntimeEffectOutcome,
///         ),
///     ),
///     roots(path = "crates/lash-sansio/src/session_model/mod.rs", TurnOutcome, ErrorEnvelope),
///     catalog(path = "crates/lash-postgres-store/src/postgres/migrate.rs", EXPAND_MIGRATIONS),
/// )
/// version_surface = "migrate"
/// format_outside_manifest = "store schema version: declared in compat.rs for the component's descriptor and read from the deployment through StorePreflight::schema_status, not reported in the durable-format manifest"
/// version_unguarded = "store schema version: a catalog step moves it, and admission reads it through the component's compat descriptor at open (ADR 0115 §1.3), never through a record decoder"
pub const POSTGRES_SCHEMA_VERSION: u32 = 141;

/// The SQLite database's version: its `lash_compat` row and its entry in the
/// release stamp. A SQLite deployment is one database file (ADR 0132 §12),
/// so this one number versions every table the file holds: the durable core,
/// the process registry and the trigger store.
///
/// Its history follows. Before the catalog existed each value was a
/// reject-and-recreate boundary: an older database is deleted before
/// opening, not migrated. The durable-core `SCHEMA` doc comment in
/// `lash-sqlite-store` has the rationale.
///
/// Bumped to 10 for the attachment three-layer cutover (ADR 0028): the
/// the legacy attachment ownership table this schema gates carried, pre-cutover, committed refs
/// and canonical URIs that named `sessions/<hash>/...` blob paths the flat
/// content-addressed layout cannot read. Pre-10 session databases are rejected
/// at open and recreated; the old `sessions/` blob trees are unreachable garbage
/// operators delete manually.
///
/// Bumped to 11 for claim generation fencing: queued-work and
/// pending-turn-input rows replace their per-claim claimed-at and expiry
/// columns with a single column pinning the session-execution-lease generation
/// the claim was taken under (since replaced by run admission, FIG-3927).
/// There is no migration chain — pre-11 session databases are rejected at open
/// and recreated.
/// Bumped to 12 for FIG-546 owner-bound attachment intents. This is a
/// reject-and-recreate cutover: pre-12 manifests have no durable execution
/// owner and cannot participate in reachability-based reclamation.
///
/// Bumped to 13 for FIG-636's factory-wide durable-core catalog. Session heads,
/// metadata, graph rows, and usage deltas are now keyed by `session_id`; node
/// ids remain globally unique across the one database. Pre-13 per-session
/// databases are rejected and must be recreated.
///
/// Bumped to 14 for FIG-654's reachability model. Parent edges, head roots,
/// and cached incoming counts are queryable rows;
/// graph structure no longer lives inside `node_json`.
///
/// Bumped to 15 for FIG-634 first-class forks. `node_anchors` makes explicit
/// continuation pins node and checkpoint roots in the same transaction domain
/// as heads and graph edges.
///
/// Bumped to 16 so an anchor binds the continuation checkpoint and source
/// session as one immutable snapshot rather than selecting either later.
///
/// Bumped to 17 so a reusable session name has a durable per-lifetime
/// incarnation for node and effect-replay identity.
///
/// Bumped to 18 because runtime commit receipts no longer persist the removed
/// realization digest; stores derive their lookup hash from commit content.
///
/// Bumped to 19 to remove cached graph-node reference counts. Node retirement
/// now derives liveness from parent edges, session heads, and anchors.
///
/// Bumped to 20 for permanent session-id tombstones and the removal of
/// per-lifetime incarnation identity. Pre-20 stores are rejected and recreated.
///
/// Bumped to 21 for consumed process-wake source-key evidence that survives
/// queue drain. Pre-21 durable-core catalogs are rejected and recreated.
///
/// Bumped to 22 to replace per-message evidence with monotone consumed
/// high-water marks. Pre-22 durable-core catalogs are rejected and recreated.
///
/// Bumped to 23 for the session-create and process-identity cutover.
///
/// Bumped to 24 to rename consumed wake high-water marks as receiver allocation
/// fences and add durable sender allocation floors. Process-event sequences
/// remain small and monotone across pruned incarnations.
///
/// Bumped to 25 for FIG-850 append-request identity receipts and idempotent
/// usage publication. Receipt identity columns are nullable so a pre-upgrade
/// row copied into the new schema retains exact-commit-hash semantics; usage
/// rows carry a required operation key, ordinal, payload-encoding version, and
/// canonical payload hash unique within a session. This unreleased schema was
/// completed in place; operators still use the store family's reject-and-
/// recreate flow rather than an in-place migration.
/// Version 25 also rejects session and artifact rows carrying pre-FIG-886
/// identities as part of the coordinated cutover.
/// Version 26 rejects pre-FIG-915 usage identities and session rows carrying
/// the former tool-batch or plugin-message names.
/// Version 27 adds the required per-turn budget to session-head configuration,
/// frame policy snapshots, and process execution environment artifacts. Older
/// databases are rejected and recreated; there is no compatibility read path.
/// Version 28 adds immutable graph generations and frame pointers plus
/// zero-copy fork-lineage accelerators. Older databases are rejected and
/// recreated; there is no backfill or compatibility read path.
/// Version 29 replaces the fixed checkpoint slots with a complete keyed
/// component descriptor set carrying per-component encoding versions. Older
/// runs have no honest compatibility interpretation and are rejected with the
/// existing recreate-store remedy.
/// Version 30 removes the CLI-era session name, creation timestamp, model, and
/// working-directory columns from session metadata. Older databases are
/// rejected and recreated; there is no compatibility read path.
/// Version 32 makes nested session metadata strict.
/// Version 33 replaces that JSON carrier with structural columns and narrow
/// ordered child tables. Older databases are rejected and recreated; there is
/// no JSON or compatibility read path.
/// Version 35 adds queued-work batch identity and coalescing metadata.
/// Version 36 adds the runtime-minted executor discriminator and store-authored
/// lease term to session lease rows.
/// Version 37 adds the attachment GC fence's per-digest condemnation table.
/// Older databases are rejected and recreated; there is no compatibility read
/// path.
/// Version 38 projects checkpoint-manifest component edges into an indexed
/// relation so owner-delete reclaim can decide blob liveness inside the
/// severing transaction. Version-37 catalogs are armed in place by decoding
/// every manifest reachable from a session head or node anchor and inserting
/// its exact component edges in the same transaction that stamps version 38.
/// Catalogs below 37 remain reject-and-recreate boundaries.
/// Version 39 adds core-owned creation and last-commit timestamps to session
/// catalog rows and preserves their enumeration projection on permanent
/// deletion tombstones. Older stores cannot reconstruct an honest creation
/// time and are rejected under the existing recreate-store policy.
///
/// An index-only catalog change does **not** bump this version. Every
/// `CREATE INDEX` above is `IF NOT EXISTS`, obsolete indexes are dropped by
/// name, and open always runs the whole schema. A same-version file self-heals
/// into the current index set on first open, and an older binary can still read
/// the newer file. Bumping would reject-and-recreate live stores for a change
/// that can be applied in place. The idle-arbitration ordering index
/// (`idx_queued_work_session_command_order`) was added under exactly this carve-out. It
/// covers index-only additions and nothing else: any table, column, or
/// semantic change bumps.
/// Version 40 persists per-turn cancellation requests and their undelivered
/// input outcomes.
/// Version 41 adds the nullable independently readable session-state generation
/// beside durable session binding metadata. NULL is the version-zero legacy map.
/// Version 42 removes the graph-node sequence column. Per-session generation is
/// the sole durable graph ordering authority.
/// Version 43 makes runtime append receipt identity columns all-or-none and
/// removes the readerless requested-ancestor receipt column. Older stores are
/// rejected and recreated; there is no compatibility read or migration path.
/// Version 44 folds the two pending observer-intent encodings into one
/// attributed table and removes the relation-wrapper depth counter. Version-43
/// catalogs are rejected and recreated like every other predecessor: the
/// in-place fold was deleted under the store-version window.
/// Version 45 switches content and semantic identities to domain-tagged BLAKE3.
/// Existing stores are rejected rather than reinterpreting SHA-256 rows.
/// Version 46 adds DDL-enforced session relation, causal-reference, and observer-
/// inheritance vocabularies. Existing durable-core catalogs are rejected rather
/// than migrated.
/// Version 47 makes session-execution-lease identity all-or-none and removes the
/// unused owner-liveness column. Existing catalogs are rejected rather than
/// migrated.
/// Version 48 constrains queued-work vocabulary and claim correlation while
/// removing its unread owner columns. Existing catalogs are rejected rather
/// than migrated.
/// Version 49 constrains pending-turn-input state and scope correlation while
/// removing the unread claim-owner-liveness column. SQL CHECK NULL semantics
/// let ingress JSON without a `scope` key pass both checks; serde cannot emit
/// that shape, so the behavior is identical across backends. Existing
/// catalogs are rejected rather than migrated.
/// Version 50 stores checked `FrameKey` values in every frame-open node. Existing
/// catalogs contain raw initial-frame keys and are rejected rather than decoded
/// through a legacy path.
/// Version 51 admits semantic-boundary receipt identities (FIG-2480): the
/// runtime-turn-commit identity CHECK now accepts a populated hash and version
/// with a NULL requested-node count. Existing catalogs are rejected rather
/// than migrated.
/// ADR 0078 replaces plugin snapshots with mediated namespace state; older
/// catalogs are refused before any prior payload can be read.
/// Version 53 persists each usage delta's typed disposition
/// (`usage_deltas.usage_disposition_json`, FIG-2765). Version 52 rows carry no
/// disposition at all and their unreported holes cannot be reconstructed, so
/// existing catalogs are rejected rather than migrated with a defaulted column.
/// Version 54 preserves successful attachment deletion as the terminal
/// `reclaimed` phase so adoption can refuse runs whose bytes are absent.
/// Version 55 keeps that phase present under an opaque write token associated
/// with its manifest session until a restoring backend put succeeds, so failed
/// re-puts and explicit host recovery can restore it exactly.
/// Version 56 persists full effect addresses in session causal metadata.
/// Version 57 also requires pending-input claim identity and token to be either
/// both NULL or both populated; both version-56 parent catalogs are recreated.
/// Version 58 adds exact owner edges and permanent execution-owner publication
/// fences. Version-57 catalogs are rejected and recreated.
/// Version 59 qualifies process-owned attachment intents with the registry-minted
/// incarnation. Version-58 catalogs are rejected so a bare process id is never
/// reinterpreted as the current incarnation with the same reusable name.
/// Bumped to 61 for FIG-2795: attachment adoption requires upload evidence.
/// the legacy attachment ownership table gains `write_id` and `written_at_ms`, and the
/// `attachment_condemnations` phase vocabulary drops `reclaimed` — a pre-61
/// database can hold rows in a phase this schema forbids and manifest rows with
/// no upload evidence for bytes that are present, so it is rejected at open and
/// recreated.
/// Bumped to 62 for FIG-2962/FIG-2963: the parent scope is a registration fact
/// and the end of a scope is one ledger row. `processes` gains
/// `parent_scope_kind`, `parent_scope_id`, `on_parent_end` and
/// a cancel-request column, and `process_parent_end_plans` is replaced by the
/// scope-keyed `parent_end_plans`. A pre-62 catalog holds children with no
/// parent scope and plans keyed by a process id, so it is rejected at open and
/// recreated.
/// Bumped to 63 for FIG-2965: `processes` carries the cancel request as
/// `cancel_requested_at_ms` instead of a boolean, and gains the partial index a
/// pending-cancel list reads. A pre-63 database has the boolean column, so it
/// is rejected at open and recreated.
/// Bumped to 65 for FIG-2995's named process-definition registry: a single
/// new table, the one durable home for a registered definition record. A
/// pre-65 database holds no registry rows, so the whole catalog is recreated
/// under the reject-and-recreate policy rather than migrated midwifing a
/// registry into a database that never had one.
/// Bumped to 66 for FIG-3092's release stamp: `release_stamp` records the lash
/// release, the schema-version tuple and the instant that release first wrote
/// this store, so a host can read which build produced the data before wiring
/// a runtime. A pre-66 database has no such table and, under the
/// reject-and-recreate policy, is refused at open rather than midwifed one.
/// Bumped to 67 for FIG-2885: `session_meta` gains the two family CHECKs that
/// tie `relation_kind` and `caused_by_kind` to exactly their payload columns,
/// so a mispaired discriminator is refused at write rather than silently
/// dropped at decode. `caused_by_process_event_sequence` and
/// `caused_by_subscription_revision` stay TEXT on purpose: both carry a u64
/// `CausalRef` field whose full range exceeds SQLite's signed INTEGER, and the
/// cross-backend differential round-trips u64::MAX through them. A pre-67
/// database lacks the family guards, so it is rejected at open and recreated.
/// Bumped to 68 for FIG-3260: the await-event tables moved out of this string
/// into a shared fragment so the declaration existed once for both carrying
/// databases. The applied DDL is statement-identical, but the guarded
/// `SCHEMA` text changed, so a pre-68 database is rejected at open and
/// recreated like any other schema change.
/// Bumped to 69 for FIG-3261: every formerly-anonymous CHECK gained a
/// `ck_<table>_<concern>` name so the required-constraints gate can see it.
/// Constraint names change the stored DDL text, so a pre-69 database is
/// rejected at open and recreated.
/// Version 70 widens `ck_pending_turn_inputs_claim_identity_all_or_none` to
/// the whole four-column claim identity (FIG-3262): a claim id/token pair
/// with no owner was representable. A pre-70 database is rejected at open and
/// recreated.
/// Version 71 removes the redundant payload-family kind from stored blob
/// envelopes (FIG-1949 layer 2). The durable-core version guards these bytes
/// as well as the DDL. Pre-71 catalogs are rejected at open and recreated;
/// there is no envelope migration or legacy decode path.
/// Generation 72 cuts over to ordered plugin parts and the standard-compaction identity.
/// Pre-cutover durable-core catalogs are rejected and recreated.
/// Bumped to 74 for FIG-1949 layer 2: the stored artifact-blob envelope now
/// actually drops its `descriptor` field — the pointer table's namespace key
/// is the sole owner of the payload-family fact. A pre-74 database holds
/// envelopes that still carry the field, so it is rejected at open and
/// recreated rather than decoded under the new shape.
/// Version 75 adds durable queued-run admissions and normalized membership.
/// Durable-core 74 catalogs require recreation.
/// Bumped to 76 for FIG-3537: `runtime_turn_commits.result_json` now carries
/// RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION and every receipt read fails closed
/// on a missing, invalid, or unsupported version instead of skipping the
/// row. Pre-versioned receipts are refused; a pre-76 database is rejected at
/// open and recreated.
/// Bumped to 77 for FIG-3544: `pending_turn_inputs` gains the immutable
/// `submitted_ingress_json` and `submission_digest` columns written once at
/// admission, and source-key replay compares the digest instead of the row's
/// mutable current ingress. The digest is computed in Rust from the submitted
/// payload, so no DDL can backfill it; a pre-77 database is rejected at open
/// and recreated.
/// Bumped to 78 for FIG-3578: the catalog gains `attachment_blobs`, the bytes
/// of `SqliteAttachmentStore`, so a SQLite deployment supplies its attachment
/// port from the same database as the manifest that roots them. A pre-78
/// database has no such table and, under the reject-and-recreate policy, is
/// refused at open rather than midwifed one.
/// Bumped to 79 for FIG-3532: the durable `RuntimeErrorCode` and `TurnOutcome`
/// vocabularies queued-run terminals persist change (`turn_input_redrive_set_unavailable`
/// removed, `accepted_turn_input_ceded` and `TurnOutcome::Queued` added). A
/// pre-79 database is rejected at open and recreated.
/// Bumped to 80 for FIG-3589: `pending_turn_inputs` gains
/// `claim_bound_turn_id` and `claim_bound_receipt_input_id`, the aborted direct
/// turn a row's claim is bound to and the input its receipt names, with
/// `ck_pending_turn_inputs_bound_claim_is_next_turn` holding the pair
/// all-or-none and a binding to an open next-turn claim. A pre-80 database is rejected at open and
/// recreated.
/// Bumped to 81 for FIG-3586: the catalog gains `turn_parks`, the typed
/// parked state of an engine-executed turn that `drain_status` counts, and the
/// durable `RuntimeErrorCode` vocabulary gains
/// `lashlang_cell_replay_divergence`, `lashlang_cell_replay_key_format_cutover`
/// and `recorded_journal_read_unsupported`. A pre-81 database is rejected at
/// open and recreated.
/// Bumped to 82 for FIG-3598: the durable `RuntimeErrorCode` vocabulary gains
/// `restate_effect_group_protocol_retired`. No relation changes; a pre-82
/// database is rejected at open and recreated.
/// Bumped to 83 for FIG-3587: the durable `RuntimeErrorCode` vocabulary gains
/// `lashlang_cell_binding_drift`, the replay-mismatch report gains
/// `effect_kind`, and a `turn_parks` reason may be `binding_drift` or
/// `effect_replay_divergence`, which a pre-83 build cannot decode. A pre-83
/// database is rejected at open and recreated.
/// Bumped to 84: the durable `RuntimeErrorCode` vocabulary replaces
/// `worker_replacement_abort` with the engine-neutral, parking
/// `effect_replay_divergence`, and the retired code is not aliased. No
/// relation changes; a pre-84 database is rejected at open and recreated.
/// Bumped to 86 for FIG-3540 (S3): the catalog gains `session_ingress`, the
/// one session ingress of ADR 0101: one row per admitted item, one per-session
/// order under the database write lock, two class-level lanes. Its
/// `delivery_*` columns hold the submitted delivery, written once and never
/// rewritten, and `submission_digest` likewise; a claim's columns are set
/// exactly on an `accepted` row, and a tombstone carries its closed cause and
/// no claim. Partial indexes keep tombstones off the claim path. A pre-86
/// database has no such table and is rejected at open and recreated. The
/// number is provisional: the ingress store merges with the FIG-3540
/// cutover, which takes the next free version at its merge.
/// Bumped to 87 for FIG-3659: `turn_parks` reshapes into the enriched parked
/// record — `park_id`, `reason_code`, `since_ms`, `last_refused_ms` and
/// `attempts` — and the catalog gains `turn_park_clock`, the feed's sequence
/// row, and `turn_park_events`, the durable ledger of park transitions. A
/// pre-87 database is rejected at open and recreated.
/// Historical, retired in 60e0e86b2a: bumped to 88 for FIG-3585: the durable `RuntimeErrorCode` vocabulary drops
/// `runtime_perf_start_gate_retry` and `tool_completion_key_process_lifetime`,
/// and the durable core no longer carries the await-event tables that
/// store-delegated turn control used (the effect-replay database keeps its
/// own). A pre-88 database is rejected at open and recreated; it is not
/// migrated.
/// Bumped to 89 for FIG-3682: `session_meta` gains
/// `admission_base_checkpoint_ref`, the checkpoint of the head the session's
/// latest turn was admitted on. Maintenance keeps it as a checkpoint root, so
/// a replay of that turn can rebuild its input state from the head it was
/// admitted on after its own commit superseded the head. A pre-89 database is
/// rejected at open and recreated; it is not migrated.
/// Bumped to 90 for FIG-3735: a `turn_parks` reason may be
/// `session_state_generation_refused`, the park of an in-flight turn whose
/// redrive the session-state generation gate refused, which a pre-90 build
/// cannot decode. No relation changes; a pre-90 database is rejected at open
/// and recreated; it is not migrated.
/// Bumped to 91 for FIG-3632: `queued_work_batches.enqueue_seq`,
/// `pending_turn_inputs.enqueue_seq` and `usage_deltas.seq` are now
/// `INTEGER PRIMARY KEY AUTOINCREMENT`, so a delete can never let SQLite
/// reissue the freed maximum rowid the way `session_ingress` already could
/// not. A pre-91 database still declares the reusable rowid columns and is
/// rejected at open and recreated; it is not migrated.
/// Bumped to 92 for FIG-3667: the `postgres_effect_replay_*`,
/// `postgres_await_event_*` and `postgres_effect_journal_retirement` codes
/// leave the durable runtime-error vocabulary. No relation changes; a pre-92
/// database is rejected at open and recreated; it is not migrated.
/// Bumped to 93 for FIG-3571: a turn's admission records the executable
/// generation it runs under (the queued-run admission gained `generation`),
/// a redrive under another one parks with the `retired_generation` reason
/// (replacing `key_format_cutover`, and the durable `RuntimeErrorCode`
/// `lashlang_cell_replay_key_format_cutover` becomes `retired_generation`),
/// and `turn_parks` gains the projected, indexed `park_executable_generation` column the
/// drain counts retired parks by. A pre-93 database is rejected at open and
/// recreated; it is not migrated.
/// Bumped to 94 for FIG-3542: `session_head` gains `pending_follow_on_json`,
/// the follow-on turn a committed agent-frame switch owes the session (ADR
/// 0101 §3), and a frame handoff is no longer a queued-work row: the
/// `agent_frame_task` payload is gone and `runtime_turn_commits.result_json`
/// carries receipt schema 2. A pre-94 database is rejected at open and
/// recreated; it is not migrated.
/// Bumped to 95 for FIG-3796: the catalog gains `fleet_format`, the
/// deployment's own fleet-format row of ADR 0106 §1, recording the
/// durable-format generation every writer in the fleet emits. A pre-95
/// database has no such table and, under the reject-and-recreate policy, is
/// refused at open rather than midwifed one.
/// Bumped to 96 for FIG-3814: the `engine_effect_group_protocol_retired` code
/// leaves the durable runtime-error vocabulary for
/// `engine_object_state_format_unsupported`, with the effect-group protocol's
/// exact-version refusal. No relation changes; a pre-96 database is rejected at
/// open and recreated; it is not migrated.
/// Bumped to 97 for FIG-3815: `session_meta` gains `shift_run_start`, the
/// start marker of the execution of an admitted run that sealed the
/// session's current admission (ADR 0105 L-S8); a later seal of the same
/// admission by another execution is refused. A pre-97 database is rejected
/// at open and recreated; it is not migrated.
/// Bumped to 98 for FIG-3607: a process is named by its minted, never-reused
/// process id, so the legacy attachment ownership table drops `owner_incarnation` and
/// `session_meta_pending_observer_intents` drops `process_incarnation`, and
/// the durable `RuntimeErrorCode` vocabulary drops
/// `process_incarnation_superseded`. A pre-98 database is rejected at open
/// and recreated; it is not migrated.
/// Bumped to 99 for FIG-3600 S7: the logical-run family. `session_runs`
/// holds each admitted run and its terminal evidence, `session_run_inputs`
/// binds an accepted input to its run, `control_intents` records operator
/// verbs and session closes, `session_meta` gains `closing_intent`, a turn
/// park gains `engine_ref` and `resume_intent`, and a park event may be
/// `redrive_requested`. A pre-99 database is rejected at open and recreated;
/// it is not migrated.
/// Version 99 also lets a parked turn record the drain generation of the
/// build whose checkpoint it resumes (FIG-3795, changed in place under the
/// pre-1.0 version freeze, FIG-3846): `turn_parks` and `turn_park_events`
/// gain the projected `park_build_generation` column, and `turn_parks` the
/// partial index drain status counts it by. `session_runs` records each
/// run's admission (`admission_json`) and the drain generation of the shift
/// that admitted it (`admitted_generation`, indexed for the drain's in-flight
/// count per generation, FIG-3795 S9), with at most one unfinished run per
/// session; the queued-run ledger is gone and a queued-work head is admitted
/// as an ordinary run (FIG-3927). `session_runs` also records the executor
/// the seal of a run's admission named (`executor_json`, FIG-4814).
/// `pending_turn_inputs` and `queued_work_batches` retain the trace cause
/// their first acceptance was given (`trace_cause_json`, written once and
/// NULL for a root cause, FIG-4829). A database written before these changes
/// has the old shape; recreate it.
/// Version 99 also lets tool-intent submissions record process-definition
/// and trigger registration (FIG-4057, changed in place under the version
/// freeze): a catalog whose kind CHECK predates them rejects both kinds, so
/// recreate it.
/// Version 99 also holds the process registry and the trigger store
/// (FIG-5195, changed in place under the version freeze): their tables, once
/// two databases of their own beside this one, are provisioned in the one
/// database file, so a producer's transaction spans every table it writes. A
/// store in the three-file layout is refused as
/// [`CompatRefusal::RetiredSqliteLayout`]; recreate it.
/// version_guard(
///     shapes(
///         path = "crates/lash-core-execution/src/runtime/effect/envelope.rs",
///         cover(
///             RuntimeEffectInvocation, RuntimeEffectEnvelope, RuntimeEffectCommand,
///             RuntimeEffectOutcome,
///         ),
///     ),
///     roots(
///         path = "crates/lash-sqlite-store/src/lib.rs", StoredBlobEnvelope,
///         BlobArtifactDescriptor, BlobStorageHint, BlobCompression,
///     ),
///     roots(path = "crates/lash-sansio/src/session_model/mod.rs", TurnOutcome, ErrorEnvelope),
///     items(
///         path = "crates/lash-sqlite-store/src/schema.rs", SCHEMA, PROCESS_SCHEMA,
///         elide = "sql_idempotent_index",
///     ),
///     items(
///         path = "crates/lash-sqlite-store/src/trigger_schema.rs", TRIGGER_SCHEMA,
///         elide = "sql_idempotent_index",
///     ),
///     items(
///         path = "crates/lash-sqlite-store/src/schema_fragments.rs", SESSION_INGRESS_TABLE,
///         SESSION_RUNS_TABLES, elide = "sql_idempotent_index",
///     ),
///     catalog(path = "crates/lash-sqlite-store/src/migration.rs", CATALOG),
/// )
/// version_surface = "migrate"
/// format_outside_manifest = "store schema version: declared in compat.rs for the component's descriptor and read from the deployment through StorePreflight::schema_status, not reported in the durable-format manifest"
/// version_unguarded = "store schema version: a catalog step moves it, and admission reads it through the component's compat descriptor at open (ADR 0115 §1.3), never through a record decoder"
pub const SQLITE_CORE_SCHEMA_VERSION: u32 = 99;

/// What this build declares about a store component whose provisioning DDL
/// is at `version`: it reads and writes exactly that version.
#[cfg(not(feature = "synthetic-next"))]
const fn store(component: ComponentId, version: u32) -> CompatDescriptor {
    CompatDescriptor {
        component,
        reads: VersionRange::exactly(version),
        writes: VersionRange::exactly(version),
    }
}

/// Phase A's synthetic N+1 (ADR 0115 §6) expands every store component one
/// step past its provisioning DDL and keeps reading the version before it.
#[cfg(feature = "synthetic-next")]
const fn store(component: ComponentId, version: u32) -> CompatDescriptor {
    CompatDescriptor {
        component,
        reads: VersionRange::between(version, version + 1),
        writes: VersionRange::exactly(version + 1),
    }
}

/// Every component this build declares. `lashctl version --json` prints them.
///
/// The versions are the components' compatibility numbers, the `version` a
/// stamp records. A store component's is its schema-version constant above;
/// the 1.0 cut resets each of those to 1.
pub const DESCRIPTORS: &[CompatDescriptor] = &[
    store(ComponentId::POSTGRES, POSTGRES_SCHEMA_VERSION),
    store(ComponentId::SQLITE_CORE, SQLITE_CORE_SCHEMA_VERSION),
    CompatDescriptor {
        component: ComponentId::RESTATE_DURABLE_WAIT_REGISTRY,
        reads: RESTATE_OBJECT_FAMILY_FORMATS,
        writes: RESTATE_OBJECT_FAMILY_FORMATS,
    },
];

/// The formats this build reads and writes for every Restate object family.
#[cfg(not(feature = "synthetic-next"))]
const RESTATE_OBJECT_FAMILY_FORMATS: VersionRange = VersionRange::exactly(1);

/// Phase A's synthetic N+1 (ADR 0115 §6) moves every Restate object family
/// to format 2 and keeps reading and writing format 1.
#[cfg(feature = "synthetic-next")]
const RESTATE_OBJECT_FAMILY_FORMATS: VersionRange = VersionRange::between(1, 2);

/// The release line this build writes into every Restate `_compat` record
/// and call envelope: 1 from the 1.0 release on.
///
/// The 1.0 cut restarts every format counter at 1, so a number a pre-release
/// build wrote and the same number from a release build name different
/// shapes. The line tells them apart: state and calls that state no line, or
/// line 0, are pre-release and refused as [`CompatRefusal::PreRelease`]
/// before anything is decoded. Counters never restart again, so the line
/// stays 1.
pub const RELEASE_LINE: u32 = 1;

/// The component a Restate call envelope's refusal names: the wire is no
/// stored component and declares no descriptor.
pub const RESTATE_WIRE_COMPONENT: &str = "restate-wire";

/// The descriptor this build declares for `component`.
pub fn descriptor(component: ComponentId) -> Option<&'static CompatDescriptor> {
    DESCRIPTORS
        .iter()
        .find(|descriptor| descriptor.component == component)
}

/// A durable stamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CompatStamp {
    pub version: u32,
    pub min_reader: u32,
}

/// What the store said about a component's stamp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StampRead {
    /// No stamp. `populated` says whether the store holds any lash objects.
    Absent { populated: bool },
    /// The stamp as stored, not yet checked.
    Present(CompatStamp),
    /// A stamp is there but did not decode. Carries the backend's words.
    Unreadable(String),
}

/// How a store is admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompatAdmission {
    /// The store is empty. The component's installer provisions it and
    /// writes the stamp; nothing opens it before that. A stamp is never
    /// defaulted to the current version.
    Provision,
    /// The stamp is inside `reads`.
    Native,
    /// A newer release expanded the component, and its floor still admits
    /// this build. The shape check runs in tolerant mode (§1.4).
    Expanded { version: u32 },
}

/// Why a store, an epoch or a stored label is refused.
///
/// Each message names its remedy with a `lashctl` command. The JSON shape is
/// what `lashctl --json` reports and what a newer build reads from an older
/// one, so a variant's fields change only in place under the version freeze.
#[derive(
    Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "refusal", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CompatRefusal {
    #[error(
        "{component} holds lash data but carries no compatibility stamp; a stamp is never \
         assumed, so the store is refused unchanged. Restore it from a backup or recreate it; \
         `lashctl preflight` reports what it found{}",
        release_suffix(.writing_release)
    )]
    Unstamped {
        component: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writing_release: Option<String>,
    },
    #[error(
        "{component} compatibility stamp is malformed ({detail}); the store is refused \
         unchanged. Restore it from a backup; `lashctl preflight` reports the stamp{}",
        release_suffix(.writing_release)
    )]
    MalformedStamp {
        component: String,
        detail: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writing_release: Option<String>,
    },
    #[error(
        "{component} is at version {found}, older than this build reads ({reads}): an older or \
         skipped release wrote it. Run `lashctl migrate` from the intermediate release first, \
         then from this build{}",
        release_suffix(.writing_release)
    )]
    TooOld {
        component: String,
        found: u32,
        reads: VersionRange,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writing_release: Option<String>,
    },
    /// A SQLite database this build reads but has not migrated: its stamp is
    /// older than the version this build writes. The store's open migrates
    /// every database together, after a complete backup; a component opened
    /// on its own never migrates.
    #[error(
        "{component} is at version {found}, older than the version {target} this build writes: \
         it has not been migrated. Open the whole store with this build (`SqliteStoreSet::open`), \
         which backs up every database and then migrates them together{}",
        release_suffix(.writing_release)
    )]
    MigrationPending {
        component: String,
        found: u32,
        target: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writing_release: Option<String>,
    },
    #[error(
        "{component} is at version {found} with reader floor {min_reader}, above the newest \
         this build reads ({reads}): a newer release contracted it. Run a build whose range \
         reaches {min_reader}; `lashctl version` prints a build's ranges{}",
        release_suffix(.writing_release)
    )]
    ReaderFloorAbove {
        component: String,
        found: u32,
        min_reader: u32,
        reads: VersionRange,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writing_release: Option<String>,
    },
    /// A Restate object's `_compat` writer floor is above the newest family
    /// format this build writes (ADR 0115 §3.2): a newer release upgraded
    /// the object, and this build may still read it but never mutate it.
    #[error(
        "{component} is at format {found} with writer floor {min_writer}, above the newest \
         this build writes ({writes}): a newer release upgraded it. Run a build whose range \
         reaches {min_writer}; `lashctl version` prints a build's ranges"
    )]
    WriterFloorAbove {
        component: String,
        found: u32,
        min_writer: u32,
        writes: VersionRange,
    },
    #[error(
        "{component} carries additions this build cannot write beside: {}. Run the release \
         that expanded it; `lashctl preflight` lists them{}",
        .findings.join("; "),
        release_suffix(.writing_release)
    )]
    ShapeRefused {
        component: String,
        findings: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writing_release: Option<String>,
    },
    /// `F` at open: below the writable range (a skipped release) or above it
    /// (a newer fleet).
    #[error(
        "store records fleet epoch {recorded}, outside this build's writable range {writable}: \
         below it a release was skipped, above it the fleet is newer. Run a build whose \
         writable range contains {recorded}; `lashctl version` prints a build's range{}",
        release_suffix(.writing_release)
    )]
    FleetOutsideWritable {
        recorded: u32,
        writable: VersionRange,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writing_release: Option<String>,
    },
    /// `F` at open: the store records no fleet epoch. The installer seeds it
    /// and an open never does, so no build decides `F` by opening first.
    #[error(
        "{component} records no fleet epoch: `lashctl migrate` seeds it when it provisions or \
         advances the store, and an open never records one. The store is refused unchanged; \
         run `lashctl migrate`, then open again{}",
        release_suffix(.writing_release)
    )]
    FleetUnrecorded {
        component: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writing_release: Option<String>,
    },
    /// A SQLite store's configured path is a directory in the retired layout
    /// of three database files (FIG-5195). A SQLite deployment is one
    /// database file (ADR 0132 §12), and formats reset at 1.0, so no release
    /// migrates the old layout: the open refuses it unchanged.
    #[error(
        "{location} is a directory in the retired SQLite layout of three database files ({}): \
         a SQLite deployment is one database file, and no release migrates the old layout. \
         The directory is left unchanged; configure the path of a database file and recreate \
         the store there",
        .files.join(", ")
    )]
    RetiredSqliteLayout {
        location: String,
        /// The retired layout's database files the directory holds.
        files: Vec<String>,
    },
    /// State or a call a pre-release build wrote (FIG-4819). The 1.0 cut
    /// restarted every counter, so its numbers do not mean what this
    /// build's do, and no release reads it: a Restate object or call that
    /// states no release line, or a store whose floor is above this build's
    /// range although an older release stamped it.
    #[error(
        "{component} holds pre-release state: a lash build from before the 1.0 release wrote \
         it, and 1.0 restarted every format counter, so its numbers do not mean what this \
         build's do. No release reads pre-release state and none migrates it; it is refused \
         unchanged. Recreate the store, and serve this build from a Restate namespace no \
         pre-release build has used{}",
        release_suffix(.writing_release)
    )]
    PreRelease {
        component: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        writing_release: Option<String>,
    },
    /// A stored label this build has no name for (an obligation state or
    /// kind, an attachment owner kind, a referrer kind): a newer build wrote
    /// it. Classified apart from corruption so no pass treats it as absent.
    #[error(
        "stored {surface} label `{label}` is unknown to this build: a newer build wrote it. The \
         row is kept unchanged; run a build that knows it (`lashctl version` prints a build's \
         ranges)"
    )]
    UnknownVocabulary { surface: String, label: String },
    /// A plugin namespace is stamped with a format the fleet record does not
    /// permit its plugin to write (FIG-4746). Nothing was published.
    #[error(
        "plugin `{plugin}` {namespace:?} format {writer} is outside the writer range {permitted} \
         the fleet record permits: before finalize a build writes only what the fleet already \
         reads. Nothing was published; run a build that writes a permitted format, or finalize \
         the release that introduced format {writer} with `lashctl finalize`"
    )]
    PluginWriterOutsideRange {
        plugin: String,
        namespace: crate::plugin_state::FormatNamespace,
        writer: u32,
        permitted: VersionRange,
    },
    /// The fleet record carries no writer range for a plugin (FIG-4746): a
    /// plugin the record does not name publishes only its first format.
    #[error(
        "the fleet record carries no writer range for plugin `{plugin}`, so only its first \
         format may be published. Nothing was published; provision the plugin's range from its \
         registration before it writes"
    )]
    PluginWriterUnprovisioned { plugin: String },
    /// A recorded plugin writer range is not a range (FIG-4746). The store
    /// fails closed: no publication of that store is admitted.
    #[error(
        "the fleet record's writer range for plugin `{plugin}` is malformed ({detail}); the \
         store is refused unchanged. Restore it from a backup"
    )]
    PluginWriterRangeMalformed { plugin: String, detail: String },
    /// A plugin writes no format the fleet record permits it (FIG-4747): an
    /// admission chooses each plugin's writer inside its range, and this
    /// plugin has none there. Nothing was admitted.
    #[error(
        "plugin `{plugin}` writes formats {writable:?}, none of which is inside the writer range \
         {permitted} the fleet record permits, so it cannot run here. Nothing was admitted; run \
         a build whose plugin writes a permitted format, or finalize the release that \
         introduced its formats with `lashctl finalize`"
    )]
    PluginWriterUnwritable {
        plugin: String,
        writable: Vec<u32>,
        permitted: VersionRange,
    },
}

impl CompatRefusal {
    /// Attach the release that last wrote the store when its stamp is readable.
    /// The refusal's reason and JSON tag remain unchanged.
    pub fn with_writing_release(mut self, release: Option<String>) -> Self {
        match &mut self {
            Self::Unstamped {
                writing_release, ..
            }
            | Self::MalformedStamp {
                writing_release, ..
            }
            | Self::TooOld {
                writing_release, ..
            }
            | Self::MigrationPending {
                writing_release, ..
            }
            | Self::ReaderFloorAbove {
                writing_release, ..
            }
            | Self::ShapeRefused {
                writing_release, ..
            }
            | Self::FleetOutsideWritable {
                writing_release, ..
            }
            | Self::FleetUnrecorded {
                writing_release, ..
            }
            | Self::PreRelease {
                writing_release, ..
            } => *writing_release = release,
            Self::WriterFloorAbove { .. }
            | Self::RetiredSqliteLayout { .. }
            | Self::UnknownVocabulary { .. }
            | Self::PluginWriterOutsideRange { .. }
            | Self::PluginWriterUnprovisioned { .. }
            | Self::PluginWriterRangeMalformed { .. }
            | Self::PluginWriterUnwritable { .. } => {}
        }
        self
    }

    /// This refusal as the store's release evidence corrects it.
    ///
    /// Within one release line counters only grow, so a floor above this
    /// build's range means a newer release contracted the store. A floor
    /// above the range on a store that a release older than `build_release`
    /// stamped can only predate the 1.0 counter restart: the refusal is
    /// [`Self::PreRelease`], not "newer". The store is refused either way;
    /// the stamp is evidence for the reason, never an admission input (ADR
    /// 0115 §1.2). A store with no readable stamp, or one this build cannot
    /// order against its own release, keeps the floor refusal.
    pub fn read_against_release(self, writing_release: Option<&str>, build_release: &str) -> Self {
        let older = writing_release.is_some_and(|writing| {
            crate::store::compare_releases(writing, build_release) == Some(std::cmp::Ordering::Less)
        });
        match self {
            Self::ReaderFloorAbove {
                component,
                writing_release: attached,
                ..
            } if older => Self::PreRelease {
                component,
                writing_release: attached,
            },
            other => other,
        }
    }
}

fn release_suffix(release: &Option<String>) -> String {
    release.as_deref().map_or_else(String::new, |release| {
        format!(". Writing release: {release}")
    })
}

/// The admission rule of §1.3, answered in order: absent, malformed, too old,
/// floor passed, admitted.
pub fn admit(
    descriptor: &CompatDescriptor,
    stamp: StampRead,
) -> Result<CompatAdmission, CompatRefusal> {
    let component = || descriptor.component.as_str().to_owned();
    let stamp = match stamp {
        StampRead::Absent { populated: false } => return Ok(CompatAdmission::Provision),
        StampRead::Absent { populated: true } => {
            return Err(CompatRefusal::Unstamped {
                component: component(),
                writing_release: None,
            });
        }
        StampRead::Unreadable(detail) => {
            return Err(CompatRefusal::MalformedStamp {
                component: component(),
                detail,
                writing_release: None,
            });
        }
        StampRead::Present(stamp) => stamp,
    };
    if stamp.min_reader == 0 || stamp.min_reader > stamp.version {
        return Err(CompatRefusal::MalformedStamp {
            component: component(),
            detail: format!(
                "reader floor {} outside [1, version {}]",
                stamp.min_reader, stamp.version
            ),
            writing_release: None,
        });
    }
    let reads = descriptor.reads;
    if stamp.version < reads.min() {
        return Err(CompatRefusal::TooOld {
            component: component(),
            found: stamp.version,
            reads,
            writing_release: None,
        });
    }
    if stamp.min_reader > reads.max() {
        return Err(CompatRefusal::ReaderFloorAbove {
            component: component(),
            found: stamp.version,
            min_reader: stamp.min_reader,
            reads,
            writing_release: None,
        });
    }
    if stamp.version <= reads.max() {
        Ok(CompatAdmission::Native)
    } else {
        Ok(CompatAdmission::Expanded {
            version: stamp.version,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CompatAdmission, CompatDescriptor, CompatRefusal, CompatStamp, ComponentId, StampRead,
        VersionRange, admit,
    };

    fn stamp(version: u32, min_reader: u32) -> StampRead {
        StampRead::Present(CompatStamp {
            version,
            min_reader,
        })
    }

    #[test]
    fn admit_truth_table() {
        // A build that reads [2,3] of a component.
        let descriptor = CompatDescriptor {
            component: ComponentId::POSTGRES,
            reads: VersionRange::new(2, 3).expect("range"),
            writes: VersionRange::new(2, 3).expect("range"),
        };
        let reads = descriptor.reads;
        let component = "postgres".to_owned();

        // 1. Absent: an empty store is provisioned, a populated one refused.
        assert_eq!(
            admit(&descriptor, StampRead::Absent { populated: false }),
            Ok(CompatAdmission::Provision)
        );
        assert_eq!(
            admit(&descriptor, StampRead::Absent { populated: true }),
            Err(CompatRefusal::Unstamped {
                component: component.clone(),
                writing_release: None,
            })
        );

        // 2. Unreadable or malformed, checked before any range.
        assert_eq!(
            admit(&descriptor, StampRead::Unreadable("not an integer".into())),
            Err(CompatRefusal::MalformedStamp {
                component: component.clone(),
                detail: "not an integer".into(),
                writing_release: None,
            })
        );
        for (version, min_reader) in [(3, 0), (2, 3), (9, 0), (1, 5), (0, 0)] {
            assert!(
                matches!(
                    admit(&descriptor, stamp(version, min_reader)),
                    Err(CompatRefusal::MalformedStamp { .. })
                ),
                "{{version: {version}, min_reader: {min_reader}}} is malformed"
            );
        }

        // 3. Too old: an older or skipped release wrote it.
        assert_eq!(
            admit(&descriptor, stamp(1, 1)),
            Err(CompatRefusal::TooOld {
                component: component.clone(),
                found: 1,
                reads,
                writing_release: None,
            })
        );

        // 4. Floor passed: a newer release contracted past this build.
        assert_eq!(
            admit(&descriptor, stamp(5, 4)),
            Err(CompatRefusal::ReaderFloorAbove {
                component: component.clone(),
                found: 5,
                min_reader: 4,
                reads,
                writing_release: None,
            })
        );

        // 5. Admitted: native inside `reads`, expanded above it under the floor.
        assert_eq!(admit(&descriptor, stamp(2, 1)), Ok(CompatAdmission::Native));
        assert_eq!(admit(&descriptor, stamp(3, 3)), Ok(CompatAdmission::Native));
        assert_eq!(
            admit(&descriptor, stamp(4, 3)),
            Ok(CompatAdmission::Expanded { version: 4 })
        );
        assert_eq!(
            admit(&descriptor, stamp(7, 2)),
            Ok(CompatAdmission::Expanded { version: 7 })
        );
    }

    /// FIG-4819: a floor above this build's range on a store an older
    /// release stamped predates the counter restart. The same floor under
    /// this release, a newer one, or no readable stamp stays "newer".
    #[test]
    fn a_floor_refusal_of_an_older_releases_store_reads_as_pre_release() {
        let floor = || CompatRefusal::ReaderFloorAbove {
            component: "sqlite-core".into(),
            found: 99,
            min_reader: 99,
            reads: VersionRange::exactly(1),
            writing_release: None,
        };
        for older in ["0.0.0-dev", "0.9.3", "0.0.0-alpha"] {
            assert_eq!(
                floor().read_against_release(Some(older), "1.0.0"),
                CompatRefusal::PreRelease {
                    component: "sqlite-core".into(),
                    writing_release: None,
                },
                "{older}"
            );
        }
        for (writing, build) in [
            (Some("1.1.0"), "1.0.0"),
            (Some("1.0.0"), "1.0.0"),
            (Some("0.0.0-dev"), "0.0.0-dev"),
            (Some("not-a-version"), "1.0.0"),
            (None, "1.0.0"),
        ] {
            assert_eq!(
                floor().read_against_release(writing, build),
                floor(),
                "{writing:?} under {build}"
            );
        }
        // Only the floor refusal claims "newer"; no other reason changes.
        let too_old = CompatRefusal::TooOld {
            component: "sqlite-core".into(),
            found: 1,
            reads: VersionRange::exactly(2),
            writing_release: None,
        };
        assert_eq!(
            too_old.clone().read_against_release(Some("0.9.0"), "1.0.0"),
            too_old
        );
        let refusal = CompatRefusal::PreRelease {
            component: "restate-wire".into(),
            writing_release: None,
        };
        assert_eq!(
            serde_json::to_string(&refusal).expect("encode"),
            r#"{"refusal":"pre_release","component":"restate-wire"}"#
        );
    }
}
