use crate::*;

/// The DDL this build provisions, committed verbatim as the crate's
/// `schema.sql` artifact so a host can vendor the exact bytes lash executes.
pub(crate) const SCHEMA_DDL: &str = include_str!("../../schema.sql");

/// Advisory-lock key lash takes for the duration of a schema-provisioning or
/// schema-verifying transaction. See
/// [`crate::PostgresStorage::schema_advisory_lock_key`].
pub(crate) const SCHEMA_ADVISORY_LOCK_KEY: (i32, i32) = (715421, 907001);

struct SchemaMigration {
    from: i32,
    to: i32,
    source_missing_tables: &'static [&'static str],
    /// `(table, column)` pairs this build adds to a table the source already
    /// has. Additive and nullable only: PostgreSQL records such a column in
    /// catalog metadata and rewrites no row, which keeps a column-adding
    /// migration in the same creation-only class as a table-adding one.
    source_missing_columns: &'static [(&'static str, &'static str)],
    /// Uniqueness guards this build adds to a table the source already has.
    /// Adding one can fail against data the guard rejects, which is exactly why
    /// it is declared: the migration transaction aborts and the operator sees
    /// the conflict rather than a half-migrated schema.
    source_missing_guards: &'static [DeclaredGuard],
    introduced_relations: &'static [&'static str],
    statements: &'static [&'static str],
}

/// One uniqueness guard an explicit migration adds, declared precisely enough
/// that tolerating its absence tolerates *only* it.
///
/// Table and key columns alone are not an identity: a `PRIMARY KEY`, a full
/// `UNIQUE`, and a partial `UNIQUE` over the same columns guard different row
/// sets, and a declaration matching on columns alone would wave a missing
/// primary key through the creation-only door on the strength of a partial
/// index this build happens to add. The predicate is carried verbatim in the
/// shape checker's normalized form, and `primary_key` and `nulls_not_distinct`
/// are required to be false rather than declared: this build adds neither by
/// migration, and a future one that needs to must say so here first.
struct DeclaredGuard {
    table: &'static str,
    /// Key columns. The *set* is the identity, not the declared order, the same
    /// way the shape checker pairs guards: column order changes which index
    /// prefixes can be scanned, not which rows the guard rejects.
    columns: &'static [&'static str],
    /// The guard's partial-index predicate in [`UniqueGuard`]'s normalized form:
    /// lower-cased, whitespace collapsed, outer parentheses stripped.
    predicate: &'static str,
}

const ATTACHMENT_CONDEMNATIONS_DDL: &str = r#"CREATE TABLE lash_attachment_condemnations (
            attachment_id TEXT PRIMARY KEY,
            phase TEXT NOT NULL CHECK (phase IN ('condemned', 'deleting'))
        )"#;

const CHECKPOINT_BLOB_REFS_DDL: &str = r#"CREATE TABLE lash_checkpoint_blob_refs (
            checkpoint_ref TEXT NOT NULL REFERENCES lash_blobs(hash) ON DELETE CASCADE,
            blob_ref TEXT NOT NULL REFERENCES lash_blobs(hash) ON DELETE CASCADE,
            PRIMARY KEY (checkpoint_ref, blob_ref)
        )"#;

const CHECKPOINT_BLOB_REFS_REVERSE_INDEX_DDL: &str = r#"CREATE INDEX idx_lash_checkpoint_blob_refs_blob_ref
            ON lash_checkpoint_blob_refs(blob_ref, checkpoint_ref)"#;

const SESSIONS_CHECKPOINT_REF_INDEX_DDL: &str = r#"CREATE INDEX idx_lash_sessions_checkpoint_ref
            ON lash_sessions(checkpoint_ref)"#;

const NODE_ANCHORS_CHECKPOINT_REF_INDEX_DDL: &str = r#"CREATE INDEX idx_lash_node_anchors_checkpoint_ref
            ON lash_node_anchors(checkpoint_ref)"#;

const PROCESS_PARENT_END_PLANS_DDL: &str = r#"CREATE TABLE lash_process_parent_end_plans (
            process_id TEXT PRIMARY KEY REFERENCES lash_processes(process_id) ON DELETE CASCADE,
            actions_json TEXT NOT NULL
        )"#;

const TOOL_INTENT_SUBMISSIONS_DDL: &str = r#"CREATE TABLE lash_tool_intent_submissions (
            replay_key TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            execution_scope_id TEXT NOT NULL,
            tool_call_id TEXT NOT NULL,
            intent_index BIGINT NOT NULL,
            kind TEXT NOT NULL,
            payload_hash TEXT NOT NULL,
            submission_json TEXT NOT NULL
        )"#;

const TOOL_INTENT_SUBMISSIONS_INDEX_DDL: &str = r#"CREATE INDEX idx_lash_tool_intent_submissions_scope
            ON lash_tool_intent_submissions(session_id, execution_scope_id, intent_index)"#;

/// The two ordering indexes that back idle ingress-family arbitration.
///
/// Both cover columns component 52 already stores, so introducing them is
/// creation-only: no column is added and no row is rewritten. This is why the
/// 53 generation is reachable by migration at all.
///
/// `IF NOT EXISTS` makes each statement idempotent on its own, so a migration
/// that is retried after failing later in its transaction cannot die on a
/// duplicate-object error. The divergence probe over `introduced_relations` still
/// refuses a stamp that already carries either index before any DDL runs; this
/// is the second line, not a replacement for it.
const QUEUED_WORK_SESSION_COMMAND_ORDER_INDEX_DDL: &str = r#"CREATE INDEX IF NOT EXISTS idx_lash_queued_work_session_command_order
            ON lash_queued_work_batches(session_id, work_kind, enqueued_at_ms, enqueue_seq)"#;

const PENDING_TURN_INPUT_ORDER_INDEX_DDL: &str = r#"CREATE INDEX IF NOT EXISTS idx_lash_pending_turn_input_order
            ON lash_pending_turn_inputs(session_id, state, enqueued_at_ms, enqueue_seq)"#;

/// The durable effect-group journal, added by the 54 generation.
///
/// The group row is the settlement-sequence allocator: a finalizing child bumps
/// `next_seq` inside its own fenced transaction, which is the only allocation
/// that cannot lose an update the way `MAX(settlement_seq) + 1` can.
const RUNTIME_EFFECT_GROUP_DDL: &str = r#"CREATE TABLE lash_runtime_effect_group (
            group_key TEXT PRIMARY KEY,
            scope_id TEXT NOT NULL,
            session_id TEXT,
            wake TEXT NOT NULL,
            loser_disposition TEXT NOT NULL,
            children BIGINT NOT NULL,
            next_seq BIGINT NOT NULL DEFAULT 0,
            created_at_ms BIGINT NOT NULL
        )"#;

const RUNTIME_EFFECT_GROUP_SESSION_INDEX_DDL: &str = r#"CREATE INDEX IF NOT EXISTS idx_lash_runtime_effect_group_session
            ON lash_runtime_effect_group(session_id)"#;

const RUNTIME_EFFECT_GROUP_SCOPE_INDEX_DDL: &str = r#"CREATE INDEX IF NOT EXISTS idx_lash_runtime_effect_group_scope
            ON lash_runtime_effect_group(scope_id)"#;

/// Both columns are nullable with no default, so PostgreSQL adds them as catalog
/// metadata: every already-journalled effect row survives the upgrade with its
/// recorded `envelope_hash` — and therefore its lease fence — untouched.
const RUNTIME_EFFECT_REPLAY_GROUP_KEY_DDL: &str =
    r#"ALTER TABLE lash_runtime_effect_replay ADD COLUMN IF NOT EXISTS group_key TEXT"#;

const RUNTIME_EFFECT_REPLAY_SETTLEMENT_SEQ_DDL: &str =
    r#"ALTER TABLE lash_runtime_effect_replay ADD COLUMN IF NOT EXISTS settlement_seq BIGINT"#;

const RUNTIME_EFFECT_REPLAY_GROUP_SEQ_INDEX_DDL: &str = r#"CREATE UNIQUE INDEX IF NOT EXISTS uq_lash_runtime_effect_replay_group_seq
            ON lash_runtime_effect_replay(group_key, settlement_seq)
            WHERE group_key IS NOT NULL AND settlement_seq IS NOT NULL"#;

/// The drain's queue read, indexed (FIG-1536).
///
/// `read_unsettled_group_children` asks for one group's children that hold no
/// settlement rank. Without this the planner reaches that answer through the
/// whole effect-replay table, which on a busy store is every effect any session
/// ever journaled — a scan whose cost grows with history the drain has no
/// interest in. The predicate keeps the index to exactly the rows a drain can
/// act on: a settled child leaves it on finalize, so the index shrinks as a
/// group drains and holds nothing at all for a fully settled one.
///
/// # Why this is a migration here and was not one on SQLite
///
/// The equivalent SQLite index shipped in the same generation as the columns it
/// covers, with no version bump, because that tier's schema is
/// reject-and-recreate: its guarded projection admits an idempotent index
/// addition under the `sql_idempotent_index` carve-out precisely so that adding
/// one does not force a bump that would delete every existing effect database.
/// PostgreSQL has migrations, so it takes the routine path — a new generation
/// with a creation-only migration into it — and the asymmetry is a property of
/// what each tier can do about an old database, not a disagreement about what
/// the index is for.
const RUNTIME_EFFECT_REPLAY_GROUP_UNSETTLED_INDEX_DDL: &str = r#"CREATE INDEX IF NOT EXISTS idx_lash_runtime_effect_replay_group_unsettled
            ON lash_runtime_effect_replay(group_key, replay_key)
            WHERE group_key IS NOT NULL AND settlement_seq IS NULL"#;

const TRIGGER_OCCURRENCE_RECLAIMABLE_AT_DDL: &str = r#"ALTER TABLE lash_trigger_occurrences
            ADD COLUMN IF NOT EXISTS reclaimable_at_ms BIGINT"#;

const TRIGGER_OCCURRENCE_RECLAIMABLE_ARM_DDL: &str = r#"UPDATE lash_trigger_occurrences AS occurrence
            SET reclaimable_at_ms = occurrence.occurred_at_ms
            WHERE occurrence.reclaimable_at_ms IS NULL
              AND NOT EXISTS (
                  SELECT 1 FROM lash_trigger_deliveries AS delivery
                  WHERE delivery.occurrence_id = occurrence.occurrence_id
              )"#;

const TRIGGER_OCCURRENCE_RECLAIMABLE_INDEX_DDL: &str = r#"CREATE INDEX IF NOT EXISTS idx_lash_trigger_occurrences_reclaimable
            ON lash_trigger_occurrences(reclaimable_at_ms, occurrence_id)
            WHERE reclaimable_at_ms IS NOT NULL"#;

const EFFECT_GROUP_GUARDS: &[DeclaredGuard] = &[DeclaredGuard {
    table: "lash_runtime_effect_replay",
    columns: &["group_key", "settlement_seq"],
    predicate: "(group_key is not null) and (settlement_seq is not null)",
}];

/// Explicit, creation-only migrations into the current component generation.
///
/// The version-bump recreation harness
/// (`runbooks/restate-postgres-workers/src/bin/version_bump.rs`) pins its
/// fixtures to this table's newest generation: `MIGRATION_FLOOR_VERSION` (the
/// oldest `from` below), `POST_FLOOR_TABLES` / `POST_FLOOR_ARTIFACTS` (the floor
/// migration's `source_missing_tables` / `introduced_relations`, dropped to
/// rebuild the published floor catalog), `POST_FLOOR_INDEXES` (the subset of
/// those artifacts that dropping the post-floor tables does not take with them),
/// and `DIVERGENT_ARTIFACTS` (the predecessor migration's
/// `introduced_relations`, which the divergence refusal must enumerate). A bump
/// that introduces a relation moves all of them.
///
/// `scripts/check_version_bump_fixtures.py` recomputes each of those from this
/// table and fails the build when they drift, so the drift is a local check
/// rather than a container-gate surprise.
const SCHEMA_MIGRATIONS: &[SchemaMigration] = &[
    // Component 57 adds the indexed manifest -> component edge projection used
    // by session-owner blob reclaim. The two root indexes make every liveness
    // arm an indexed NOT EXISTS predicate.
    SchemaMigration {
        from: 56,
        to: 57,
        source_missing_tables: &["lash_checkpoint_blob_refs"],
        source_missing_columns: &[],
        source_missing_guards: &[],
        introduced_relations: &[
            "lash_checkpoint_blob_refs",
            "idx_lash_checkpoint_blob_refs_blob_ref",
            "idx_lash_sessions_checkpoint_ref",
            "idx_lash_node_anchors_checkpoint_ref",
        ],
        statements: &[
            CHECKPOINT_BLOB_REFS_DDL,
            CHECKPOINT_BLOB_REFS_REVERSE_INDEX_DDL,
            SESSIONS_CHECKPOINT_REF_INDEX_DDL,
            NODE_ANCHORS_CHECKPOINT_REF_INDEX_DDL,
        ],
    },
    // A component-55 store takes both later generations at once: trigger
    // occurrence reclaim eligibility from 56 and checkpoint edges from 57.
    SchemaMigration {
        from: 55,
        to: 57,
        source_missing_tables: &["lash_checkpoint_blob_refs"],
        source_missing_columns: &[("lash_trigger_occurrences", "reclaimable_at_ms")],
        source_missing_guards: &[],
        introduced_relations: &[
            "lash_checkpoint_blob_refs",
            "idx_lash_checkpoint_blob_refs_blob_ref",
            "idx_lash_sessions_checkpoint_ref",
            "idx_lash_node_anchors_checkpoint_ref",
            "idx_lash_trigger_occurrences_reclaimable",
        ],
        statements: &[
            CHECKPOINT_BLOB_REFS_DDL,
            CHECKPOINT_BLOB_REFS_REVERSE_INDEX_DDL,
            SESSIONS_CHECKPOINT_REF_INDEX_DDL,
            NODE_ANCHORS_CHECKPOINT_REF_INDEX_DDL,
            TRIGGER_OCCURRENCE_RECLAIMABLE_AT_DDL,
            TRIGGER_OCCURRENCE_RECLAIMABLE_ARM_DDL,
            TRIGGER_OCCURRENCE_RECLAIMABLE_INDEX_DDL,
        ],
    },
    // The 55 generation adds one index and nothing else: the drain's
    // unsettled-children read, which layer 2.5 deferred (FIG-1564) and the
    // drain (FIG-1536) makes a hot path. No table, no column, no guard — so the
    // source shape a 54 store must present is the current one.
    SchemaMigration {
        from: 54,
        to: 57,
        source_missing_tables: &["lash_checkpoint_blob_refs"],
        source_missing_columns: &[("lash_trigger_occurrences", "reclaimable_at_ms")],
        source_missing_guards: &[],
        introduced_relations: &[
            "lash_checkpoint_blob_refs",
            "idx_lash_checkpoint_blob_refs_blob_ref",
            "idx_lash_sessions_checkpoint_ref",
            "idx_lash_node_anchors_checkpoint_ref",
            "idx_lash_runtime_effect_replay_group_unsettled",
            "idx_lash_trigger_occurrences_reclaimable",
        ],
        statements: &[
            CHECKPOINT_BLOB_REFS_DDL,
            CHECKPOINT_BLOB_REFS_REVERSE_INDEX_DDL,
            SESSIONS_CHECKPOINT_REF_INDEX_DDL,
            NODE_ANCHORS_CHECKPOINT_REF_INDEX_DDL,
            RUNTIME_EFFECT_REPLAY_GROUP_UNSETTLED_INDEX_DDL,
            TRIGGER_OCCURRENCE_RECLAIMABLE_AT_DDL,
            TRIGGER_OCCURRENCE_RECLAIMABLE_ARM_DDL,
            TRIGGER_OCCURRENCE_RECLAIMABLE_INDEX_DDL,
        ],
    },
    // A 53 store takes both later generations at once: the 54 effect-group
    // journal (one new table, its two indexes, two nullable columns on
    // `lash_runtime_effect_replay`) and the 55 drain index over them.
    SchemaMigration {
        from: 53,
        to: 57,
        source_missing_tables: &["lash_runtime_effect_group", "lash_checkpoint_blob_refs"],
        source_missing_columns: &[
            ("lash_runtime_effect_replay", "group_key"),
            ("lash_runtime_effect_replay", "settlement_seq"),
            ("lash_trigger_occurrences", "reclaimable_at_ms"),
        ],
        source_missing_guards: EFFECT_GROUP_GUARDS,
        introduced_relations: &[
            "lash_runtime_effect_group",
            "idx_lash_runtime_effect_group_session",
            "idx_lash_runtime_effect_group_scope",
            "uq_lash_runtime_effect_replay_group_seq",
            "idx_lash_runtime_effect_replay_group_unsettled",
            "lash_checkpoint_blob_refs",
            "idx_lash_checkpoint_blob_refs_blob_ref",
            "idx_lash_sessions_checkpoint_ref",
            "idx_lash_node_anchors_checkpoint_ref",
            "idx_lash_trigger_occurrences_reclaimable",
        ],
        statements: &[
            CHECKPOINT_BLOB_REFS_DDL,
            CHECKPOINT_BLOB_REFS_REVERSE_INDEX_DDL,
            SESSIONS_CHECKPOINT_REF_INDEX_DDL,
            NODE_ANCHORS_CHECKPOINT_REF_INDEX_DDL,
            RUNTIME_EFFECT_REPLAY_GROUP_KEY_DDL,
            RUNTIME_EFFECT_REPLAY_SETTLEMENT_SEQ_DDL,
            RUNTIME_EFFECT_REPLAY_GROUP_SEQ_INDEX_DDL,
            RUNTIME_EFFECT_GROUP_DDL,
            RUNTIME_EFFECT_GROUP_SESSION_INDEX_DDL,
            RUNTIME_EFFECT_GROUP_SCOPE_INDEX_DDL,
            RUNTIME_EFFECT_REPLAY_GROUP_UNSETTLED_INDEX_DDL,
            TRIGGER_OCCURRENCE_RECLAIMABLE_AT_DDL,
            TRIGGER_OCCURRENCE_RECLAIMABLE_ARM_DDL,
            TRIGGER_OCCURRENCE_RECLAIMABLE_INDEX_DDL,
        ],
    },
    SchemaMigration {
        from: 52,
        to: 57,
        source_missing_tables: &["lash_runtime_effect_group", "lash_checkpoint_blob_refs"],
        source_missing_columns: &[
            ("lash_runtime_effect_replay", "group_key"),
            ("lash_runtime_effect_replay", "settlement_seq"),
            ("lash_trigger_occurrences", "reclaimable_at_ms"),
        ],
        source_missing_guards: EFFECT_GROUP_GUARDS,
        introduced_relations: &[
            "idx_lash_queued_work_session_command_order",
            "idx_lash_pending_turn_input_order",
            "lash_runtime_effect_group",
            "idx_lash_runtime_effect_group_session",
            "idx_lash_runtime_effect_group_scope",
            "uq_lash_runtime_effect_replay_group_seq",
            "idx_lash_runtime_effect_replay_group_unsettled",
            "lash_checkpoint_blob_refs",
            "idx_lash_checkpoint_blob_refs_blob_ref",
            "idx_lash_sessions_checkpoint_ref",
            "idx_lash_node_anchors_checkpoint_ref",
            "idx_lash_trigger_occurrences_reclaimable",
        ],
        statements: &[
            CHECKPOINT_BLOB_REFS_DDL,
            CHECKPOINT_BLOB_REFS_REVERSE_INDEX_DDL,
            SESSIONS_CHECKPOINT_REF_INDEX_DDL,
            NODE_ANCHORS_CHECKPOINT_REF_INDEX_DDL,
            QUEUED_WORK_SESSION_COMMAND_ORDER_INDEX_DDL,
            PENDING_TURN_INPUT_ORDER_INDEX_DDL,
            RUNTIME_EFFECT_REPLAY_GROUP_KEY_DDL,
            RUNTIME_EFFECT_REPLAY_SETTLEMENT_SEQ_DDL,
            RUNTIME_EFFECT_REPLAY_GROUP_SEQ_INDEX_DDL,
            RUNTIME_EFFECT_GROUP_DDL,
            RUNTIME_EFFECT_GROUP_SESSION_INDEX_DDL,
            RUNTIME_EFFECT_GROUP_SCOPE_INDEX_DDL,
            RUNTIME_EFFECT_REPLAY_GROUP_UNSETTLED_INDEX_DDL,
            TRIGGER_OCCURRENCE_RECLAIMABLE_AT_DDL,
            TRIGGER_OCCURRENCE_RECLAIMABLE_ARM_DDL,
            TRIGGER_OCCURRENCE_RECLAIMABLE_INDEX_DDL,
        ],
    },
    SchemaMigration {
        from: 51,
        to: 57,
        source_missing_tables: &[
            "lash_attachment_condemnations",
            "lash_runtime_effect_group",
            "lash_checkpoint_blob_refs",
        ],
        source_missing_columns: &[
            ("lash_runtime_effect_replay", "group_key"),
            ("lash_runtime_effect_replay", "settlement_seq"),
            ("lash_trigger_occurrences", "reclaimable_at_ms"),
        ],
        source_missing_guards: EFFECT_GROUP_GUARDS,
        introduced_relations: &[
            "lash_attachment_condemnations",
            "idx_lash_queued_work_session_command_order",
            "idx_lash_pending_turn_input_order",
            "lash_runtime_effect_group",
            "idx_lash_runtime_effect_group_session",
            "idx_lash_runtime_effect_group_scope",
            "uq_lash_runtime_effect_replay_group_seq",
            "idx_lash_runtime_effect_replay_group_unsettled",
            "lash_checkpoint_blob_refs",
            "idx_lash_checkpoint_blob_refs_blob_ref",
            "idx_lash_sessions_checkpoint_ref",
            "idx_lash_node_anchors_checkpoint_ref",
            "idx_lash_trigger_occurrences_reclaimable",
        ],
        statements: &[
            CHECKPOINT_BLOB_REFS_DDL,
            CHECKPOINT_BLOB_REFS_REVERSE_INDEX_DDL,
            SESSIONS_CHECKPOINT_REF_INDEX_DDL,
            NODE_ANCHORS_CHECKPOINT_REF_INDEX_DDL,
            ATTACHMENT_CONDEMNATIONS_DDL,
            QUEUED_WORK_SESSION_COMMAND_ORDER_INDEX_DDL,
            PENDING_TURN_INPUT_ORDER_INDEX_DDL,
            RUNTIME_EFFECT_REPLAY_GROUP_KEY_DDL,
            RUNTIME_EFFECT_REPLAY_SETTLEMENT_SEQ_DDL,
            RUNTIME_EFFECT_REPLAY_GROUP_SEQ_INDEX_DDL,
            RUNTIME_EFFECT_GROUP_DDL,
            RUNTIME_EFFECT_GROUP_SESSION_INDEX_DDL,
            RUNTIME_EFFECT_GROUP_SCOPE_INDEX_DDL,
            RUNTIME_EFFECT_REPLAY_GROUP_UNSETTLED_INDEX_DDL,
            TRIGGER_OCCURRENCE_RECLAIMABLE_AT_DDL,
            TRIGGER_OCCURRENCE_RECLAIMABLE_ARM_DDL,
            TRIGGER_OCCURRENCE_RECLAIMABLE_INDEX_DDL,
        ],
    },
    // Component-50 stores skipped the 51 generation entirely; they take one
    // creation-only migration that lands every later generation at once.
    SchemaMigration {
        from: 50,
        to: 57,
        source_missing_tables: &[
            "lash_attachment_condemnations",
            "lash_process_parent_end_plans",
            "lash_tool_intent_submissions",
            "lash_runtime_effect_group",
            "lash_checkpoint_blob_refs",
        ],
        source_missing_columns: &[
            ("lash_runtime_effect_replay", "group_key"),
            ("lash_runtime_effect_replay", "settlement_seq"),
            ("lash_trigger_occurrences", "reclaimable_at_ms"),
        ],
        source_missing_guards: EFFECT_GROUP_GUARDS,
        introduced_relations: &[
            "lash_attachment_condemnations",
            "lash_process_parent_end_plans",
            "lash_tool_intent_submissions",
            "idx_lash_tool_intent_submissions_scope",
            "idx_lash_queued_work_session_command_order",
            "idx_lash_pending_turn_input_order",
            "lash_runtime_effect_group",
            "idx_lash_runtime_effect_group_session",
            "idx_lash_runtime_effect_group_scope",
            "uq_lash_runtime_effect_replay_group_seq",
            "idx_lash_runtime_effect_replay_group_unsettled",
            "lash_checkpoint_blob_refs",
            "idx_lash_checkpoint_blob_refs_blob_ref",
            "idx_lash_sessions_checkpoint_ref",
            "idx_lash_node_anchors_checkpoint_ref",
            "idx_lash_trigger_occurrences_reclaimable",
        ],
        statements: &[
            CHECKPOINT_BLOB_REFS_DDL,
            CHECKPOINT_BLOB_REFS_REVERSE_INDEX_DDL,
            SESSIONS_CHECKPOINT_REF_INDEX_DDL,
            NODE_ANCHORS_CHECKPOINT_REF_INDEX_DDL,
            PROCESS_PARENT_END_PLANS_DDL,
            TOOL_INTENT_SUBMISSIONS_DDL,
            TOOL_INTENT_SUBMISSIONS_INDEX_DDL,
            ATTACHMENT_CONDEMNATIONS_DDL,
            QUEUED_WORK_SESSION_COMMAND_ORDER_INDEX_DDL,
            PENDING_TURN_INPUT_ORDER_INDEX_DDL,
            RUNTIME_EFFECT_REPLAY_GROUP_KEY_DDL,
            RUNTIME_EFFECT_REPLAY_SETTLEMENT_SEQ_DDL,
            RUNTIME_EFFECT_REPLAY_GROUP_SEQ_INDEX_DDL,
            RUNTIME_EFFECT_GROUP_DDL,
            RUNTIME_EFFECT_GROUP_SESSION_INDEX_DDL,
            RUNTIME_EFFECT_GROUP_SCOPE_INDEX_DDL,
            RUNTIME_EFFECT_REPLAY_GROUP_UNSETTLED_INDEX_DDL,
            TRIGGER_OCCURRENCE_RECLAIMABLE_AT_DDL,
            TRIGGER_OCCURRENCE_RECLAIMABLE_ARM_DDL,
            TRIGGER_OCCURRENCE_RECLAIMABLE_INDEX_DDL,
        ],
    },
];
/// How one open should treat the database's schema.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SchemaOpenOptions {
    pub(crate) provisioning: SchemaProvisioning,
    pub(crate) check: SchemaCheck,
}

/// Brings the database to the state this build requires and returns the
/// store-resident await-event signing secret.
///
/// Both provisioning modes end in the same structural verification, so a
/// database that opens is a database whose shape lash has read — never one whose
/// version stamp merely claimed the right number.
pub(crate) async fn ensure_schema(
    pool: &PgPool,
    options: SchemaOpenOptions,
) -> Result<Vec<u8>, StoreError> {
    let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
    // Serializes lash's own openers, so two concurrent first opens cannot race
    // each other's DDL and a verifying open cannot read a half-applied batch from
    // a provisioning one. It does *not* coordinate with host migrations: nothing
    // outside lash takes this key unless a host chooses to, which
    // `PostgresStorage::schema_advisory_lock_key` exists to let it do. The lock
    // needs no privileges, so it is taken in both modes.
    let (lock_namespace, lock_key) = SCHEMA_ADVISORY_LOCK_KEY;
    sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
        .bind(lock_namespace)
        .bind(lock_key)
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
    if options.provisioning == SchemaProvisioning::LashManaged {
        // Preflight before the DDL: a stale baseline must be rejected rather than
        // have this build's creation statements layered over it.
        let search_path = read_search_path(&mut tx).await?;
        let installation = resolve_installation(&mut tx, &search_path).await?;
        let mut search_path_to_restore = None;
        let preflight_mismatch = if let Some(installation) = installation {
            match read_component_version(&mut tx, &installation, &SchemaShape::expected()).await? {
                ComponentVersion::Readable(Some(found)) if found != SCHEMA_VERSION => {
                    match apply_schema_migration(
                        &mut tx,
                        installation.namespace(),
                        found,
                        options.check == SchemaCheck::Enforce,
                    )
                    .await?
                    {
                        SchemaMigrationOutcome::Applied {
                            previous_search_path,
                        } => {
                            search_path_to_restore = Some(previous_search_path);
                            None
                        }
                        SchemaMigrationOutcome::NotApplicable => {
                            Some((installation.namespace().to_string(), Some(found)))
                        }
                        SchemaMigrationOutcome::Divergent { artifacts } => {
                            let preflight = SchemaReport {
                                schema: Some(installation.namespace().to_string()),
                                expected_version: SCHEMA_VERSION,
                                found_version: Some(found),
                                findings: vec![SchemaFinding::VersionMismatch {
                                    expected: SCHEMA_VERSION,
                                    found: Some(found),
                                }],
                            };
                            record_schema_migration_denial(
                                &preflight,
                                options,
                                "denied_migration_divergence",
                                "migration_artifacts",
                                &artifacts.join(", "),
                            );
                            return Err(schema_migration_divergence_error(found, &artifacts));
                        }
                        SchemaMigrationOutcome::SourceMismatch { report } => {
                            let details = report
                                .findings
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join("; ");
                            record_schema_migration_denial(
                                &report,
                                options,
                                "denied_migration_source_shape",
                                "migration_source_findings",
                                &details,
                            );
                            return Err(schema_migration_source_mismatch_error(found, &report));
                        }
                    }
                }
                ComponentVersion::Readable(found) if found != Some(SCHEMA_VERSION) => {
                    Some((installation.namespace().to_string(), found))
                }
                ComponentVersion::Readable(_) | ComponentVersion::Unreadable => None,
            }
        } else {
            let unstamped_schema: Option<String> = sqlx::query_scalar(
                r#"SELECT current_schema()::text
                   WHERE EXISTS (
                       SELECT 1
                       FROM pg_catalog.pg_class AS class
                       JOIN pg_catalog.pg_namespace AS namespace
                         ON namespace.oid = class.relnamespace
                       WHERE namespace.nspname = current_schema()
                         AND class.relname LIKE 'lash\_%' ESCAPE '\'
                         AND class.relname <> 'lash_schema_versions'
                         AND class.relkind IN ('r', 'p', 'v', 'm', 'S')
                   )"#,
            )
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            unstamped_schema.map(|schema| (schema, None))
        };
        if let Some((schema, found_version)) = preflight_mismatch {
            // Same field set as every other outcome, built from what the preflight
            // knows: it runs before the structural read, so the only finding it can
            // have is the version itself.
            let preflight = SchemaReport {
                schema: Some(schema),
                expected_version: SCHEMA_VERSION,
                found_version,
                findings: vec![SchemaFinding::VersionMismatch {
                    expected: SCHEMA_VERSION,
                    found: found_version,
                }],
            };
            record_schema_gate_decision(&preflight, options, "denied_version_preflight");
            return Err(version_mismatch_error(found_version));
        }
        tx.execute(SCHEMA_DDL).await.map_err(store_sqlx_error)?;
        if let Some(search_path) = search_path_to_restore {
            sqlx::query("SELECT set_config('search_path', $1, true)")
                .bind(search_path)
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
        }
    }

    let report = verify_schema_shape(&mut tx).await?;
    // Any mismatch left after the explicit migration preflight is the
    // reject-and-recreate boundary. `SchemaCheck` governs the catalog comparison
    // only; letting `WarnOnly` downgrade this would silently run one build against
    // another schema generation.
    if report.found_version != Some(SCHEMA_VERSION) {
        record_schema_gate_decision(&report, options, "denied_version");
        return Err(version_mismatch_error(report.found_version));
    }
    let admitted_as = match (report.is_conformant(), options.check) {
        (true, _) => "allowed",
        (false, SchemaCheck::Enforce) => {
            record_schema_gate_decision(&report, options, "denied_shape");
            return Err(StoreError::Backend(report.to_string()));
        }
        (false, SchemaCheck::WarnOnly) => {
            tracing::warn!(
                "opening Postgres storage against a non-conformant schema because \
                 SchemaCheck::WarnOnly is configured: {report}"
            );
            "allowed_warn_only"
        }
    };

    let signing_secret: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT signing_secret FROM lash_await_event_meta WHERE singleton = TRUE",
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(store_sqlx_error)?;
    // The secret is a data precondition, not a shape: `SchemaCheck::WarnOnly`
    // relaxes structural enforcement, never the store's ability to construct
    // itself. Without this row there is no key to authenticate durable await-event
    // promises with, so there is nothing to hand back. A host-provisioned database
    // missing it must apply the seed statements from `schema.sql`.
    //
    // The admission is recorded only after this succeeds. Logging it earlier would
    // let a database with an unusable secret produce an admission event and then a
    // rejected open, which is the one shape of decision evidence worse than none.
    let signing_secret = match signing_secret {
        Some(secret) if secret.len() == AWAIT_EVENT_SIGNING_SECRET_BYTES => secret,
        Some(secret) => {
            record_schema_gate_decision(&report, options, "denied_seed_secret_width");
            return Err(StoreError::Backend(format!(
                "Postgres await-event signing secret has {} bytes, expected \
                 {AWAIT_EVENT_SIGNING_SECRET_BYTES}",
                secret.len()
            )));
        }
        None => {
            record_schema_gate_decision(&report, options, "denied_seed_secret_missing");
            return Err(StoreError::Backend(
                "Postgres await-event signing secret row is missing from \
                 lash_await_event_meta; apply the seed statements from this build's schema.sql \
                 artifact"
                    .to_string(),
            ));
        }
    };
    record_schema_gate_decision(&report, options, admitted_as);
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(signing_secret)
}

enum SchemaMigrationOutcome {
    NotApplicable,
    Applied { previous_search_path: String },
    Divergent { artifacts: Vec<String> },
    SourceMismatch { report: SchemaReport },
}

async fn apply_schema_migration(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    namespace: &str,
    found: i32,
    apply: bool,
) -> Result<SchemaMigrationOutcome, StoreError> {
    let Some(migration) = SCHEMA_MIGRATIONS
        .iter()
        .find(|migration| migration.from == found && migration.to == SCHEMA_VERSION)
    else {
        return Ok(SchemaMigrationOutcome::NotApplicable);
    };
    let artifacts = sqlx::query_scalar::<_, String>(
        r#"SELECT pg_catalog.format('%I.%I', namespace.nspname, class.relname)
           FROM pg_catalog.pg_class AS class
           JOIN pg_catalog.pg_namespace AS namespace
             ON namespace.oid = class.relnamespace
          WHERE namespace.nspname = ANY(pg_catalog.current_schemas(true))
            AND class.relname = ANY($1)
          ORDER BY namespace.nspname, class.relname"#,
    )
    .bind(migration.introduced_relations)
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    if !artifacts.is_empty() {
        return Ok(SchemaMigrationOutcome::Divergent { artifacts });
    }
    if !apply {
        return Ok(SchemaMigrationOutcome::NotApplicable);
    }
    let source_report = verify_schema_migration_source_shape(tx).await?;
    if !migration.matches_source_shape(&source_report) {
        return Ok(SchemaMigrationOutcome::SourceMismatch {
            report: source_report,
        });
    }
    let previous_search_path: String = sqlx::query_scalar("SELECT current_setting('search_path')")
        .fetch_one(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    sqlx::query("SELECT set_config('search_path', pg_catalog.quote_ident($1::text), true)")
        .bind(namespace)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    for statement in migration.statements {
        sqlx::query(statement)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    }
    backfill_checkpoint_blob_refs_tx(tx).await?;
    let stamped = sqlx::query(
        "UPDATE lash_schema_versions
         SET version = $1
         WHERE component = $2 AND version = $3",
    )
    .bind(migration.to)
    .bind(SCHEMA_COMPONENT)
    .bind(migration.from)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?
    .rows_affected();
    if stamped != 1 {
        return Err(StoreError::Backend(format!(
            "Postgres schema migration {} -> {} updated {stamped} component stamps, expected 1",
            migration.from, migration.to
        )));
    }
    tracing::info!(
        component = SCHEMA_COMPONENT,
        from_version = migration.from,
        to_version = migration.to,
        outcome = "migrated",
        "applied Lash-managed PostgreSQL schema migration"
    );
    Ok(SchemaMigrationOutcome::Applied {
        previous_search_path,
    })
}

/// Arm exact-edge reclaim for every checkpoint manifest that was rooted before
/// component 56 existed. This runs after the projection table is created and
/// before the component stamp advances, inside the opener's schema transaction.
///
/// Only the manifest envelope is decoded. Component codec compatibility remains
/// a hydration concern: an old component version can still name an exact blob
/// edge without this binary interpreting its body.
async fn backfill_checkpoint_blob_refs_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), StoreError> {
    let rooted_manifests = sqlx::query(
        "WITH rooted AS (
             SELECT checkpoint_ref FROM lash_sessions WHERE checkpoint_ref IS NOT NULL
             UNION
             SELECT checkpoint_ref FROM lash_node_anchors
         )
         SELECT rooted.checkpoint_ref, blob.content
         FROM rooted
         LEFT JOIN lash_blobs AS blob ON blob.hash = rooted.checkpoint_ref
         ORDER BY rooted.checkpoint_ref",
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    for row in rooted_manifests {
        let checkpoint_ref: String = row.get(0);
        let bytes: Option<Vec<u8>> = row.get(1);
        let bytes = bytes.ok_or_else(|| StoreError::StoredDataCorrupt {
            record_kind: "SessionCheckpoint",
            message: format!("rooted checkpoint manifest `{checkpoint_ref}` is missing"),
        })?;
        let manifest: SessionCheckpoint = decode_versioned_msgpack_record(
            &bytes,
            "SessionCheckpoint",
            lash_core::store::SESSION_CHECKPOINT_SCHEMA_VERSION,
        )?;
        let component_refs = manifest
            .components
            .values()
            .map(|descriptor| descriptor.blob_ref.as_str())
            .collect::<Vec<_>>();
        sqlx::query(
            "INSERT INTO lash_checkpoint_blob_refs (checkpoint_ref, blob_ref)
             SELECT $1, component_ref
             FROM unnest($2::TEXT[]) AS component_ref
             ON CONFLICT (checkpoint_ref, blob_ref) DO NOTHING",
        )
        .bind(&checkpoint_ref)
        .bind(component_refs)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    }
    Ok(())
}

impl SchemaMigration {
    fn matches_source_shape(&self, report: &SchemaReport) -> bool {
        if report.found_version != Some(self.from) {
            return false;
        }
        let mut missing_tables = self
            .source_missing_tables
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        let mut missing_columns = self
            .source_missing_columns
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        let mut missing_guards = self.source_missing_guards.iter().collect::<Vec<_>>();
        let mut saw_version = false;
        for finding in &report.findings {
            match finding {
                SchemaFinding::VersionMismatch { expected, found }
                    if *expected == self.to && *found == Some(self.from) =>
                {
                    if saw_version {
                        return false;
                    }
                    saw_version = true;
                }
                SchemaFinding::MissingTable { table } if missing_tables.remove(table.as_str()) => {}
                SchemaFinding::MissingColumn { table, expected }
                    if is_creation_only_column(expected)
                        && missing_columns.remove(&(table.as_str(), expected.name.as_str())) => {}
                SchemaFinding::MissingUniqueGuard { table, expected }
                    if remove_guard(&mut missing_guards, table, expected) => {}
                _ => return false,
            }
        }
        saw_version
            && missing_tables.is_empty()
            && missing_columns.is_empty()
            && missing_guards.is_empty()
    }
}

/// Whether a missing column is one PostgreSQL adds without rewriting a row.
///
/// This is the property that put column-adding migrations in the creation-only
/// class at all. A nullable column with no value source of its own is recorded
/// in catalog metadata and touches no page, so an effect journal keeps every
/// recorded `envelope_hash` across the bump. A `NOT NULL DEFAULT` or a
/// `BIGSERIAL` supplies a value for every existing row, which is a rewrite of
/// the whole table under a lock — a data migration wearing a creation-only
/// declaration. Checked here rather than trusted from the declaration, because
/// the declaration names a column and the *shape* is what decides.
fn is_creation_only_column(expected: &ColumnShape) -> bool {
    expected.nullable && !expected.value_source.supplies_own_value()
}

/// Consumes the declared guard matching a finding.
///
/// Matched on table, key-column set, and predicate together — and only for a
/// guard that is neither a primary key nor `NULLS NOT DISTINCT`. Columns alone
/// would make a declaration for an added partial index also excuse a *missing
/// primary key* or a missing full `UNIQUE` over the same columns, which guard
/// strictly more rows and whose absence is real drift.
fn remove_guard(
    declared: &mut Vec<&'static DeclaredGuard>,
    table: &str,
    expected: &UniqueGuard,
) -> bool {
    if expected.primary_key || expected.nulls_not_distinct {
        return false;
    }
    let columns = expected
        .columns
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let Some(index) = declared.iter().position(|guard| {
        guard.table == table
            && guard
                .columns
                .iter()
                .copied()
                .map(str::to_string)
                .collect::<std::collections::BTreeSet<_>>()
                == columns
            && expected.predicate.as_deref() == Some(guard.predicate)
    }) else {
        return false;
    };
    declared.remove(index);
    true
}

fn schema_migration_divergence_error(found: i32, artifacts: &[String]) -> StoreError {
    StoreError::Backend(format!(
        "Postgres schema component `{SCHEMA_COMPONENT}` has version {found}, expected \
         {SCHEMA_VERSION}, but the live schema contains schema artifacts newer than the recorded \
         version: {}. Lash will not guess whether this is a partial migration, version-ledger \
         rollback, or other corruption. Stop the deployment, inspect and recreate the whole Lash \
         trust domain before retrying; see docs/persistence.html#delete-sessions.",
        artifacts.join(", ")
    ))
}

fn schema_migration_source_mismatch_error(found: i32, report: &SchemaReport) -> StoreError {
    let findings = report
        .findings
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    StoreError::Backend(format!(
        "Postgres schema component `{SCHEMA_COMPONENT}` has version {found}, expected \
         {SCHEMA_VERSION}, but the live schema does not match the published component-{found} \
         migration source shape. Lash will not run migration DDL against an unknown source or \
         guess at repairs. Stop the deployment, inspect and recreate the whole Lash trust domain \
         before retrying; source-shape findings: {findings}"
    ))
}

/// Runs the structural check under the published advisory key, held in *shared*
/// mode, with every catalog read pinned to one snapshot.
///
/// Two orderings matter here and neither is incidental.
///
/// The key is taken at *session* scope, before the transaction begins, because a
/// `REPEATABLE READ` snapshot is established by the transaction's first statement —
/// and that includes the statement that waits for a lock. Acquiring an
/// `xact`-scoped lock as the first statement would therefore snapshot the catalog
/// *before* the lock was granted, so a verification that queued behind a host
/// migration would go on to describe the schema as it was before that migration.
/// Measured on PostgreSQL 16: a transaction whose first statement blocks on the key
/// cannot see a table the lock holder committed while it waited.
///
/// The transaction is then `REPEATABLE READ` so every `pg_catalog` read shares one
/// snapshot. `READ COMMITTED` would re-snapshot per statement, which is what let a
/// concurrently committed catalog row appear midway through a verification.
pub(crate) async fn verify_schema_under_advisory_lock(
    pool: &PgPool,
) -> Result<SchemaReport, StoreError> {
    let (lock_namespace, lock_key) = SCHEMA_ADVISORY_LOCK_KEY;
    // Detached rather than borrowed from the pool, because the lock this takes is
    // *session*-scoped: a future cancelled between the lock and the unlock would
    // otherwise hand a still-locked connection back to the pool and block every
    // later exclusive holder for that connection's lifetime. An owned connection is
    // closed when it drops — on the error and cancellation paths as much as the
    // happy one — and the backend releases the session lock with it.
    let mut connection = pool.acquire().await.map_err(store_sqlx_error)?.detach();
    let verified = async {
        sqlx::query("SELECT pg_advisory_lock_shared($1, $2)")
            .bind(lock_namespace)
            .bind(lock_key)
            .execute(&mut connection)
            .await
            .map_err(store_sqlx_error)?;
        verify_within_repeatable_read(&mut connection).await
    }
    .await;
    let _ = sqlx::Connection::close(connection).await;
    verified
}

/// Reads the schema inside one `REPEATABLE READ` transaction.
async fn verify_within_repeatable_read(
    connection: &mut sqlx::PgConnection,
) -> Result<SchemaReport, StoreError> {
    let mut tx = sqlx::Connection::begin(connection)
        .await
        .map_err(store_sqlx_error)?;
    // Must precede every other statement in the transaction: PostgreSQL rejects the
    // change once a snapshot has been established.
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
    let report = verify_schema_shape(&mut tx).await?;
    // Read-only, but committing rather than rolling back keeps the transaction's
    // disposition unambiguous in a host's own logs.
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(report)
}

/// Emits the schema gate's decision basis.
///
/// A gate that can deny ships the inputs it consulted, not just its verdict
/// (`docs/agents/way-of-working.md`): the stamped and expected versions, both
/// policy knobs, and the finding counts per class, so a refused open can be
/// diagnosed from a trace without reproducing it.
fn record_schema_gate_decision(
    report: &SchemaReport,
    options: SchemaOpenOptions,
    outcome: &'static str,
) {
    let counts = report.finding_counts();
    let fields = tracing::field::display(
        counts
            .iter()
            .map(|(section, count)| format!("{section}={count}"))
            .collect::<Vec<_>>()
            .join(", "),
    );
    let schema = report.schema.as_deref().unwrap_or("<unresolved>");
    match outcome {
        "allowed" => tracing::debug!(
            component = SCHEMA_COMPONENT,
            schema,
            expected_version = report.expected_version,
            found_version = ?report.found_version,
            provisioning = ?options.provisioning,
            schema_check = ?options.check,
            findings = %fields,
            finding_total = report.findings.len(),
            outcome,
            "lash Postgres schema gate admitted the database"
        ),
        _ => tracing::warn!(
            component = SCHEMA_COMPONENT,
            schema,
            expected_version = report.expected_version,
            found_version = ?report.found_version,
            provisioning = ?options.provisioning,
            schema_check = ?options.check,
            findings = %fields,
            finding_total = report.findings.len(),
            outcome,
            "lash Postgres schema gate decided against admitting the database as-is"
        ),
    }
}

/// Emits the full basis for a migration-specific denial.
///
/// The ordinary schema-gate event carries finding counts. Migration preflight
/// also consults concrete artifact names or source-shape findings, so those
/// inputs ride the denial event rather than existing only in the returned error.
fn record_schema_migration_denial(
    report: &SchemaReport,
    options: SchemaOpenOptions,
    outcome: &'static str,
    detail_kind: &'static str,
    details: &str,
) {
    let counts = report.finding_counts();
    let fields = tracing::field::display(
        counts
            .iter()
            .map(|(section, count)| format!("{section}={count}"))
            .collect::<Vec<_>>()
            .join(", "),
    );
    let schema = report.schema.as_deref().unwrap_or("<unresolved>");
    tracing::warn!(
        component = SCHEMA_COMPONENT,
        schema,
        expected_version = report.expected_version,
        found_version = ?report.found_version,
        provisioning = ?options.provisioning,
        schema_check = ?options.check,
        findings = %fields,
        finding_total = report.findings.len(),
        migration_detail_kind = detail_kind,
        migration_details = details,
        outcome,
        "lash Postgres schema migration preflight refused the database"
    );
}

/// Renders the remaining version-mismatch error, naming the remedy rather than
/// only the numbers. The explicit floor migrations have already been handled
/// by the Lash-managed `Enforce` preflight when it is applicable.
pub(crate) fn version_mismatch_error(found: Option<i32>) -> StoreError {
    let (found, expected) = match found {
        Some(version) => (
            format!("has version {version}"),
            format!("expected {SCHEMA_VERSION}"),
        ),
        None => (
            "has no version stamp".to_string(),
            format!("expected version {SCHEMA_VERSION}"),
        ),
    };
    StoreError::Backend(format!(
        "Postgres schema component `{SCHEMA_COMPONENT}` {found}, {expected}. \
         The component schema is normally a reject-and-recreate boundary. This build has \
         explicit Lash-managed migrations from the published component-50, component-51, \
         component-52, component-53, component-54, component-55, and component-56 shapes to 57; they run only under \
         SchemaCheck::Enforce \
         after an exact \
         source-shape preflight. This mismatch \
         has no applicable migration. Drain affected sessions and recreate the whole Lash trust \
         domain with this version: provision \
         the database from this build's schema.sql artifact, and reset the tombstones, await-event \
         revocation ledger, effect journal, and Restate state together; see \
         docs/persistence.html#delete-sessions. This gate is unconditional; \
         SchemaCheck::WarnOnly does not relax it."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The declared 53 -> 57 migration, which every case below perturbs.
    fn migration() -> &'static SchemaMigration {
        SCHEMA_MIGRATIONS
            .iter()
            .find(|migration| migration.from == 53)
            .expect("the component-53 migration is declared")
    }

    fn column(name: &str, nullable: bool, value_source: ColumnValueSource) -> ColumnShape {
        ColumnShape {
            name: name.to_string(),
            sql_type: "text".to_string(),
            nullable,
            value_source,
        }
    }

    fn guard(primary_key: bool, predicate: Option<&str>, nulls_not_distinct: bool) -> UniqueGuard {
        UniqueGuard {
            primary_key,
            columns: vec!["group_key".to_string(), "settlement_seq".to_string()],
            predicate: predicate.map(str::to_string),
            nulls_not_distinct,
        }
    }

    /// The exact partial guard the 54 generation adds, as the shape checker
    /// renders it.
    fn declared_guard() -> UniqueGuard {
        guard(
            false,
            Some("(group_key is not null) and (settlement_seq is not null)"),
            false,
        )
    }

    fn report(findings: Vec<SchemaFinding>) -> SchemaReport {
        SchemaReport {
            schema: Some("public".to_string()),
            expected_version: 57,
            found_version: Some(53),
            findings,
        }
    }

    /// The full set of findings a genuine published component-53 database
    /// produces against this build, which the migration must accept.
    fn published_53_findings() -> Vec<SchemaFinding> {
        vec![
            SchemaFinding::VersionMismatch {
                expected: 57,
                found: Some(53),
            },
            SchemaFinding::MissingTable {
                table: "lash_runtime_effect_group".to_string(),
            },
            SchemaFinding::MissingTable {
                table: "lash_checkpoint_blob_refs".to_string(),
            },
            SchemaFinding::MissingColumn {
                table: "lash_runtime_effect_replay".to_string(),
                expected: column("group_key", true, ColumnValueSource::Supplied),
            },
            SchemaFinding::MissingColumn {
                table: "lash_runtime_effect_replay".to_string(),
                expected: column("settlement_seq", true, ColumnValueSource::Supplied),
            },
            SchemaFinding::MissingColumn {
                table: "lash_trigger_occurrences".to_string(),
                expected: ColumnShape {
                    name: "reclaimable_at_ms".to_string(),
                    sql_type: "bigint".to_string(),
                    nullable: true,
                    value_source: ColumnValueSource::Supplied,
                },
            },
            SchemaFinding::MissingUniqueGuard {
                table: "lash_runtime_effect_replay".to_string(),
                expected: declared_guard(),
            },
        ]
    }

    #[test]
    fn the_published_predecessor_shape_is_accepted() {
        assert!(
            migration().matches_source_shape(&report(published_53_findings())),
            "the shape the migration exists for must pass its own preflight"
        );
    }

    /// A declaration names a column; it does not license every column shape that
    /// could wear the name. `NOT NULL` and a value source each make the `ALTER`
    /// write a value into every existing row — a full table rewrite under lock,
    /// which is the one thing the creation-only class promises never happens.
    #[test]
    fn a_column_that_would_rewrite_every_row_is_refused_by_the_creation_only_door() {
        for (label, expected) in [
            (
                "NOT NULL",
                column("group_key", false, ColumnValueSource::Supplied),
            ),
            (
                "a default",
                column("group_key", true, ColumnValueSource::Default),
            ),
            (
                "an identity",
                column("group_key", true, ColumnValueSource::IdentityByDefault),
            ),
            (
                "a generated value",
                column("group_key", true, ColumnValueSource::Generated),
            ),
        ] {
            let mut findings = published_53_findings();
            findings[3] = SchemaFinding::MissingColumn {
                table: "lash_runtime_effect_replay".to_string(),
                expected,
            };
            assert!(
                !migration().matches_source_shape(&report(findings)),
                "a missing column with {label} must not pass the creation-only door"
            );
        }
    }

    /// A declared partial guard is permission for that guard alone. A missing
    /// `PRIMARY KEY` or full `UNIQUE` over the same columns guards strictly more
    /// rows, so tolerating it would migrate a database that is genuinely drifted
    /// — and silently drop a uniqueness guarantee lash depends on.
    #[test]
    fn a_stronger_missing_guard_over_the_same_columns_is_refused() {
        for (label, expected) in [
            ("a primary key", guard(true, None, false)),
            ("a full unique guard", guard(false, None, false)),
            (
                "a differently-predicated guard",
                guard(false, Some("(group_key is not null)"), false),
            ),
            (
                "a NULLS NOT DISTINCT rebuild",
                guard(
                    false,
                    Some("(group_key is not null) and (settlement_seq is not null)"),
                    true,
                ),
            ),
        ] {
            let mut findings = published_53_findings();
            findings[5] = SchemaFinding::MissingUniqueGuard {
                table: "lash_runtime_effect_replay".to_string(),
                expected,
            };
            assert!(
                !migration().matches_source_shape(&report(findings)),
                "{label} must not be consumed by the declaration for the partial guard"
            );
        }
    }

    /// The declaration is per table, not per column set: the same key columns on
    /// a table the migration says nothing about is drift.
    #[test]
    fn a_declared_guard_does_not_travel_to_another_table() {
        let mut findings = published_53_findings();
        findings[5] = SchemaFinding::MissingUniqueGuard {
            table: "lash_queued_work_batches".to_string(),
            expected: declared_guard(),
        };
        assert!(!migration().matches_source_shape(&report(findings)));
    }
}
