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
              source_node_id, created_at_ms, last_commit_at_ms, owning_process_id";

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

/// What a session's permanent deletion evidence is copied from.
///
/// Five metadata columns plus the head revision the session reached, joined
/// from the head table. It is its own list because the deleted set records a
/// session's final shape rather than its relation: no causal column survives
/// deletion, and the head revision is not a `session_meta` column at all.
pub const DELETED_EVIDENCE_COLUMNS: &str =
    "meta.session_id, meta.created_at_ms, meta.last_commit_at_ms,
                            COALESCE(head.head_revision, 0), meta.relation_kind,
                            meta.parent_session_id";

crate::statements! {
    /// `session_meta` statements both backends issue verbatim.
    pub struct SessionMetaStatements @ "session_meta" {
        /// Record fault `?2` on session `?1` at `?3` (ADR 0109 §9). A session
        /// already faulted keeps its first: zero rows.
        record_fault = "UPDATE session_meta SET fault_json = ?2, fault_at_ms = ?3
             WHERE session_id = ?1 AND fault_json IS NULL";

        /// Session `?1`'s standing fault and when it was recorded.
        select_fault = "SELECT fault_json, fault_at_ms FROM session_meta
             WHERE session_id = ?1 AND fault_json IS NOT NULL";

        /// The standing faults of sessions after `?1`, in session-id order,
        /// at most `?2`.
        list_faults = "SELECT session_id, fault_json, fault_at_ms FROM session_meta
             WHERE fault_json IS NOT NULL AND session_id > ?1
             ORDER BY session_id LIMIT ?2";

        /// Clear session `?1`'s fault. Zero rows means it had none.
        clear_fault = "UPDATE session_meta SET fault_json = NULL, fault_at_ms = NULL
             WHERE session_id = ?1 AND fault_json IS NOT NULL";

        /// Close session `?1` under control intent `?2`: record the intent,
        /// after which the session admits nothing (FIG-3600 S7). A session
        /// already closing is left as it is: zero rows.
        begin_close = "UPDATE session_meta
             SET closing_intent = ?2
             WHERE session_id = ?1 AND closing_intent IS NULL";

        /// The control intent session `?1` is closing under, if any.
        select_closing_intent = "SELECT closing_intent FROM session_meta WHERE session_id = ?1";

        /// The retention policy of session `?1`: its kind and, for
        /// `last_turns`, the window.
        select_retention = "SELECT retention_kind, retention_last_turns FROM session_meta
             WHERE session_id = ?1";

        /// Set session `?1`'s retention policy to kind `?2` with window `?3`.
        set_retention = "UPDATE session_meta
             SET retention_kind = ?2, retention_last_turns = ?3
             WHERE session_id = ?1";

        /// Retain checkpoint `?2` (or nothing) as the base session `?1`'s
        /// latest turn was admitted on, replacing the previous admission's
        /// (FIG-3682). Maintenance keeps it as a checkpoint root, so a replay
        /// of that turn can still read the head it was admitted on.
        retain_admission_base = "UPDATE session_meta SET admission_base_checkpoint_ref = ?2 WHERE session_id = ?1";

        /// The recorded lineage of `?1`.
        select_lineage = "SELECT relation_kind, parent_session_id, source_session_id, source_node_id
             FROM session_meta WHERE session_id = ?1";

        /// The process that runs session `?1` as its own, recorded when that
        /// process's start created the session (FIG-3607 R1); NULL for every
        /// other session.
        select_owning_process = "SELECT owning_process_id FROM session_meta WHERE session_id = ?1";

        /// The durable session-state version marker of `?1`.
        select_state_version = "SELECT session_state_version FROM session_meta WHERE session_id = ?1";

        /// Stamp `?1`'s last commit at `?2` and answer its retention policy,
        /// which decides whether the commit releases revisions: one
        /// statement, so a head commit reads the policy without a round trip
        /// of its own.
        touch_last_commit = "UPDATE session_meta SET last_commit_at_ms = ?2 WHERE session_id = ?1
             RETURNING retention_kind, retention_last_turns";

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
