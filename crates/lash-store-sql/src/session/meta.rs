//! `session_meta`: one row per session identity, its recorded lineage and the
//! causal columns that say what created it.

/// The table's unprefixed name.
pub const TABLE: &str = "session_meta";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str =
    "session_id, session_state_version, relation_kind, parent_session_id,
              caused_by_kind, caused_by_session_id, caused_by_turn_id,
              caused_by_effect_id, caused_by_call_id, caused_by_process_id,
              caused_by_process_event_sequence, caused_by_occurrence_id,
              caused_by_subscription_id, caused_by_subscription_incarnation,
              caused_by_subscription_revision, caused_by_node_id, source_session_id,
              source_node_id, created_at_ms, last_commit_at_ms";

/// The stored relation, as `StoredRelation` decodes it positionally.
///
/// The full row minus `session_state_version`, `created_at_ms` and
/// `last_commit_at_ms`: the version marker is read on its own by the admission
/// path, and the two timestamps are catalog reporting rather than relation
/// identity. Both backends decode this projection by position, so its order is
/// load-bearing.
pub const RELATION_COLUMNS: &str = "session_id, relation_kind, parent_session_id,
    caused_by_kind, caused_by_session_id, caused_by_turn_id,
    caused_by_effect_id, caused_by_call_id, caused_by_process_id,
    caused_by_process_event_sequence, caused_by_occurrence_id,
    caused_by_subscription_id, caused_by_subscription_incarnation,
    caused_by_subscription_revision, caused_by_node_id, source_session_id,
    source_node_id";

/// The four lineage columns alone.
///
/// Admission reads this inside its own transaction to decide whether a re-bind
/// agrees with what was recorded, so it must not open a nested one and must
/// not pay for the observer-intent reads a full metadata
/// load performs.
pub const LINEAGE_COLUMNS: &str =
    "relation_kind, parent_session_id, source_session_id, source_node_id";

/// The SQLite catalog projection: [`RELATION_COLUMNS`] qualified by the
/// catalog's `meta` alias, with the two catalog timestamps, the joined head
/// revision and the deleted flag the union's other arm supplies as `1`.
///
/// It is its own list because the catalog reads columns the relation read does
/// not — a session's creation instant, its last commit and its head revision
/// are what a listing sorts and reports — and because every column is
/// alias-qualified against the `LEFT JOIN` beside it.
pub const CATALOG_COLUMNS_SQLITE: &str =
    "meta.session_id, meta.relation_kind, meta.parent_session_id,
                    meta.caused_by_kind,
                    meta.caused_by_session_id, meta.caused_by_turn_id,
                    meta.caused_by_effect_id, meta.caused_by_call_id,
                    meta.caused_by_process_id, meta.caused_by_process_event_sequence,
                    meta.caused_by_occurrence_id, meta.caused_by_subscription_id,
                    meta.caused_by_subscription_incarnation,
                    meta.caused_by_subscription_revision, meta.caused_by_node_id,
                    meta.source_session_id, meta.source_node_id,
                    meta.created_at_ms,
                    meta.last_commit_at_ms, COALESCE(head.head_revision, 0), 0 AS deleted";

/// The PostgreSQL spelling of [`CATALOG_COLUMNS_SQLITE`].
///
/// It forks for the same two reasons the catalog statement does: the head
/// table is spelled `sessions` here (ADR 0098), and `deleted` is a real
/// boolean rather than SQLite's integer. `created_at_ms` is additionally
/// `COALESCE`d, because PostgreSQL's column is nullable where SQLite's is not.
pub const CATALOG_COLUMNS_POSTGRES: &str =
    "meta.session_id, meta.relation_kind, meta.parent_session_id,
                    meta.caused_by_kind,
                    meta.caused_by_session_id, meta.caused_by_turn_id,
                    meta.caused_by_effect_id, meta.caused_by_call_id,
                    meta.caused_by_process_id, meta.caused_by_process_event_sequence,
                    meta.caused_by_occurrence_id, meta.caused_by_subscription_id,
                    meta.caused_by_subscription_incarnation,
                    meta.caused_by_subscription_revision, meta.caused_by_node_id,
                    meta.source_session_id, meta.source_node_id,
                    COALESCE(meta.created_at_ms, 0) AS created_at_ms,
                    meta.last_commit_at_ms,
                    COALESCE(session.head_revision, 0) AS head_revision,
                    FALSE AS deleted";

/// What a session's permanent deletion evidence is copied from.
///
/// Five metadata columns plus the head revision the session reached, joined
/// from the head table. It is its own list because the deleted set records a
/// session's final shape rather than its relation: no causal column survives
/// deletion, and the head revision is not a `session_meta` column at all.
pub const DELETED_EVIDENCE_COLUMNS_SQLITE: &str =
    "meta.session_id, meta.created_at_ms, meta.last_commit_at_ms,
                            COALESCE(head.head_revision, 0), meta.relation_kind,
                            meta.parent_session_id";

/// The PostgreSQL spelling of [`DELETED_EVIDENCE_COLUMNS_SQLITE`], which
/// differs only in the head table's alias, because the head table itself is
/// spelled differently (ADR 0098).
pub const DELETED_EVIDENCE_COLUMNS_POSTGRES: &str =
    "meta.session_id, meta.created_at_ms, meta.last_commit_at_ms,
                    COALESCE(session.head_revision, 0), meta.relation_kind,
                    meta.parent_session_id";

/// What a process-prune batch copies into the deleted set, per target id.
///
/// The same six facts as [`DELETED_EVIDENCE_COLUMNS_POSTGRES`], `COALESCE`d
/// against the target list rather than against the metadata row: a
/// process-owned session id may have a head and no metadata row, and the
/// evidence must still cover it.
pub const BATCH_DELETED_EVIDENCE_COLUMNS: &str =
    "target.session_id, COALESCE(meta.created_at_ms, 0),
                meta.last_commit_at_ms, COALESCE(session.head_revision, 0),
                COALESCE(meta.relation_kind, 'root'), meta.parent_session_id";

/// A session's drive epoch and the admission that last raised it (ADR 0105
/// §2, B3): the fence every claim, reclaim and settlement checks. The start
/// marker of the execution that sealed the admission rides with it (L-S8),
/// and so does the control intent a closing session closes under.
pub const DRIVE_EPOCH_COLUMNS: &str =
    "drive_epoch, drive_admission_id, drive_root_start, closing_intent";

crate::statements! {
    /// `session_meta` statements both backends issue verbatim.
    pub struct SessionMetaStatements @ "session_meta" {
        /// Session `?1`'s drive epoch, the admission that last raised it, the
        /// start marker of the execution that sealed that admission, and the
        /// control intent the session is closing under.
        select_drive_epoch = "SELECT drive_epoch, drive_admission_id, drive_root_start, closing_intent,
            EXISTS (SELECT 1 FROM control_intents WHERE control_intents.session_id = session_meta.session_id
                AND kind IN ('cancel', 'fork') AND state NOT IN ('acknowledged', 'superseded'))
            FROM session_meta WHERE session_id = ?1";

        /// The seal's compare-and-set: raise session `?1`'s drive epoch from
        /// `?2` to `?3` under admission `?4`, sealed by the execution whose
        /// start marker is `?5`. Zero rows means the epoch moved.
        seal_drive_epoch = "UPDATE session_meta
             SET drive_epoch = ?3, drive_admission_id = ?4, drive_root_start = ?5
             WHERE session_id = ?1 AND drive_epoch = ?2";

        /// Close session `?1` under control intent `?2`: record the intent
        /// and raise the drive epoch under admission `?3`, so every fence an
        /// earlier admission sealed is stale (FIG-3600 S7); no execution
        /// sealed it, so it stores no start marker. A session already closing
        /// is left as it is: zero rows.
        begin_close = "UPDATE session_meta
             SET closing_intent = ?2, drive_epoch = drive_epoch + 1, drive_admission_id = ?3,
                 drive_root_start = NULL
             WHERE session_id = ?1 AND closing_intent IS NULL";

        /// The control intent session `?1` is closing under, if any.
        select_closing_intent = "SELECT closing_intent FROM session_meta WHERE session_id = ?1";

        /// Retain checkpoint `?2` (or nothing) as the base session `?1`'s
        /// latest turn was admitted on, replacing the previous admission's
        /// (FIG-3682). Maintenance keeps it as a checkpoint root, so a replay
        /// of that turn can still read the head it was admitted on.
        retain_admission_base = "UPDATE session_meta SET admission_base_checkpoint_ref = ?2 WHERE session_id = ?1";

        /// The recorded lineage of `?1`.
        select_lineage = "SELECT relation_kind, parent_session_id, source_session_id, source_node_id
             FROM session_meta WHERE session_id = ?1";

        /// The durable session-state version marker of `?1`.
        select_state_version = "SELECT session_state_version FROM session_meta WHERE session_id = ?1";

        touch_last_commit = "UPDATE session_meta SET last_commit_at_ms = ?2 WHERE session_id = ?1";

        /// Stamp `?1`'s session-state version marker to `?2`.
        ///
        /// Test-only, and named here rather than spelled at the two probes
        /// that issue it: a conformance probe that writes a marker no
        /// production path writes is exactly the sort of statement that drifts
        /// between the backends unnoticed.
        set_state_version = "UPDATE session_meta SET session_state_version = ?2 WHERE session_id = ?1";

        delete_by_session = "DELETE FROM session_meta WHERE session_id = ?1";
    }
}

crate::statements! {
    /// Statements for parked-root control and recovery.
    pub struct MetaRootVerbStatements @ "session_meta" {
        raise_epoch = "UPDATE session_meta SET drive_epoch = drive_epoch + 1, drive_admission_id = ?2, drive_root_start = NULL WHERE session_id = ?1 AND closing_intent IS NULL";
        sessions = "SELECT session_id FROM session_meta WHERE session_id > ?1 AND closing_intent IS NULL ORDER BY session_id LIMIT ?2";
    }
}
