//! Every process-family statement this store issues, rendered once.
//!
//! The shared halves live in `lash_store_sql::process`; the sets below are the
//! statements only SQLite issues, and each one has a `[[dialect_only]]` entry
//! in `crates/lash-store-sql/dialect-only.toml` saying why its text forks.
//!
//! # Why two renderings
//!
//! The process registry is its own SQLite database and addresses its tables
//! unqualified — the spelling every `INDEXED BY` plan in this crate was
//! measured against. One caller reaches the same tables from a *session*
//! connection with the registry `ATTACH`ed (the effect journal's bind-time
//! fence repair, `crate::scope_fence`), so the set is rendered a second time
//! through the `process_registry` qualifier. Rendering runs once, at first
//! use, for each.

use std::sync::LazyLock;

use lash_core::WakeDeliveryState;
use lash_core::store_backend_support as vocabulary;
use lash_store_sql::process::{
    artifact_cleanup::ArtifactCleanupStatements, definitions::DefinitionStatements,
    events::EventStatements, leases::LeaseStatements, observers::ObserverStatements,
    parent_end_plans::ParentEndPlanStatements, processes::ProcessStatements,
    segment_handovers::SegmentHandoverStatements, tombstones::TombstoneStatements,
    wake_allocation_floors::WakeAllocationFloorStatements, wake_deliveries::WakeDeliveryStatements,
    wake_redelivery_fences::WakeRedeliveryFenceStatements,
};
use lash_store_sql::{Dialect, Vocabulary, VocabularyTerm};

use crate::scope_fence::Schema;

/// `<column> = '<state>'`, for the one wake-delivery state named.
///
/// The label still comes from [`WakeDeliveryState`] through
/// `wake_delivery_state_sql_literal`; these wrappers only put a column and an
/// operator around it, so the vocabulary keeps exactly one source. A `SET`
/// clause and a `WHERE` clause spell the same text, which is why one term
/// serves both.
fn pending_wake_delivery_state(column: &str) -> String {
    wake_delivery_state_equals(column, WakeDeliveryState::Pending)
}

fn enqueuing_wake_delivery_state(column: &str) -> String {
    wake_delivery_state_equals(column, WakeDeliveryState::Enqueuing)
}

fn discarded_wake_delivery_state(column: &str) -> String {
    wake_delivery_state_equals(column, WakeDeliveryState::Discarded)
}

/// `<column> <> '<enqueued>'`: everything a delivery can be before it leaves
/// the queue.
fn not_enqueued_wake_delivery_state(column: &str) -> String {
    format!(
        "{column} <> {}",
        vocabulary::wake_delivery_state_sql_literal(WakeDeliveryState::Enqueued)
    )
}

/// `'<state>'`: the bare label, for the `VALUES` list that writes it.
///
/// The token still names the column the label belongs to, because that is
/// what says which vocabulary it is drawn from; the expansion needs only the
/// state.
fn pending_wake_delivery_state_value(_column: &str) -> String {
    vocabulary::wake_delivery_state_sql_literal(WakeDeliveryState::Pending)
}

fn wake_delivery_state_equals(column: &str, state: WakeDeliveryState) -> String {
    format!(
        "{column} = {}",
        vocabulary::wake_delivery_state_sql_literal(state)
    )
}

/// The process family's domain vocabulary, as this backend supplies it.
///
/// Every expansion is generated from `lash_core::ProcessStatus` or
/// `lash_core::WakeDeliveryState`, so adding a variant is still one edit in
/// the enum rather than one per statement. The term names are the domain's and
/// are identical in the PostgreSQL store.
const PROCESS_LIFECYCLE: Vocabulary = Vocabulary::new(&[
    VocabularyTerm::new(
        "live_process_status",
        vocabulary::live_process_status_predicate_sql,
    ),
    VocabularyTerm::new(
        "retired_process_status",
        vocabulary::retired_process_status_predicate_sql,
    ),
    VocabularyTerm::new(
        "nonterminal_process_status",
        vocabulary::nonterminal_process_status_predicate_sql,
    ),
    VocabularyTerm::new(
        "undelivered_wake_delivery_state",
        vocabulary::undelivered_wake_delivery_state_predicate_sql,
    ),
    VocabularyTerm::new("pending_wake_delivery_state", pending_wake_delivery_state),
    VocabularyTerm::new(
        "pending_wake_delivery_state_value",
        pending_wake_delivery_state_value,
    ),
    VocabularyTerm::new(
        "enqueuing_wake_delivery_state",
        enqueuing_wake_delivery_state,
    ),
    VocabularyTerm::new(
        "discarded_wake_delivery_state",
        discarded_wake_delivery_state,
    ),
    VocabularyTerm::new(
        "not_enqueued_wake_delivery_state",
        not_enqueued_wake_delivery_state,
    ),
]);

lash_store_sql::statements! {
    /// `processes` statements only SQLite issues.
    pub(crate) struct ProcessSqliteStatements @ "process" {
        /// No conflict clause: the row was read as absent under the same
        /// `BEGIN IMMEDIATE` lock, so a conflict is a defect and the
        /// constraint error is the right report. PostgreSQL cannot hold that
        /// read across statements and swallows the race instead.
        insert_registration = "INSERT INTO processes (
                            process_id, incarnation, registration_fingerprint, originator_id, wake_session_id,
                            identity_kind, identity_label,
                            created_at_ms, updated_at_ms, last_event_sequence,
                            change_seq, status,
                            parent_scope_kind, parent_scope_id, on_parent_end, cancel_requested_at_ms,
                            record_json
                         )
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)";

        /// The live incarnation of `?1`, for the artifact-cleanup
        /// acknowledgement that has to tell a stale incarnation from an
        /// unknown one.
        ///
        /// A standalone read on SQLite, where the whole acknowledgement runs
        /// under one write lock; PostgreSQL folds the same question into the
        /// CTE that deletes the cleanup row.
        select_incarnation = "SELECT incarnation FROM processes WHERE process_id = ?1";

        /// SQLite spells a bound id list `json_each`; PostgreSQL deletes these
        /// rows inside its one-statement prune instead.
        delete_by_ids = "DELETE FROM processes
             WHERE process_id IN (SELECT value FROM json_each(?1))";

        /// How many processes are live.
        ///
        /// `INDEXED BY` is the fork: SQLite's planner is pinned to the partial
        /// index rather than asked to choose it, so a statistics change cannot
        /// silently turn the worklist into a table scan.
        count_live_worklist = "SELECT COUNT(*) FROM processes INDEXED BY idx_processes_live_worklist
     WHERE {{live_process_status(status)}}";

        /// The id the first worklist page is bounded by: the page is pinned to
        /// a snapshot high-water mark so a process registered mid-walk cannot
        /// shift it.
        select_max_worklist_process_id = "SELECT MAX(process_id) FROM processes INDEXED BY idx_processes_live_worklist
     WHERE {{live_process_status(status)}}";

        /// The first `?3` live processes at or below `?1`.
        list_first_worklist_page = "SELECT record_json FROM processes
     INDEXED BY idx_processes_live_worklist
     WHERE {{live_process_status(status)}} AND process_id <= ?1
     ORDER BY process_id ASC LIMIT ?3";

        /// The next `?3` live processes in `(?2, ?1]`.
        list_next_worklist_page = "SELECT record_json FROM processes
     INDEXED BY idx_processes_live_worklist
     WHERE {{live_process_status(status)}}
       AND process_id <= ?1 AND process_id > ?2
     ORDER BY process_id ASC LIMIT ?3";

        /// Children of ended parent scope `?1` / `?2` that still owe a cancel:
        /// after `?3`, at most `?4`.
        ///
        /// The predicate is exactly `idx_processes_parent_end_pending`.
        /// PostgreSQL's twin casts its cursor parameter, which is the fork.
        list_parent_end_children = "SELECT record_json FROM processes
     WHERE parent_scope_kind = ?1
       AND parent_scope_id = ?2
       AND on_parent_end = 'cancel'
       AND cancel_requested_at_ms IS NULL
       AND {{live_process_status(status)}}
       AND (?3 IS NULL OR process_id > ?3)
     ORDER BY process_id ASC
     LIMIT ?4";

        /// Turn scopes with live `Cancel` children and no ledger row yet:
        /// after `?1`, at most `?2`.
        list_unrecorded_turn_parents = "SELECT DISTINCT child.parent_scope_id FROM processes AS child
                 WHERE child.parent_scope_kind = 'turn'
                   AND child.on_parent_end = 'cancel'
                   AND child.cancel_requested_at_ms IS NULL
                   AND {{live_process_status(child.status)}}
                   AND NOT EXISTS (
                       SELECT 1 FROM parent_end_plans AS plan
                       WHERE plan.parent_kind = 'turn'
                         AND plan.parent_id = child.parent_scope_id
                   )
                   AND (?1 IS NULL OR child.parent_scope_id > ?1)
                 ORDER BY child.parent_scope_id
                 LIMIT ?2";

        /// Prune candidates: retired rows older than `?1`, at or below change
        /// sequence `?2`, with no wake still owed.
        list_prunable_terminal = "SELECT process_id, record_json FROM processes
             WHERE {{retired_process_status(status)}}
               AND updated_at_ms < ?1
               AND (?2 IS NULL OR change_seq <= ?2)
               AND NOT EXISTS (
                   SELECT 1 FROM process_wake_deliveries AS delivery
                   WHERE delivery.process_id = processes.process_id
                     AND {{undelivered_wake_delivery_state(delivery.state)}}
               )
             ORDER BY process_id ASC";

        /// The change feed after `?1`, at most `?2` rows: live rows unioned
        /// with the tombstones of rows that were pruned.
        list_changes_after = "SELECT change_seq, kind, payload FROM (
                 SELECT change_seq, 'upsert' AS kind, record_json AS payload
                 FROM processes WHERE change_seq > ?1
                 UNION ALL
                 SELECT pruned_change_seq, 'deleted' AS kind,
                        json_object(
                            'process_id', process_id,
                            'incarnation', incarnation,
                            'terminal_label', terminal_label,
                            'pruned_at_ms', pruned_at_ms,
                            'pruned_change_seq', pruned_change_seq
                        ) AS payload
                 FROM process_tombstones WHERE pruned_change_seq > ?1
             )
             ORDER BY change_seq ASC
             LIMIT ?2";

        /// Of the JSON id array `?1`, the ids this registry has never heard
        /// of: neither a row nor a tombstone.
        classify_unregistered_candidates = "SELECT candidate.value
                         FROM json_each(?1) AS candidate
                         WHERE NOT EXISTS (
                             SELECT 1 FROM processes p
                             WHERE p.process_id = candidate.value
                         )
                           AND NOT EXISTS (
                             SELECT 1 FROM process_tombstones t
                             WHERE t.process_id = candidate.value
                         )
                         ORDER BY candidate.key ASC";

        /// Of the JSON id array `?1`, the ids that have been pruned: a
        /// tombstone and no row.
        classify_tombstoned_candidates = "SELECT candidate.value
                         FROM json_each(?1) AS candidate
                         WHERE EXISTS (
                             SELECT 1 FROM process_tombstones t
                             WHERE t.process_id = candidate.value
                         )
                           AND NOT EXISTS (
                             SELECT 1 FROM processes p
                             WHERE p.process_id = candidate.value
                         )
                         ORDER BY candidate.key ASC";

/// Every process matching the always-bound filters.
        list = "SELECT record_json FROM processes
     WHERE (?1 IS NULL OR status IN (SELECT value FROM json_each(?1)))
       AND (?2 IS NULL OR originator_id = ?2)
       AND (?3 IS NULL OR identity_kind = ?3)
       AND (?4 IS NULL OR identity_label = ?4)
       AND (?5 IS NULL OR
            (json_type(record_json, '$.identity.definition.definition') IS NOT NULL
             AND json_type(record_json, '$.identity.definition.definition') = json_type(?5, '$')
             AND (json_type(?5, '$') IN ('null', 'true', 'false')
                  OR json_quote(json_extract(record_json, '$.identity.definition.definition')) IS json(?5))))
       AND (?6 IS NULL OR
            json_extract(record_json, '$.provenance.caused_by.occurrence_id') = ?6)
       AND (?7 IS NULL OR
            json_extract(record_json, '$.provenance.caused_by.subscription_id') = ?7)
       AND (?8 IS NULL OR created_at_ms >= ?8)
       AND (?9 IS NULL OR created_at_ms < ?9)
     ORDER BY process_id ASC";
        /// The same, narrowed to parent scope `?10` / `?11`.
        list_by_parent_scope = "SELECT record_json FROM processes
     WHERE (?1 IS NULL OR status IN (SELECT value FROM json_each(?1)))
       AND (?2 IS NULL OR originator_id = ?2)
       AND (?3 IS NULL OR identity_kind = ?3)
       AND (?4 IS NULL OR identity_label = ?4)
       AND (?5 IS NULL OR
            (json_type(record_json, '$.identity.definition.definition') IS NOT NULL
             AND json_type(record_json, '$.identity.definition.definition') = json_type(?5, '$')
             AND (json_type(?5, '$') IN ('null', 'true', 'false')
                  OR json_quote(json_extract(record_json, '$.identity.definition.definition')) IS json(?5))))
       AND (?6 IS NULL OR
            json_extract(record_json, '$.provenance.caused_by.occurrence_id') = ?6)
       AND (?7 IS NULL OR
            json_extract(record_json, '$.provenance.caused_by.subscription_id') = ?7)
       AND (?8 IS NULL OR created_at_ms >= ?8)
       AND (?9 IS NULL OR created_at_ms < ?9)
           AND parent_scope_kind = ?10
           AND parent_scope_id IS ?11
     ORDER BY process_id ASC";
        /// The same, narrowed to rows whose cancel request is older than `?10`
        /// and whose outcome is still open.
        list_pending_cancel = "SELECT record_json FROM processes
     WHERE (?1 IS NULL OR status IN (SELECT value FROM json_each(?1)))
       AND (?2 IS NULL OR originator_id = ?2)
       AND (?3 IS NULL OR identity_kind = ?3)
       AND (?4 IS NULL OR identity_label = ?4)
       AND (?5 IS NULL OR
            (json_type(record_json, '$.identity.definition.definition') IS NOT NULL
             AND json_type(record_json, '$.identity.definition.definition') = json_type(?5, '$')
             AND (json_type(?5, '$') IN ('null', 'true', 'false')
                  OR json_quote(json_extract(record_json, '$.identity.definition.definition')) IS json(?5))))
       AND (?6 IS NULL OR
            json_extract(record_json, '$.provenance.caused_by.occurrence_id') = ?6)
       AND (?7 IS NULL OR
            json_extract(record_json, '$.provenance.caused_by.subscription_id') = ?7)
       AND (?8 IS NULL OR created_at_ms >= ?8)
       AND (?9 IS NULL OR created_at_ms < ?9)
           AND cancel_requested_at_ms IS NOT NULL
           AND cancel_requested_at_ms < ?10
           AND {{nonterminal_process_status(status)}}
     ORDER BY process_id ASC";
        /// Both narrowings at once.
        list_by_parent_scope_pending_cancel = "SELECT record_json FROM processes
     WHERE (?1 IS NULL OR status IN (SELECT value FROM json_each(?1)))
       AND (?2 IS NULL OR originator_id = ?2)
       AND (?3 IS NULL OR identity_kind = ?3)
       AND (?4 IS NULL OR identity_label = ?4)
       AND (?5 IS NULL OR
            (json_type(record_json, '$.identity.definition.definition') IS NOT NULL
             AND json_type(record_json, '$.identity.definition.definition') = json_type(?5, '$')
             AND (json_type(?5, '$') IN ('null', 'true', 'false')
                  OR json_quote(json_extract(record_json, '$.identity.definition.definition')) IS json(?5))))
       AND (?6 IS NULL OR
            json_extract(record_json, '$.provenance.caused_by.occurrence_id') = ?6)
       AND (?7 IS NULL OR
            json_extract(record_json, '$.provenance.caused_by.subscription_id') = ?7)
       AND (?8 IS NULL OR created_at_ms >= ?8)
       AND (?9 IS NULL OR created_at_ms < ?9)
           AND parent_scope_kind = ?10
           AND parent_scope_id IS ?11
           AND cancel_requested_at_ms IS NOT NULL
           AND cancel_requested_at_ms < ?12
           AND {{nonterminal_process_status(status)}}
     ORDER BY process_id ASC";
        /// Every live process plus those retired since `?10`.
        ///
        /// The union is the point: each arm is planned on its own partial
        /// index, where one disjunction over both would be planned on neither.
        list_recent_retired = "SELECT record_json FROM (
         SELECT process_id, record_json FROM processes
         WHERE {{live_process_status(status)}}
           AND (?1 IS NULL OR status IN (SELECT value FROM json_each(?1)))
           AND (?2 IS NULL OR originator_id = ?2)
           AND (?3 IS NULL OR identity_kind = ?3)
           AND (?4 IS NULL OR identity_label = ?4)
           AND (?5 IS NULL OR
                (json_type(record_json, '$.identity.definition.definition') IS NOT NULL
                 AND json_type(record_json, '$.identity.definition.definition') = json_type(?5, '$')
                 AND (json_type(?5, '$') IN ('null', 'true', 'false')
                      OR json_quote(json_extract(record_json, '$.identity.definition.definition')) IS json(?5))))
           AND (?6 IS NULL OR
                json_extract(record_json, '$.provenance.caused_by.occurrence_id') = ?6)
           AND (?7 IS NULL OR
                json_extract(record_json, '$.provenance.caused_by.subscription_id') = ?7)
           AND (?8 IS NULL OR created_at_ms >= ?8)
           AND (?9 IS NULL OR created_at_ms < ?9)
         UNION ALL
         SELECT process_id, record_json FROM processes
         WHERE {{retired_process_status(status)}}
           AND updated_at_ms >= ?10
           AND (?1 IS NULL OR status IN (SELECT value FROM json_each(?1)))
           AND (?2 IS NULL OR originator_id = ?2)
           AND (?3 IS NULL OR identity_kind = ?3)
           AND (?4 IS NULL OR identity_label = ?4)
           AND (?5 IS NULL OR
                (json_type(record_json, '$.identity.definition.definition') IS NOT NULL
                 AND json_type(record_json, '$.identity.definition.definition') = json_type(?5, '$')
                 AND (json_type(?5, '$') IN ('null', 'true', 'false')
                      OR json_quote(json_extract(record_json, '$.identity.definition.definition')) IS json(?5))))
           AND (?6 IS NULL OR
                json_extract(record_json, '$.provenance.caused_by.occurrence_id') = ?6)
           AND (?7 IS NULL OR
                json_extract(record_json, '$.provenance.caused_by.subscription_id') = ?7)
           AND (?8 IS NULL OR created_at_ms >= ?8)
           AND (?9 IS NULL OR created_at_ms < ?9)
     ) ORDER BY process_id ASC";
        /// The same, narrowed to parent scope `?11` / `?12`.
        list_recent_retired_by_parent_scope = "SELECT record_json FROM (
         SELECT process_id, record_json FROM processes
         WHERE {{live_process_status(status)}}
           AND (?1 IS NULL OR status IN (SELECT value FROM json_each(?1)))
           AND (?2 IS NULL OR originator_id = ?2)
           AND (?3 IS NULL OR identity_kind = ?3)
           AND (?4 IS NULL OR identity_label = ?4)
           AND (?5 IS NULL OR
                (json_type(record_json, '$.identity.definition.definition') IS NOT NULL
                 AND json_type(record_json, '$.identity.definition.definition') = json_type(?5, '$')
                 AND (json_type(?5, '$') IN ('null', 'true', 'false')
                      OR json_quote(json_extract(record_json, '$.identity.definition.definition')) IS json(?5))))
           AND (?6 IS NULL OR
                json_extract(record_json, '$.provenance.caused_by.occurrence_id') = ?6)
           AND (?7 IS NULL OR
                json_extract(record_json, '$.provenance.caused_by.subscription_id') = ?7)
           AND (?8 IS NULL OR created_at_ms >= ?8)
           AND (?9 IS NULL OR created_at_ms < ?9)
           AND parent_scope_kind = ?11
           AND parent_scope_id IS ?12
         UNION ALL
         SELECT process_id, record_json FROM processes
         WHERE {{retired_process_status(status)}}
           AND updated_at_ms >= ?10
           AND (?1 IS NULL OR status IN (SELECT value FROM json_each(?1)))
           AND (?2 IS NULL OR originator_id = ?2)
           AND (?3 IS NULL OR identity_kind = ?3)
           AND (?4 IS NULL OR identity_label = ?4)
           AND (?5 IS NULL OR
                (json_type(record_json, '$.identity.definition.definition') IS NOT NULL
                 AND json_type(record_json, '$.identity.definition.definition') = json_type(?5, '$')
                 AND (json_type(?5, '$') IN ('null', 'true', 'false')
                      OR json_quote(json_extract(record_json, '$.identity.definition.definition')) IS json(?5))))
           AND (?6 IS NULL OR
                json_extract(record_json, '$.provenance.caused_by.occurrence_id') = ?6)
           AND (?7 IS NULL OR
                json_extract(record_json, '$.provenance.caused_by.subscription_id') = ?7)
           AND (?8 IS NULL OR created_at_ms >= ?8)
           AND (?9 IS NULL OR created_at_ms < ?9)
           AND parent_scope_kind = ?11
           AND parent_scope_id IS ?12
     ) ORDER BY process_id ASC";
        /// The same, narrowed to rows whose cancel request is older than `?11`
        /// and whose outcome is still open.
        list_recent_retired_pending_cancel = "SELECT record_json FROM (
         SELECT process_id, record_json FROM processes
         WHERE {{live_process_status(status)}}
           AND (?1 IS NULL OR status IN (SELECT value FROM json_each(?1)))
           AND (?2 IS NULL OR originator_id = ?2)
           AND (?3 IS NULL OR identity_kind = ?3)
           AND (?4 IS NULL OR identity_label = ?4)
           AND (?5 IS NULL OR
                (json_type(record_json, '$.identity.definition.definition') IS NOT NULL
                 AND json_type(record_json, '$.identity.definition.definition') = json_type(?5, '$')
                 AND (json_type(?5, '$') IN ('null', 'true', 'false')
                      OR json_quote(json_extract(record_json, '$.identity.definition.definition')) IS json(?5))))
           AND (?6 IS NULL OR
                json_extract(record_json, '$.provenance.caused_by.occurrence_id') = ?6)
           AND (?7 IS NULL OR
                json_extract(record_json, '$.provenance.caused_by.subscription_id') = ?7)
           AND (?8 IS NULL OR created_at_ms >= ?8)
           AND (?9 IS NULL OR created_at_ms < ?9)
           AND cancel_requested_at_ms IS NOT NULL
           AND cancel_requested_at_ms < ?11
           AND {{nonterminal_process_status(status)}}
         UNION ALL
         SELECT process_id, record_json FROM processes
         WHERE {{retired_process_status(status)}}
           AND updated_at_ms >= ?10
           AND (?1 IS NULL OR status IN (SELECT value FROM json_each(?1)))
           AND (?2 IS NULL OR originator_id = ?2)
           AND (?3 IS NULL OR identity_kind = ?3)
           AND (?4 IS NULL OR identity_label = ?4)
           AND (?5 IS NULL OR
                (json_type(record_json, '$.identity.definition.definition') IS NOT NULL
                 AND json_type(record_json, '$.identity.definition.definition') = json_type(?5, '$')
                 AND (json_type(?5, '$') IN ('null', 'true', 'false')
                      OR json_quote(json_extract(record_json, '$.identity.definition.definition')) IS json(?5))))
           AND (?6 IS NULL OR
                json_extract(record_json, '$.provenance.caused_by.occurrence_id') = ?6)
           AND (?7 IS NULL OR
                json_extract(record_json, '$.provenance.caused_by.subscription_id') = ?7)
           AND (?8 IS NULL OR created_at_ms >= ?8)
           AND (?9 IS NULL OR created_at_ms < ?9)
           AND cancel_requested_at_ms IS NOT NULL
           AND cancel_requested_at_ms < ?11
           AND {{nonterminal_process_status(status)}}
     ) ORDER BY process_id ASC";
        /// Both narrowings at once.
        list_recent_retired_by_parent_scope_pending_cancel = "SELECT record_json FROM (
         SELECT process_id, record_json FROM processes
         WHERE {{live_process_status(status)}}
           AND (?1 IS NULL OR status IN (SELECT value FROM json_each(?1)))
           AND (?2 IS NULL OR originator_id = ?2)
           AND (?3 IS NULL OR identity_kind = ?3)
           AND (?4 IS NULL OR identity_label = ?4)
           AND (?5 IS NULL OR
                (json_type(record_json, '$.identity.definition.definition') IS NOT NULL
                 AND json_type(record_json, '$.identity.definition.definition') = json_type(?5, '$')
                 AND (json_type(?5, '$') IN ('null', 'true', 'false')
                      OR json_quote(json_extract(record_json, '$.identity.definition.definition')) IS json(?5))))
           AND (?6 IS NULL OR
                json_extract(record_json, '$.provenance.caused_by.occurrence_id') = ?6)
           AND (?7 IS NULL OR
                json_extract(record_json, '$.provenance.caused_by.subscription_id') = ?7)
           AND (?8 IS NULL OR created_at_ms >= ?8)
           AND (?9 IS NULL OR created_at_ms < ?9)
           AND parent_scope_kind = ?11
           AND parent_scope_id IS ?12
           AND cancel_requested_at_ms IS NOT NULL
           AND cancel_requested_at_ms < ?13
           AND {{nonterminal_process_status(status)}}
         UNION ALL
         SELECT process_id, record_json FROM processes
         WHERE {{retired_process_status(status)}}
           AND updated_at_ms >= ?10
           AND (?1 IS NULL OR status IN (SELECT value FROM json_each(?1)))
           AND (?2 IS NULL OR originator_id = ?2)
           AND (?3 IS NULL OR identity_kind = ?3)
           AND (?4 IS NULL OR identity_label = ?4)
           AND (?5 IS NULL OR
                (json_type(record_json, '$.identity.definition.definition') IS NOT NULL
                 AND json_type(record_json, '$.identity.definition.definition') = json_type(?5, '$')
                 AND (json_type(?5, '$') IN ('null', 'true', 'false')
                      OR json_quote(json_extract(record_json, '$.identity.definition.definition')) IS json(?5))))
           AND (?6 IS NULL OR
                json_extract(record_json, '$.provenance.caused_by.occurrence_id') = ?6)
           AND (?7 IS NULL OR
                json_extract(record_json, '$.provenance.caused_by.subscription_id') = ?7)
           AND (?8 IS NULL OR created_at_ms >= ?8)
           AND (?9 IS NULL OR created_at_ms < ?9)
           AND parent_scope_kind = ?11
           AND parent_scope_id IS ?12
           AND cancel_requested_at_ms IS NOT NULL
           AND cancel_requested_at_ms < ?13
           AND {{nonterminal_process_status(status)}}
     ) ORDER BY process_id ASC";
    }
}

lash_store_sql::statements! {
    /// Registry-wide statements only SQLite issues.
    pub(crate) struct ProcessRegistrySqliteStatements @ "process_registry" {
        /// Session `?1`'s observed processes, filtered by the JSON status
        /// array `?2` and, when `?3` is bound, by retirement recency.
        list_observed = "SELECT p.record_json
                     FROM process_observers o
                     JOIN processes p ON p.process_id = o.process_id
                                     AND p.incarnation = o.process_incarnation
                     WHERE o.session_id = ?1
                       AND (?2 IS NULL OR p.status IN (SELECT value FROM json_each(?2)))
                       AND (?3 IS NULL OR {{live_process_status(p.status)}}
                            OR p.updated_at_ms >= ?3)
                     ORDER BY p.process_id";

        /// The same question when `?3` is always bound, split into a live arm
        /// and a recently-retired arm.
        ///
        /// The union is the fork and the point: SQLite plans each arm on its
        /// own partial index, where the single-statement form above disjoins
        /// them and plans neither.
        list_observed_recent_retired = "SELECT record_json FROM (
         SELECT process_id, record_json FROM processes p
         WHERE {{live_process_status(p.status)}}
           AND (?2 IS NULL OR p.status IN (SELECT value FROM json_each(?2)))
           AND EXISTS (SELECT 1 FROM process_observers o
                       WHERE o.session_id = ?1 AND o.process_id = p.process_id
                         AND o.process_incarnation = p.incarnation)
         UNION ALL
         SELECT process_id, record_json FROM processes p
         WHERE {{retired_process_status(p.status)}} AND p.updated_at_ms >= ?3
           AND (?2 IS NULL OR p.status IN (SELECT value FROM json_each(?2)))
           AND EXISTS (SELECT 1 FROM process_observers o
                       WHERE o.session_id = ?1 AND o.process_id = p.process_id
                         AND o.process_incarnation = p.incarnation)
     ) ORDER BY process_id";
    }
}

lash_store_sql::statements! {
    /// `process_observers` statements only SQLite issues.
    pub(crate) struct ObserverSqliteStatements @ "process_observer" {
        /// `INSERT OR IGNORE` is SQLite's spelling of PostgreSQL's `ON CONFLICT DO NOTHING`.
        insert_if_absent = "INSERT OR IGNORE INTO process_observers (session_id, process_id, process_incarnation)
                             VALUES (?1, ?2, ?3)";

        /// A standalone read on SQLite, where the caller's other half of the
        /// question runs under the same lock; PostgreSQL asks both halves in
        /// one statement because it cannot.
        exists = "SELECT EXISTS(
                         SELECT 1 FROM process_observers
                         WHERE session_id = ?1 AND process_id = ?2
                     )";

        /// The sessions observing incarnation `?1` / `?2`.
        ///
        /// Narrowed to the incarnation the caller read; PostgreSQL's twin
        /// reports every incarnation's observers.
        list_sessions_for_incarnation = "SELECT session_id FROM process_observers
                             WHERE process_id = ?1 AND process_incarnation = ?2
                             ORDER BY session_id";

        /// SQLite deletes the dependent rows itself; PostgreSQL's prune
        /// statement lets the foreign key cascade do it.
        delete_by_process_ids = "DELETE FROM process_observers
                 WHERE process_id IN (SELECT value FROM json_each(?1))";
    }
}

lash_store_sql::statements! {
    /// `process_leases` statements only SQLite issues.
    pub(crate) struct LeaseSqliteStatements @ "process_lease" {
        /// The retained fencing token of `?1`.
        ///
        /// No lock suffix: the whole claim decision runs under
        /// `BEGIN IMMEDIATE`. PostgreSQL takes the row's write lock here.
        select_fencing_token = "SELECT lease_fencing_token FROM process_leases WHERE process_id = ?1";

        /// `?1`'s lease. Same lock fork as
        /// [`LeaseSqliteStatements::select_fencing_token`].
        select_by_process = "SELECT lease_owner_id, lease_token, lease_fencing_token,
                    lease_claimed_at_ms, lease_expires_at_ms,
                    lease_owner_incarnation_id
             FROM process_leases
             WHERE process_id = ?1";

        /// The leases of every process named by the JSON id array `?1`.
        list_by_process_ids = "SELECT process_id, lease_owner_id, lease_token,
                                lease_fencing_token, lease_claimed_at_ms,
                                lease_expires_at_ms, lease_owner_incarnation_id
                         FROM process_leases
                         WHERE process_id IN (SELECT value FROM json_each(?1))";

        /// Take the lease of `?1` at fencing token `?5`.
        ///
        /// `excluded` is SQLite's spelling of the proposed row; PostgreSQL
        /// spells it `EXCLUDED`.
        upsert_acquired = "INSERT INTO process_leases (
                process_id, lease_owner_id, lease_owner_incarnation_id,
                lease_token, lease_fencing_token,
                lease_claimed_at_ms, lease_expires_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(process_id) DO UPDATE SET
                lease_owner_id = excluded.lease_owner_id,
                lease_owner_incarnation_id = excluded.lease_owner_incarnation_id,
                lease_token = excluded.lease_token,
                lease_fencing_token = excluded.lease_fencing_token,
                lease_claimed_at_ms = excluded.lease_claimed_at_ms,
                lease_expires_at_ms = excluded.lease_expires_at_ms";

        /// Drop the leases of every process named by the JSON id array `?1`.
        delete_by_process_ids = "DELETE FROM process_leases
                 WHERE process_id IN (SELECT value FROM json_each(?1))";
    }
}

lash_store_sql::statements! {
    /// `process_change_clock` statements only SQLite issues.
    ///
    /// Every one of them forks: the singleton flag is `INTEGER 1` here and
    /// `BOOLEAN TRUE` on PostgreSQL, and the bump-then-read pair is two
    /// statements under this store's write lock where PostgreSQL uses one
    /// `RETURNING`.
    pub(crate) struct ChangeClockSqliteStatements @ "process_change_clock" {
        /// Advance the change sequence by one.
        bump = "UPDATE process_change_clock
             SET current_seq = current_seq + 1
             WHERE singleton = 1";

        /// Advance the change sequence by `?1`, for a prune allocating one
        /// sequence per tombstone.
        bump_by = "UPDATE process_change_clock
         SET current_seq = current_seq + ?1
         WHERE singleton = 1";

        /// The current change sequence.
        select_current = "SELECT current_seq FROM process_change_clock WHERE singleton = 1";

        /// How far tombstone compaction has run: the floor a change cursor
        /// may not be older than.
        select_compaction_horizon = "SELECT tombstone_compaction_horizon FROM process_change_clock WHERE singleton = 1";

        /// Raise the compaction horizon to `?1`, never lowering it.
        raise_compaction_horizon = "UPDATE process_change_clock
             SET tombstone_compaction_horizon = MAX(
                 tombstone_compaction_horizon, ?1
             )
             WHERE singleton = 1";
    }
}

lash_store_sql::statements! {
    /// `process_tombstones` statements only SQLite issues.
    pub(crate) struct TombstoneSqliteStatements @ "process_tombstone" {
        /// Tombstone every process named by the JSON id array `?1`, stamped
        /// `?2`, from change sequence `?3` upwards.
        ///
        /// `json_each` preserves the candidate array's zero-based order, so
        /// the tombstones' change sequences retain process-id ordering without
        /// one clock update per process.
        insert_from_pruned = "INSERT INTO process_tombstones (
             process_id, incarnation, terminal_label, pruned_at_ms, pruned_change_seq
         )
         SELECT process.process_id,
                process.incarnation,
                process.status,
                ?2,
                ?3 + CAST(candidate.key AS INTEGER)
         FROM json_each(?1) AS candidate
         JOIN processes AS process ON process.process_id = candidate.value
         ORDER BY CAST(candidate.key AS INTEGER)";

        /// The highest change sequence compaction may advance to: tombstones
        /// older than `?1`, at or below `?2`, not named by the JSON id array
        /// `?3`, and with no artifact cleanup still owed.
        select_max_compactable_change_seq = "SELECT MAX(pruned_change_seq) FROM process_tombstones
             WHERE pruned_at_ms < ?1
               AND (?2 IS NULL OR pruned_change_seq <= ?2)
               AND process_id NOT IN (SELECT value FROM json_each(?3))
               AND NOT EXISTS (
                   SELECT 1 FROM process_artifact_cleanup AS cleanup
                   WHERE cleanup.process_id = process_tombstones.process_id
                     AND cleanup.incarnation = process_tombstones.incarnation
               )";

        /// Delete exactly the rows
        /// [`TombstoneSqliteStatements::select_max_compactable_change_seq`]
        /// measured.
        delete_compactable = "DELETE FROM process_tombstones
         WHERE pruned_at_ms < ?1
           AND (?2 IS NULL OR pruned_change_seq <= ?2)
           AND process_id NOT IN (SELECT value FROM json_each(?3))
           AND NOT EXISTS (
               SELECT 1 FROM process_artifact_cleanup AS cleanup
               WHERE cleanup.process_id = process_tombstones.process_id
                 AND cleanup.incarnation = process_tombstones.incarnation
           )";
    }
}

lash_store_sql::statements! {
    /// `process_artifact_cleanup` statements only SQLite issues.
    pub(crate) struct ArtifactCleanupSqliteStatements @ "process_artifact_cleanup" {
        /// Record the artifact release owed for pruned incarnation `?1` / `?2`.
        ///
        /// A plain insert per candidate here; PostgreSQL writes the same rows
        /// from the one statement that performs its whole prune.
        insert = "INSERT INTO process_artifact_cleanup (process_id, incarnation, cleanup_json)
             VALUES (?1, ?2, ?3)";

        /// Acknowledge the release owed for `?1` / `?2`.
        delete_for_incarnation = "DELETE FROM process_artifact_cleanup
                     WHERE process_id = ?1 AND incarnation = ?2";
    }
}

lash_store_sql::statements! {
    /// `process_events` statements only SQLite issues.
    pub(crate) struct EventSqliteStatements @ "process_event" {
        delete_by_process_ids = "DELETE FROM process_events
             WHERE process_id IN (SELECT value FROM json_each(?1))";
    }
}

lash_store_sql::statements! {
    /// `process_segment_handovers` statements only SQLite issues.
    pub(crate) struct SegmentHandoverSqliteStatements @ "process_segment_handover" {
        /// Park the handover of `?1` at segment `?2`.
        ///
        /// A plain insert: the caller read the ordinal's absence under the
        /// same write lock and refuses a conflicting handover itself, where
        /// PostgreSQL has to express that refusal as a conflict clause.
        insert = "INSERT INTO process_segment_handovers
                         (process_id, segment_ordinal, handover_json) VALUES (?1, ?2, ?3)";

        delete_by_process_ids = "DELETE FROM process_segment_handovers
                 WHERE process_id IN (SELECT value FROM json_each(?1))";

        /// The parked-continuation page of the preflight walk: after cursor
        /// `?1`, at most `?2`.
        ///
        /// The keyset expression is computed once in the projection and used
        /// for both the resume filter and the ordering, so the two cannot
        /// disagree. PostgreSQL compares the two ordered columns as a row
        /// value instead, which is the fork.
        list_parked_segments = "WITH parked AS (
    SELECT
        handovers.process_id || ':' || printf('%020d', handovers.segment_ordinal) AS walk_cursor,
        handovers.process_id    AS process_id,
        handovers.handover_json AS handover_json,
        processes.status          AS status,
        processes.wake_session_id AS wake_session_id,
        processes.record_json     AS record_json
    FROM process_segment_handovers AS handovers
    JOIN processes ON processes.process_id = handovers.process_id
    WHERE {{live_process_status(processes.status)}}
)
SELECT walk_cursor, process_id, handover_json, status, wake_session_id, record_json
FROM parked
WHERE ?1 IS NULL OR walk_cursor > ?1
ORDER BY walk_cursor
LIMIT ?2";
    }
}

lash_store_sql::statements! {
    /// `process_wake_deliveries` statements only SQLite issues.
    pub(crate) struct WakeDeliverySqliteStatements @ "process_wake_delivery" {
        /// `INSERT OR IGNORE` is SQLite's spelling of PostgreSQL's
        /// `ON CONFLICT (delivery_id) DO NOTHING`.
        insert_pending = "INSERT OR IGNORE INTO process_wake_deliveries (
                delivery_id, process_id, process_incarnation, target_session_id, sequence, state,
                claim_token, attempts, first_attempt_ms, next_attempt_at_ms, expires_at_ms,
                discard_reason, delivery_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, {{pending_wake_delivery_state_value(state)}}, NULL, 0, NULL, ?6, ?7, NULL, ?8)";

        /// The next `?3` claimable wakes at `?1`, skipping any whose ordering
        /// group still holds an earlier delivery that blocks it. `?2` is the
        /// JSON array of discard reasons that do not block.
        ///
        /// No lock suffix and no `SKIP LOCKED`: `BEGIN IMMEDIATE` already
        /// serialises claimants. PostgreSQL needs both, and spells the
        /// non-blocking reason list as an array parameter.
        select_claimable = "SELECT candidate.delivery_id
                             FROM process_wake_deliveries AS candidate
                             WHERE {{pending_wake_delivery_state(candidate.state)}}
                               AND candidate.next_attempt_at_ms <= ?1
                               AND NOT EXISTS (
                                   SELECT 1
                                   FROM process_wake_deliveries AS earlier
                                   WHERE {{not_enqueued_wake_delivery_state(earlier.state)}}
                                     AND NOT (
                                         {{discarded_wake_delivery_state(earlier.state)}}
                                         AND (
                                             earlier.discard_reason IS NULL
                                             OR earlier.discard_reason IN (
                                                 SELECT value FROM json_each(?2)
                                             )
                                         )
                                     )
                                     AND earlier.target_session_id = candidate.target_session_id
                                     AND earlier.process_id = candidate.process_id
                                     AND earlier.sequence < candidate.sequence
                               )
                             ORDER BY candidate.next_attempt_at_ms ASC,
                                      candidate.target_session_id ASC,
                                      candidate.process_id ASC,
                                      candidate.sequence ASC
                             LIMIT ?3";

        /// Delivery `?1`, whole.
        ///
        /// Keyed by the caller's own binding, so the key is not read back;
        /// PostgreSQL reports it because the same projection also serves its
        /// unkeyed listings.
        select_report = "SELECT state, claim_token, attempts, first_attempt_ms, next_attempt_at_ms,
                    expires_at_ms, discard_reason, delivery_json
             FROM process_wake_deliveries WHERE delivery_id = ?1";

        /// Every delivery id.
        ///
        /// SQLite lists the ids and reads each delivery back through
        /// [`WakeDeliverySqliteStatements::select_report`]; PostgreSQL reports
        /// the rows themselves.
        list_delivery_ids = "SELECT delivery_id FROM process_wake_deliveries ORDER BY delivery_id ASC";

        /// Every delivery id in state `?1`.
        list_delivery_ids_by_state = "SELECT delivery_id FROM process_wake_deliveries WHERE state = ?1 ORDER BY delivery_id ASC";

        /// The undelivered-wake page of the preflight walk: after `?1`, at
        /// most `?2`. PostgreSQL casts its cursor parameter, which is the
        /// fork.
        list_undelivered_for_walk = "SELECT delivery_id, process_id, target_session_id, state, delivery_json
FROM process_wake_deliveries
WHERE {{undelivered_wake_delivery_state(state)}}
  AND (?1 IS NULL OR delivery_id > ?1)
ORDER BY delivery_id
LIMIT ?2";
    }
}

lash_store_sql::statements! {
    /// `parent_end_plans` statements only SQLite issues.
    pub(crate) struct ParentEndPlanSqliteStatements @ "parent_end_plan" {
        /// Reclaim settled plans older than `?1` that no live child still
        /// names.
        delete_reclaimable = "DELETE FROM parent_end_plans
         WHERE settled_at_ms IS NOT NULL
           AND settled_at_ms < ?1
           AND NOT EXISTS (
               SELECT 1 FROM processes AS child
               WHERE child.parent_scope_kind = parent_end_plans.parent_kind
                 AND child.parent_scope_id = parent_end_plans.parent_id
                 AND {{live_process_status(child.status)}}
           )";
    }
}

lash_store_sql::statements! {
    /// `wake_allocation_floors` statements only SQLite issues.
    pub(crate) struct WakeAllocationFloorSqliteStatements @ "wake_allocation_floor" {
        /// Raise session `?1`'s floor for process `?2` to `?3`, never lowering
        /// it. `MAX` over `excluded` is SQLite's spelling of PostgreSQL's
        /// `GREATEST` over `EXCLUDED`.
        upsert_max = "INSERT INTO wake_allocation_floors (
                target_session_id, process_id, allocation_floor
             ) VALUES (?1, ?2, ?3)
             ON CONFLICT (target_session_id, process_id) DO UPDATE SET
                allocation_floor = MAX(
                    wake_allocation_floors.allocation_floor,
                    excluded.allocation_floor
                )";
    }
}

lash_store_sql::statements! {
    /// `wake_redelivery_fences` statements only SQLite issues.
    pub(crate) struct WakeRedeliveryFenceSqliteStatements @ "wake_redelivery_fence" {
        /// Raise session `?1`'s consumed floor from the wakes in the claimed
        /// batch `?2` / `?3` / `?4`.
        ///
        /// SQLite derives the floors from the queued rows it is committing, in
        /// the same statement; PostgreSQL's commit path has already decoded
        /// them and binds the values.
        insert_from_claimed_batch = "INSERT INTO wake_redelivery_fences (
                 session_id, process_id, allocation_floor
             )
             SELECT batch.session_id,
                    json_extract(item.payload_json, '$.wake.process_id'),
                    json_extract(item.payload_json, '$.wake.sequence')
             FROM queued_work_batches AS batch
             JOIN queued_work_items AS item ON item.batch_id = batch.batch_id
             WHERE batch.session_id = ?1
               AND batch.batch_id = ?2
               AND batch.claim_id = ?3
               AND batch.claim_token = ?4
               AND json_extract(item.payload_json, '$.type') = 'process_wake'
             ON CONFLICT(session_id, process_id) DO UPDATE SET
                 allocation_floor = MAX(
                     wake_redelivery_fences.allocation_floor,
                     excluded.allocation_floor
                 )";
    }
}

/// Every process-family statement, rendered for one way of addressing the
/// registry's tables.
pub(crate) struct ProcessSql {
    /// `processes` statements both backends issue verbatim.
    pub(crate) process: ProcessStatements,
    /// `processes` statements only SQLite issues.
    pub(crate) process_sqlite: ProcessSqliteStatements,
    /// Registry-wide statements only SQLite issues.
    pub(crate) registry_sqlite: ProcessRegistrySqliteStatements,
    /// `process_definitions` statements, all of them shared.
    pub(crate) definition: DefinitionStatements,
    /// `process_events` statements both backends issue verbatim.
    pub(crate) event: EventStatements,
    /// `process_events` statements only SQLite issues.
    pub(crate) event_sqlite: EventSqliteStatements,
    /// `process_leases` statements both backends issue verbatim.
    pub(crate) lease: LeaseStatements,
    /// `process_leases` statements only SQLite issues.
    pub(crate) lease_sqlite: LeaseSqliteStatements,
    /// `process_observers` statements both backends issue verbatim.
    pub(crate) observer: ObserverStatements,
    /// `process_observers` statements only SQLite issues.
    pub(crate) observer_sqlite: ObserverSqliteStatements,
    /// `process_segment_handovers` statements both backends issue verbatim.
    pub(crate) handover: SegmentHandoverStatements,
    /// `process_segment_handovers` statements only SQLite issues.
    pub(crate) handover_sqlite: SegmentHandoverSqliteStatements,
    /// `process_tombstones` statements both backends issue verbatim.
    pub(crate) tombstone: TombstoneStatements,
    /// `process_tombstones` statements only SQLite issues.
    pub(crate) tombstone_sqlite: TombstoneSqliteStatements,
    /// `process_wake_deliveries` statements both backends issue verbatim.
    pub(crate) wake: WakeDeliveryStatements,
    /// `process_wake_deliveries` statements only SQLite issues.
    pub(crate) wake_sqlite: WakeDeliverySqliteStatements,
    /// `process_change_clock` statements, all of them SQLite's own.
    pub(crate) clock_sqlite: ChangeClockSqliteStatements,
    /// `process_artifact_cleanup` statements both backends issue verbatim.
    pub(crate) cleanup: ArtifactCleanupStatements,
    /// `process_artifact_cleanup` statements only SQLite issues.
    pub(crate) cleanup_sqlite: ArtifactCleanupSqliteStatements,
    /// `parent_end_plans` statements both backends issue verbatim.
    pub(crate) plan: ParentEndPlanStatements,
    /// `parent_end_plans` statements only SQLite issues.
    pub(crate) plan_sqlite: ParentEndPlanSqliteStatements,
    /// `wake_allocation_floors` statements both backends issue verbatim.
    pub(crate) floor: WakeAllocationFloorStatements,
    /// `wake_allocation_floors` statements only SQLite issues.
    pub(crate) floor_sqlite: WakeAllocationFloorSqliteStatements,
    /// `wake_redelivery_fences` statements both backends issue verbatim.
    pub(crate) fence: WakeRedeliveryFenceStatements,
    /// `wake_redelivery_fences` statements only SQLite issues.
    pub(crate) fence_sqlite: WakeRedeliveryFenceSqliteStatements,
}

impl ProcessSql {
    fn render(dialect: Dialect) -> Self {
        let dialect = dialect.with_vocabulary(PROCESS_LIFECYCLE);
        Self {
            process: ProcessStatements::render(dialect),
            process_sqlite: ProcessSqliteStatements::render(dialect),
            registry_sqlite: ProcessRegistrySqliteStatements::render(dialect),
            definition: DefinitionStatements::render(dialect),
            event: EventStatements::render(dialect),
            event_sqlite: EventSqliteStatements::render(dialect),
            lease: LeaseStatements::render(dialect),
            lease_sqlite: LeaseSqliteStatements::render(dialect),
            observer: ObserverStatements::render(dialect),
            observer_sqlite: ObserverSqliteStatements::render(dialect),
            handover: SegmentHandoverStatements::render(dialect),
            handover_sqlite: SegmentHandoverSqliteStatements::render(dialect),
            tombstone: TombstoneStatements::render(dialect),
            tombstone_sqlite: TombstoneSqliteStatements::render(dialect),
            wake: WakeDeliveryStatements::render(dialect),
            wake_sqlite: WakeDeliverySqliteStatements::render(dialect),
            clock_sqlite: ChangeClockSqliteStatements::render(dialect),
            cleanup: ArtifactCleanupStatements::render(dialect),
            cleanup_sqlite: ArtifactCleanupSqliteStatements::render(dialect),
            plan: ParentEndPlanStatements::render(dialect),
            plan_sqlite: ParentEndPlanSqliteStatements::render(dialect),
            floor: WakeAllocationFloorStatements::render(dialect),
            floor_sqlite: WakeAllocationFloorSqliteStatements::render(dialect),
            fence: WakeRedeliveryFenceStatements::render(dialect),
            fence_sqlite: WakeRedeliveryFenceSqliteStatements::render(dialect),
        }
    }
}

static PROCESS_SQL: LazyLock<ProcessSql> =
    LazyLock::new(|| ProcessSql::render(Dialect::sqlite_unqualified()));

static ATTACHED_PROCESS_SQL: LazyLock<ProcessSql> =
    LazyLock::new(|| ProcessSql::render(Schema::ProcessRegistry.dialect()));

/// The process-family statements as the registry's own connection addresses
/// them, rendered once at first use and never again.
pub(crate) fn process_sql() -> &'static ProcessSql {
    &PROCESS_SQL
}

/// The same statements as a connection that has `ATTACH`ed the registry
/// addresses them.
pub(crate) fn attached_process_sql() -> &'static ProcessSql {
    &ATTACHED_PROCESS_SQL
}

/// The list statement this filter asks for, and the values it binds.
///
/// The optional clauses are conjuncts that are either present or absent — an
/// `(?n IS NULL OR …)` over them would cost the planner the partial indexes
/// they exist to use — so each combination is its own named statement rather
/// than a template with a hole. Choosing the statement and pushing the values
/// in one place is what keeps a clause and the parameter it reads together.
pub(crate) fn list_processes_query(
    filter: &lash_core::ProcessListFilter,
    status: Option<String>,
    definition: Option<String>,
) -> (&'static str, Vec<rusqlite::types::Value>) {
    use rusqlite::types::Value;

    fn text(value: Option<String>) -> Value {
        value.map_or(Value::Null, Value::Text)
    }
    fn integer(value: Option<u64>) -> Value {
        value.map_or(Value::Null, |value| {
            Value::Integer(crate::clamp_epoch_ms(value))
        })
    }

    let mut values = vec![
        text(status),
        text(filter.originator.as_ref().map(|o| o.originator_id())),
        text(filter.identity_kind.clone()),
        text(filter.identity_label.clone()),
        text(definition),
        text(filter.caused_by_occurrence_id.clone()),
        text(filter.caused_by_subscription_id.clone()),
        integer(filter.created_at_start_ms),
        integer(filter.created_at_end_ms),
    ];
    let retired = filter.retired_since_ms.is_some();
    if retired {
        values.push(integer(filter.retired_since_ms));
    }
    if let Some(parent) = &filter.parent_scope {
        values.push(Value::Text(parent.storage_kind().to_string()));
        // `IS` rather than `=`: a Host scope stores a NULL id, and the check
        // constraint ties that NULL to the kind, so the pair is still an
        // equality lookup on `idx_processes_parent_scope`.
        values.push(text(parent.storage_id()));
    }
    if let Some(before_ms) = filter.cancel_pending_before_ms {
        values.push(integer(Some(before_ms)));
    }

    let statements = &process_sql().process_sqlite;
    let sql = match (
        retired,
        filter.parent_scope.is_some(),
        filter.cancel_pending_before_ms.is_some(),
    ) {
        (false, false, false) => statements.list.sql(),
        (false, true, false) => statements.list_by_parent_scope.sql(),
        (false, false, true) => statements.list_pending_cancel.sql(),
        (false, true, true) => statements.list_by_parent_scope_pending_cancel.sql(),
        (true, false, false) => statements.list_recent_retired.sql(),
        (true, true, false) => statements.list_recent_retired_by_parent_scope.sql(),
        (true, false, true) => statements.list_recent_retired_pending_cancel.sql(),
        (true, true, true) => statements
            .list_recent_retired_by_parent_scope_pending_cancel
            .sql(),
    };
    (sql, values)
}
