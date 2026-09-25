//! Every process-family statement this store issues, rendered once.
//!
//! The shared halves live in `lash_store_sql::process`; the sets below are the
//! statements only PostgreSQL issues, and each one has a `[[dialect_only]]`
//! entry in `crates/lash-store-sql/dialect-only.toml` saying why its text
//! forks. Most of the forks are one of four things: a `FOR UPDATE` or
//! `FOR SHARE` lock suffix that SQLite does not need under `BEGIN IMMEDIATE`,
//! an `ON CONFLICT` clause that detects a race SQLite's write lock makes
//! unreachable, an array parameter where SQLite binds a JSON list, and the
//! `BOOLEAN TRUE` singleton flag where SQLite writes `INTEGER 1`.

use std::sync::LazyLock;

use lash_core_execution::WakeDeliveryState;
use lash_core_execution::store_backend_support as vocabulary;
use lash_store_sql::process::{
    artifact_cleanup::ArtifactCleanupStatements, definitions::DefinitionStatements,
    events::EventStatements, leases::LeaseStatements, observers::ObserverStatements,
    parent_end_plans::ParentEndPlanStatements, park_events::ProcessParkEventStatements,
    processes::ProcessStatements, segment_handovers::SegmentHandoverStatements,
    tombstones::TombstoneStatements, wake_allocation_floors::WakeAllocationFloorStatements,
    wake_deliveries::WakeDeliveryStatements, wake_redelivery_fences::WakeRedeliveryFenceStatements,
};
use lash_store_sql::{Dialect, Vocabulary, VocabularyTerm};

/// `<column> = '<state>'`, for the one wake-delivery state named.
///
/// The label still comes from [`WakeDeliveryState`] through
/// `wake_delivery_state_sql_literal`; these wrappers only put a column and an
/// operator around it, so the vocabulary keeps exactly one source. The term
/// names and expansions are identical in the SQLite store.
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
/// Every expansion is generated from `lash_core_execution::ProcessStatus` or
/// `lash_core_execution::WakeDeliveryState`. The term names are the domain's, not a
/// dialect's, and are identical in the SQLite store.
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
    /// `processes` statements only PostgreSQL issues.
    pub(crate) struct ProcessPostgresStatements @ "process" {
        /// The deployment's parked processes as a `?1`-row page in
        /// `(parked_since_ms, process_id)` order over the parked projection's
        /// partial index: only parks at or before `?2`, strictly after keyset
        /// `?3`/`?4`, reason codes drawn from the text array `?5` (`NULL`
        /// means all).
        list_parked = "SELECT record_json FROM processes
             WHERE parked_since_ms IS NOT NULL
               AND (?2 IS NULL OR parked_since_ms <= ?2)
               AND (?3 IS NULL OR parked_since_ms > ?3 OR (parked_since_ms = ?3 AND process_id > ?4))
               AND (?5 IS NULL OR parked_reason_code = ANY(?5))
             ORDER BY parked_since_ms, process_id
             LIMIT ?1";

        /// The stored record for `?1`, under its write lock.
        ///
        /// `FOR UPDATE` is the fork: every decision this store makes about a
        /// process is taken from the row it locks here, where SQLite already
        /// holds the database write lock.
        select_record_json_for_update = "SELECT record_json
             FROM processes
             WHERE process_id = ?1
             FOR UPDATE";

        /// Register a fresh process row, reporting no row when a concurrent
        /// registrar won.
        ///
        /// `ON CONFLICT DO NOTHING` is how that race is detected: under
        /// `READ COMMITTED` the read that decided the row was absent and this
        /// insert take different snapshots, so the loser has to be told rather
        /// than raise a primary-key violation. SQLite reads the absence under
        /// the lock it inserts under and keeps the constraint error.
        insert_registration = "INSERT INTO processes (
                process_id, incarnation, registration_fingerprint, originator_id, wake_session_id,
                identity_kind, identity_label,
                created_at_ms, updated_at_ms, last_event_sequence,
                change_seq, status,
                parent_scope_kind, parent_scope_id, on_parent_end, cancel_requested_at_ms,
                record_json
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
             ON CONFLICT (process_id) DO NOTHING";

        /// How many processes are live.
        ///
        /// No `INDEXED BY`: PostgreSQL has no such hint and its planner picks
        /// `idx_lash_processes_live_worklist` from the statistics, which
        /// `worklist_plans_put_both_cursor_bounds_in_the_partial_index_condition`
        /// pins.
        count_live_worklist = "SELECT COUNT(*) FROM processes WHERE {{live_process_status(status)}}";

        /// The id the first worklist page is bounded by.
        select_max_worklist_process_id = "SELECT MAX(process_id) FROM processes WHERE {{live_process_status(status)}}";

        /// The first `?2` live processes at or below `?1`.
        ///
        /// One parameter fewer than SQLite's, which binds an unused cursor
        /// slot so that both of its worklist pages take the same three binds.
        list_first_worklist_page = "SELECT record_json FROM processes
     WHERE {{live_process_status(status)}} AND process_id <= ?1
     ORDER BY process_id ASC LIMIT ?2";

        /// The next `?3` live processes in `(?2, ?1]`.
        list_next_worklist_page = "SELECT record_json FROM processes
     WHERE {{live_process_status(status)}}
       AND process_id <= ?1 AND process_id > ?2
     ORDER BY process_id ASC LIMIT ?3";

        /// Children of ended parent scope `?1` / `?2` that still owe a cancel:
        /// after `?3`, at most `?4`.
        ///
        /// The cursor parameter carries its type cast, which is the fork: a
        /// bare `NULL` parameter has no type PostgreSQL can compare.
        list_parent_end_children = "SELECT record_json FROM processes
         WHERE parent_scope_kind = ?1
           AND parent_scope_id = ?2
           AND on_parent_end = 'cancel'
           AND cancel_requested_at_ms IS NULL
           AND {{live_process_status(status)}}
           AND (?3::text IS NULL OR process_id > ?3::text)
         ORDER BY process_id ASC
         LIMIT ?4";

        /// Turn scopes with live `Cancel` children and no ledger row yet:
        /// after `?1`, at most `?2`. Same cursor-cast fork.
        ///
        /// The projection id is never parsed back: `parent_scope_id` is a
        /// collision-free canonical key, so `DISTINCT ON` keeps one row per
        /// scope — any child's, since every row sharing the key names the
        /// same typed parent — and `record_json` carries the authority.
        list_unrecorded_opener_parents = "SELECT DISTINCT ON (child.parent_scope_id)
               child.parent_scope_id, child.parent_scope_kind, child.record_json
         FROM processes AS child
         WHERE child.parent_scope_kind IN ('turn', 'queue_drain')
           AND child.on_parent_end = 'cancel'
           AND child.cancel_requested_at_ms IS NULL
           AND {{live_process_status(child.status)}}
           AND NOT EXISTS (
               SELECT 1 FROM parent_end_plans AS plan
               WHERE plan.parent_kind = child.parent_scope_kind
                 AND plan.parent_id = child.parent_scope_id
           )
           AND (?1::text IS NULL OR child.parent_scope_id > ?1::text)
         ORDER BY child.parent_scope_id, child.process_id
         LIMIT ?2";

        /// Prune candidates: retired rows older than `?1`, at or below change
        /// sequence `?2`, with no wake still owed. The survey half, which
        /// locks nothing.
        list_prunable_terminal = "SELECT process_id, record_json FROM processes
         WHERE {{retired_process_status(status)}}
           AND updated_at_ms < ?1
           AND (?2::BIGINT IS NULL OR change_seq <= ?2)
           AND NOT EXISTS (
               SELECT 1 FROM process_wake_deliveries AS delivery
               WHERE delivery.process_id = processes.process_id
                 AND {{undelivered_wake_delivery_state(delivery.state)}}
           )
         ORDER BY process_id ASC";

        /// The same predicate, locking every candidate row for the prune that
        /// follows.
        ///
        /// Two statements rather than one built with a suffix: the survey must
        /// not lock, and the prune must hold its candidates from selection
        /// through the delete.
        list_prunable_terminal_for_update = "SELECT process_id, record_json FROM processes
         WHERE {{retired_process_status(status)}}
           AND updated_at_ms < ?1
           AND (?2::BIGINT IS NULL OR change_seq <= ?2)
           AND NOT EXISTS (
               SELECT 1 FROM process_wake_deliveries AS delivery
               WHERE delivery.process_id = processes.process_id
                 AND {{undelivered_wake_delivery_state(delivery.state)}}
           )
         ORDER BY process_id ASC
         FOR UPDATE";

        /// The change feed after `?1`, at most `?2` rows.
        ///
        /// `json_build_object` and the `::TEXT` cast are the fork; SQLite
        /// spells the same assembly `json_object`.
        list_changes_after = "SELECT change_seq, kind, payload FROM (
                 SELECT change_seq, 'upsert' AS kind, record_json AS payload
                 FROM processes WHERE change_seq > ?1
                 UNION ALL
                 SELECT pruned_change_seq,
                    'deleted' AS kind,
                    json_build_object(
                        'process_id', process_id,
                        'incarnation', incarnation,
                        'terminal_label', terminal_label,
                        'pruned_at_ms', pruned_at_ms,
                        'pruned_change_seq', pruned_change_seq
                    )::TEXT AS payload
                 FROM process_tombstones WHERE pruned_change_seq > ?1
             ) changes
             ORDER BY change_seq ASC
             LIMIT ?2";

        /// Of the id array `?1`, the ids this registry has never heard of.
        ///
        /// `UNNEST … WITH ORDINALITY` is PostgreSQL's spelling of the bound
        /// list SQLite reads with `json_each`.
        classify_unregistered_candidates = "SELECT candidate.process_id
         FROM UNNEST(?1::TEXT[]) WITH ORDINALITY AS candidate(process_id, ordinal)
         WHERE NOT EXISTS (
             SELECT 1 FROM processes p
             WHERE p.process_id = candidate.process_id
         )
           AND NOT EXISTS (
             SELECT 1 FROM process_tombstones t
             WHERE t.process_id = candidate.process_id
         )
         ORDER BY candidate.ordinal ASC";

        /// Of the id array `?1`, the ids that have been pruned.
        classify_tombstoned_candidates = "SELECT candidate.process_id
         FROM UNNEST(?1::TEXT[]) WITH ORDINALITY AS candidate(process_id, ordinal)
         WHERE EXISTS (
             SELECT 1 FROM process_tombstones t
             WHERE t.process_id = candidate.process_id
         )
           AND NOT EXISTS (
             SELECT 1 FROM processes p
             WHERE p.process_id = candidate.process_id
         )
         ORDER BY candidate.ordinal ASC";

/// Every process matching the always-bound filters, including those
        /// retired since `?10` when it is bound.

        list = "SELECT record_json FROM processes
             WHERE (?1::TEXT[] IS NULL OR status = ANY(?1))
               AND (?2::TEXT IS NULL OR originator_id = ?2)
               AND (?3::TEXT IS NULL OR identity_kind = ?3)
               AND (?4::TEXT IS NULL OR identity_label = ?4)
               AND (?5::JSONB IS NULL OR
                    (record_json::JSONB #> '{identity,definition,definition}') = ?5)
               AND (?6::TEXT IS NULL OR
                    (record_json::JSONB #>> '{provenance,caused_by,occurrence_id}') = ?6)
               AND (?7::TEXT IS NULL OR
                    (record_json::JSONB #>> '{provenance,caused_by,subscription_id}') = ?7)
               AND (?8::BIGINT IS NULL OR created_at_ms >= ?8)
               AND (?9::BIGINT IS NULL OR created_at_ms < ?9)
               AND (?10::BIGINT IS NULL OR {{live_process_status(status)}}
                    OR updated_at_ms >= ?10)
             ORDER BY process_id ASC";
        /// The same, narrowed to parent scope `?11` / `?12`.
        list_by_parent_scope = "SELECT record_json FROM processes
             WHERE (?1::TEXT[] IS NULL OR status = ANY(?1))
               AND (?2::TEXT IS NULL OR originator_id = ?2)
               AND (?3::TEXT IS NULL OR identity_kind = ?3)
               AND (?4::TEXT IS NULL OR identity_label = ?4)
               AND (?5::JSONB IS NULL OR
                    (record_json::JSONB #> '{identity,definition,definition}') = ?5)
               AND (?6::TEXT IS NULL OR
                    (record_json::JSONB #>> '{provenance,caused_by,occurrence_id}') = ?6)
               AND (?7::TEXT IS NULL OR
                    (record_json::JSONB #>> '{provenance,caused_by,subscription_id}') = ?7)
               AND (?8::BIGINT IS NULL OR created_at_ms >= ?8)
               AND (?9::BIGINT IS NULL OR created_at_ms < ?9)
               AND (?10::BIGINT IS NULL OR {{live_process_status(status)}}
                    OR updated_at_ms >= ?10)
               AND parent_scope_kind = ?11
               AND parent_scope_id IS NOT DISTINCT FROM ?12::TEXT
             ORDER BY process_id ASC";
        /// The same, narrowed to rows whose cancel request is older than `?11`
        /// and whose outcome is still open.
        list_pending_cancel = "SELECT record_json FROM processes
             WHERE (?1::TEXT[] IS NULL OR status = ANY(?1))
               AND (?2::TEXT IS NULL OR originator_id = ?2)
               AND (?3::TEXT IS NULL OR identity_kind = ?3)
               AND (?4::TEXT IS NULL OR identity_label = ?4)
               AND (?5::JSONB IS NULL OR
                    (record_json::JSONB #> '{identity,definition,definition}') = ?5)
               AND (?6::TEXT IS NULL OR
                    (record_json::JSONB #>> '{provenance,caused_by,occurrence_id}') = ?6)
               AND (?7::TEXT IS NULL OR
                    (record_json::JSONB #>> '{provenance,caused_by,subscription_id}') = ?7)
               AND (?8::BIGINT IS NULL OR created_at_ms >= ?8)
               AND (?9::BIGINT IS NULL OR created_at_ms < ?9)
               AND (?10::BIGINT IS NULL OR {{live_process_status(status)}}
                    OR updated_at_ms >= ?10)
               AND cancel_requested_at_ms IS NOT NULL
               AND cancel_requested_at_ms < ?11
               AND {{nonterminal_process_status(status)}}
             ORDER BY process_id ASC";
        /// Both narrowings at once.
        list_by_parent_scope_pending_cancel = "SELECT record_json FROM processes
             WHERE (?1::TEXT[] IS NULL OR status = ANY(?1))
               AND (?2::TEXT IS NULL OR originator_id = ?2)
               AND (?3::TEXT IS NULL OR identity_kind = ?3)
               AND (?4::TEXT IS NULL OR identity_label = ?4)
               AND (?5::JSONB IS NULL OR
                    (record_json::JSONB #> '{identity,definition,definition}') = ?5)
               AND (?6::TEXT IS NULL OR
                    (record_json::JSONB #>> '{provenance,caused_by,occurrence_id}') = ?6)
               AND (?7::TEXT IS NULL OR
                    (record_json::JSONB #>> '{provenance,caused_by,subscription_id}') = ?7)
               AND (?8::BIGINT IS NULL OR created_at_ms >= ?8)
               AND (?9::BIGINT IS NULL OR created_at_ms < ?9)
               AND (?10::BIGINT IS NULL OR {{live_process_status(status)}}
                    OR updated_at_ms >= ?10)
               AND parent_scope_kind = ?11
               AND parent_scope_id IS NOT DISTINCT FROM ?12::TEXT
               AND cancel_requested_at_ms IS NOT NULL
               AND cancel_requested_at_ms < ?13
               AND {{nonterminal_process_status(status)}}
             ORDER BY process_id ASC";
    }
}

lash_store_sql::statements! {
    /// Registry-wide statements only PostgreSQL issues.
    pub(crate) struct ProcessRegistryPostgresStatements @ "process_registry" {
        /// Session `?1`'s observed processes, filtered by the status array
        /// `?2` and, when `?3` is bound, by retirement recency.
        ///
        /// One statement where SQLite keeps two: SQLite splits the recency arm
        /// into a union so each half lands on its own partial index, and
        /// PostgreSQL's planner does not need the split.
        list_observed = "SELECT p.record_json
             FROM process_observers o
             JOIN processes p ON p.process_id = o.process_id
                                    AND p.incarnation = o.process_incarnation
             WHERE o.session_id = ?1
               AND (?2::TEXT[] IS NULL OR p.status = ANY(?2))
               AND (?3::BIGINT IS NULL OR {{live_process_status(p.status)}}
                    OR p.updated_at_ms >= ?3)
             ORDER BY p.process_id";

        /// One statement rather than two: under `READ COMMITTED` a
        /// registration could land between two reads, and the pair is what
        /// decides whether an observer question is refused as unknown or
        /// answered. SQLite asks the two halves separately under one write
        /// lock.
        observation_and_registration_exist = "SELECT EXISTS(SELECT 1 FROM processes WHERE process_id = ?2),
                EXISTS(
                    SELECT 1 FROM process_observers
                    WHERE session_id = ?1 AND process_id = ?2
                )";

        /// Prune the process rows named by the id array `?1`, stamping their
        /// tombstones `?2`; reports the events and the rows it removed.
        ///
        /// One statement for the whole prune, where SQLite issues eight under
        /// its write lock. Every part has to see the same snapshot: the clock
        /// is bumped once for the batch, the tombstones take their change
        /// sequences from that bump in candidate order, the artifact cleanup
        /// rows are derived from the tombstones, and the process delete runs
        /// only if both counts match the candidate count. Splitting it would
        /// let a status writer land between the parts.
        prune_rows = "WITH candidates AS (
             SELECT process_id, ordinality
             FROM unnest(?1::TEXT[]) WITH ORDINALITY
                  AS candidate(process_id, ordinality)
         ),
         deleted_events AS (
             DELETE FROM process_events AS event
             USING candidates AS candidate
             WHERE event.process_id = candidate.process_id
             RETURNING event.process_id
         ),
         event_count AS MATERIALIZED (
             SELECT count(*) AS value FROM deleted_events
         ),
         advanced_clock AS (
             UPDATE process_change_clock
             SET current_seq = current_seq + (SELECT count(*) FROM candidates)
             WHERE singleton = TRUE
             RETURNING current_seq
         ),
         inserted_tombstones AS (
             INSERT INTO process_tombstones (
                 process_id, incarnation, terminal_label, pruned_at_ms, pruned_change_seq
             )
             SELECT candidate.process_id,
                    process.incarnation,
                    process.status,
                    ?2,
                    clock.current_seq - (SELECT count(*) FROM candidates) + candidate.ordinality
             FROM candidates AS candidate
             JOIN processes AS process USING (process_id)
             CROSS JOIN advanced_clock AS clock
             CROSS JOIN event_count
             WHERE event_count.value >= 0
             ORDER BY candidate.ordinality
             RETURNING process_id, incarnation
         ),
         inserted_artifact_cleanup AS (
             INSERT INTO process_artifact_cleanup (
                 process_id, incarnation, cleanup_json
             )
             SELECT tombstone.process_id,
                    tombstone.incarnation,
                    jsonb_build_object(
                        'process_id', process.process_id,
                        'incarnation', process.incarnation,
                        'env_ref', process.record_json::jsonb -> 'env_ref',
                        'input', process.record_json::jsonb -> 'input'
                    )::text
             FROM inserted_tombstones AS tombstone
             JOIN processes AS process USING (process_id, incarnation)
             RETURNING process_id
         ),
         deleted_processes AS (
             DELETE FROM processes AS process
             USING candidates AS candidate,
                   (SELECT count(*) FROM inserted_tombstones) AS tombstones,
                   (SELECT count(*) FROM inserted_artifact_cleanup) AS cleanup
             WHERE process.process_id = candidate.process_id
               AND tombstones.count = (SELECT count(*) FROM candidates)
               AND cleanup.count = (SELECT count(*) FROM candidates)
             RETURNING process.process_id
         )
         SELECT (SELECT value FROM event_count),
                (SELECT count(*) FROM deleted_processes)";
    }
}

lash_store_sql::statements! {
    /// `process_observers` statements only PostgreSQL issues.
    pub(crate) struct ObserverPostgresStatements @ "process_observer" {
        /// `ON CONFLICT DO NOTHING` is PostgreSQL's spelling of SQLite's `INSERT OR IGNORE`.
        insert_if_absent = "INSERT INTO process_observers (session_id, process_id, process_incarnation)
             VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING";

        /// The sessions observing process `?1`, at any incarnation.
        ///
        /// Unnarrowed, where SQLite's twin filters to the incarnation the
        /// caller read.
        list_sessions_for_process = "SELECT session_id FROM process_observers WHERE process_id = ?1 ORDER BY session_id";
    }
}

lash_store_sql::statements! {
    /// `process_leases` statements only PostgreSQL issues.
    pub(crate) struct LeasePostgresStatements @ "process_lease" {
        /// The retained fencing token of `?1`, under the row's write lock.
        ///
        /// `FOR UPDATE` is the fork: the claim decision is taken from this
        /// read and must not interleave with another claimant's, which
        /// SQLite's `BEGIN IMMEDIATE` already guarantees.
        select_fencing_token_for_update = "SELECT lease_fencing_token FROM process_leases WHERE process_id = ?1 FOR UPDATE";

        /// `?1`'s lease, under the row's write lock. Same lock fork.
        select_by_process_for_update = "SELECT lease_owner_id, lease_token, lease_fencing_token,
                lease_claimed_at_ms, lease_expires_at_ms,
                lease_owner_incarnation_id
         FROM process_leases
         WHERE process_id = ?1
         FOR UPDATE";

        /// The leases of every process in the id array `?1`.
        list_by_process_ids = "SELECT process_id, lease_owner_id, lease_token,
                lease_fencing_token, lease_claimed_at_ms,
                lease_expires_at_ms, lease_owner_incarnation_id
         FROM process_leases
         WHERE process_id = ANY(?1)";

        /// Take the lease of `?1` at fencing token `?5`. `EXCLUDED` is
        /// PostgreSQL's spelling of SQLite's `excluded`.
        upsert_acquired = "INSERT INTO process_leases (
            process_id, lease_owner_id, lease_owner_incarnation_id,
            lease_token, lease_fencing_token,
            lease_claimed_at_ms, lease_expires_at_ms
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (process_id) DO UPDATE SET
            lease_owner_id = EXCLUDED.lease_owner_id,
            lease_owner_incarnation_id = EXCLUDED.lease_owner_incarnation_id,
            lease_token = EXCLUDED.lease_token,
            lease_fencing_token = EXCLUDED.lease_fencing_token,
            lease_claimed_at_ms = EXCLUDED.lease_claimed_at_ms,
            lease_expires_at_ms = EXCLUDED.lease_expires_at_ms";
    }
}

lash_store_sql::statements! {
    /// `process_change_clock` statements only PostgreSQL issues.
    ///
    /// Every one of them forks: the singleton flag is `BOOLEAN TRUE` here and
    /// `INTEGER 1` on SQLite, and the bump reports its new value through
    /// `RETURNING` where SQLite issues a second read.
    pub(crate) struct ChangeClockPostgresStatements @ "process_change_clock" {
        /// Advance the change sequence by one and report it.
        bump_returning = "UPDATE process_change_clock
         SET current_seq = current_seq + 1
         WHERE singleton = TRUE
         RETURNING current_seq";

        /// The current change sequence, under the row's write lock: the lock
        /// that orders two concurrent registrations of one content-addressed
        /// process id.
        select_current_for_update = "SELECT current_seq FROM process_change_clock WHERE singleton = TRUE FOR UPDATE";

        /// How far tombstone compaction has run, under a share lock: a reader
        /// must not see the horizon move past its own cursor mid-read.
        select_compaction_horizon_for_share = "SELECT tombstone_compaction_horizon
         FROM process_change_clock
         WHERE singleton = TRUE
         FOR SHARE";

        /// Raise the compaction horizon to `?1`, never lowering it.
        /// `GREATEST` is PostgreSQL's spelling of SQLite's two-argument `MAX`.
        raise_compaction_horizon = "UPDATE process_change_clock
         SET tombstone_compaction_horizon = GREATEST(
             tombstone_compaction_horizon, ?1
         )
         WHERE singleton = TRUE";
    }
}

lash_store_sql::statements! {
    /// `process_tombstones` statements only PostgreSQL issues.
    pub(crate) struct TombstonePostgresStatements @ "process_tombstone" {
        /// The highest change sequence compaction may advance to: tombstones
        /// older than `?1`, at or below `?2`, not in the id array `?3`, and
        /// with no artifact cleanup still owed.
        select_max_compactable_change_seq = "SELECT MAX(pruned_change_seq) FROM process_tombstones
             WHERE pruned_at_ms < ?1
               AND (?2::BIGINT IS NULL OR pruned_change_seq <= ?2)
               AND NOT (process_id = ANY(?3::TEXT[]))
               AND NOT EXISTS (
                   SELECT 1 FROM process_artifact_cleanup AS cleanup
                   WHERE cleanup.process_id = process_tombstones.process_id
                     AND cleanup.incarnation = process_tombstones.incarnation
               )";

        /// Delete exactly the rows
        /// [`TombstonePostgresStatements::select_max_compactable_change_seq`]
        /// measured.
        delete_compactable = "DELETE FROM process_tombstones
             WHERE pruned_at_ms < ?1
               AND (?2::BIGINT IS NULL OR pruned_change_seq <= ?2)
               AND NOT (process_id = ANY(?3::TEXT[]))
               AND NOT EXISTS (
                   SELECT 1 FROM process_artifact_cleanup AS cleanup
                   WHERE cleanup.process_id = process_tombstones.process_id
                     AND cleanup.incarnation = process_tombstones.incarnation
               )";
    }
}

lash_store_sql::statements! {
    /// `process_artifact_cleanup` statements only PostgreSQL issues.
    pub(crate) struct ArtifactCleanupPostgresStatements @ "process_artifact_cleanup" {
        /// Acknowledge the release owed for `?1` / `?2`, reporting whether a
        /// row was removed and what incarnation the process is on now.
        ///
        /// One statement, because the pair decides between "acknowledged",
        /// "stale incarnation" and "unknown" and a re-registration between two
        /// reads would answer the wrong one. SQLite asks the two halves under
        /// its write lock.
        delete_for_incarnation_reporting_incarnation = "WITH deleted AS (
             DELETE FROM process_artifact_cleanup
             WHERE process_id = ?1 AND incarnation = ?2
             RETURNING 1
         )
         SELECT EXISTS(SELECT 1 FROM deleted),
                (SELECT incarnation FROM processes WHERE process_id = ?1)";
    }
}

lash_store_sql::statements! {
    /// `process_segment_handovers` statements only PostgreSQL issues.
    pub(crate) struct SegmentHandoverPostgresStatements @ "process_segment_handover" {
        /// Park the handover of `?1` at segment `?2`, reporting no row when
        /// another writer's handover is already parked there.
        ///
        /// The conflict clause carries the refusal: a repeat of the same
        /// bytes, or any handover from the writer already parked there (its own
        /// retried write), is idempotent and keeps the parked bytes; another
        /// writer's handover writes nothing and the caller raises the
        /// conflict. SQLite reads the ordinal under its write lock and decides
        /// the same thing in Rust.
        upsert_identical = "INSERT INTO process_segment_handovers
             (process_id, segment_ordinal, handover_json) VALUES (?1, ?2, ?3)
             ON CONFLICT (process_id, segment_ordinal) DO UPDATE
             SET handover_json = process_segment_handovers.handover_json
             WHERE process_segment_handovers.handover_json = EXCLUDED.handover_json
                OR (COALESCE(EXCLUDED.handover_json::jsonb ->> 'writer', '') <> ''
                    AND process_segment_handovers.handover_json::jsonb ->> 'writer'
                        = EXCLUDED.handover_json::jsonb ->> 'writer')";

        /// The parked-continuation page of the preflight walk: after `?1` /
        /// `?2`, at most `?3`.
        ///
        /// The resume filter is a row-value comparison against the same two
        /// columns the `ORDER BY` uses, which cannot disagree with that
        /// ordering under any collation. SQLite mints a text cursor in the
        /// projection instead, which is the fork.
        list_parked_segments = "SELECT
         handovers.process_id,
         handovers.segment_ordinal,
         handovers.handover_json,
         process.status,
         process.wake_session_id,
         process.record_json
     FROM process_segment_handovers AS handovers
     JOIN processes AS process
         ON process.process_id = handovers.process_id
     WHERE {{live_process_status(process.status)}}
       AND (
           ?1::text IS NULL
           OR (handovers.process_id, handovers.segment_ordinal) > (?1::text, ?2::bigint)
       )
     ORDER BY handovers.process_id, handovers.segment_ordinal
     LIMIT ?3";
    }
}

lash_store_sql::statements! {
    /// `process_wake_deliveries` statements only PostgreSQL issues.
    pub(crate) struct WakeDeliveryPostgresStatements @ "process_wake_delivery" {
        /// `ON CONFLICT (delivery_id) DO NOTHING` is PostgreSQL's spelling of SQLite's `INSERT
        /// OR IGNORE`.
        insert_pending = "INSERT INTO process_wake_deliveries (
            delivery_id, process_id, process_incarnation, target_session_id, sequence, state,
            claim_token, attempts, first_attempt_ms, next_attempt_at_ms, expires_at_ms,
            discard_reason, delivery_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, {{pending_wake_delivery_state_value(state)}}, NULL, 0, NULL, ?6, ?7, NULL, ?8)
         ON CONFLICT (delivery_id) DO NOTHING";

        /// The next `?1` claimable wakes at `?2`, skipping any whose ordering
        /// group still holds an earlier delivery that blocks it. `?3` is the
        /// array of discard reasons that do not block.
        ///
        /// `FOR UPDATE OF candidate SKIP LOCKED` is the fork: several workers
        /// claim from this queue concurrently and each must take a disjoint
        /// page. SQLite serialises them through `BEGIN IMMEDIATE` and needs
        /// neither clause.
        select_claimable = "SELECT candidate.delivery_id
         FROM process_wake_deliveries AS candidate
         WHERE {{pending_wake_delivery_state(candidate.state)}}
           AND candidate.next_attempt_at_ms <= ?2
           AND NOT EXISTS (
               SELECT 1
               FROM process_wake_deliveries AS earlier
               WHERE {{not_enqueued_wake_delivery_state(earlier.state)}}
                 AND NOT (
                     {{discarded_wake_delivery_state(earlier.state)}}
                     AND (
                         earlier.discard_reason IS NULL
                         OR earlier.discard_reason = ANY(?3::TEXT[])
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
         LIMIT ?1
         FOR UPDATE OF candidate SKIP LOCKED";

        /// Delivery `?1`, whole.
        ///
        /// Carries the key, where SQLite's keyed read drops it: this store
        /// decodes every delivery through one row decoder, and the unkeyed
        /// listings below need the key in the projection.
        select_report = "SELECT delivery_id, state, claim_token, attempts, first_attempt_ms,
                    next_attempt_at_ms, expires_at_ms, discard_reason, delivery_json
         FROM process_wake_deliveries WHERE delivery_id = ?1";

        /// Every delivery in state `?1`.
        ///
        /// Reports the rows themselves; SQLite lists the ids and reads each
        /// delivery back.
        list_by_state = "SELECT delivery_id, state, claim_token, attempts, first_attempt_ms,
                    next_attempt_at_ms, expires_at_ms, discard_reason, delivery_json
         FROM process_wake_deliveries WHERE state = ?1 ORDER BY delivery_id ASC";

        /// Every delivery.
        list_all = "SELECT delivery_id, state, claim_token, attempts, first_attempt_ms,
                    next_attempt_at_ms, expires_at_ms, discard_reason, delivery_json
         FROM process_wake_deliveries ORDER BY delivery_id ASC";

        /// The undelivered-wake page of the preflight walk: after `?1`, at
        /// most `?2`. The cursor's cast is the fork.
        list_undelivered_for_walk = "SELECT
         delivery_id,
         process_id,
         target_session_id,
         state,
         delivery_json
     FROM process_wake_deliveries
     WHERE {{undelivered_wake_delivery_state(state)}}
       AND (?1::text IS NULL OR delivery_id > ?1::text)
     ORDER BY delivery_id
     LIMIT ?2";
    }
}

lash_store_sql::statements! {
    /// `parent_end_plans` statements only PostgreSQL issues.
    pub(crate) struct ParentEndPlanPostgresStatements @ "parent_end_plan" {
        /// Reclaim settled plans older than `?1` that no live child still
        /// names.
        ///
        /// The alias is the fork: PostgreSQL cannot name the table it is
        /// deleting from inside a correlated subquery without one, and SQLite
        /// cannot use one at all in a `DELETE`.
        delete_reclaimable = "DELETE FROM parent_end_plans AS plan
         WHERE plan.settled_at_ms IS NOT NULL
           AND plan.settled_at_ms < ?1
           AND NOT EXISTS (
               SELECT 1 FROM processes AS child
               WHERE child.parent_scope_kind = plan.parent_kind
                 AND child.parent_scope_id = plan.parent_id
                 AND {{live_process_status(child.status)}}
           )";
    }
}

lash_store_sql::statements! {
    /// `wake_allocation_floors` statements only PostgreSQL issues.
    pub(crate) struct WakeAllocationFloorPostgresStatements @ "wake_allocation_floor" {
        /// Raise session `?1`'s floor for process `?2` to `?3`, never lowering
        /// it. `GREATEST` over `EXCLUDED` is PostgreSQL's spelling of SQLite's
        /// `MAX` over `excluded`.
        upsert_max = "INSERT INTO wake_allocation_floors (
            target_session_id, process_id, allocation_floor
         ) VALUES (?1, ?2, ?3)
         ON CONFLICT (target_session_id, process_id) DO UPDATE SET
            allocation_floor = GREATEST(
                wake_allocation_floors.allocation_floor,
                EXCLUDED.allocation_floor
            )";
    }
}

lash_store_sql::statements! {
    /// `wake_redelivery_fences` statements only PostgreSQL issues.
    pub(crate) struct WakeRedeliveryFencePostgresStatements @ "wake_redelivery_fence" {
        /// Raise session `?1`'s consumed floor for process `?2` to `?3`.
        ///
        /// This store's commit path has already decoded the wakes it is
        /// settling and binds their values; SQLite derives the same floors
        /// from the queued rows in one statement.
        upsert_max = "INSERT INTO wake_redelivery_fences (
                session_id, process_id, allocation_floor
             ) VALUES (?1, ?2, ?3)
             ON CONFLICT (session_id, process_id) DO UPDATE SET
                allocation_floor = GREATEST(
                    wake_redelivery_fences.allocation_floor,
                    EXCLUDED.allocation_floor
                )";
    }
}

lash_store_sql::statements! {
    /// `process_park_clock` statements only PostgreSQL issues (FIG-3659
    /// NOW-B): the singleton flag, and the bump reporting its value through
    /// `RETURNING` in the round trip that takes the row lock.
    pub(crate) struct ProcessParkClockPostgresStatements @ "process_park_clock" {
        /// Allocate one feed sequence and report it. The row lock the update
        /// takes orders writers, so the allocated order is commit order.
        bump_returning = "UPDATE process_park_clock
             SET current_seq = current_seq + 1
             WHERE singleton = TRUE
             RETURNING current_seq";

        /// The cursor below which a feed read is refused
        /// `ProcessParkFeedCursorCompacted`, under a share lock: a compaction
        /// that is still committing must not let a read at a stale cursor
        /// pass unrefused while its events are already gone.
        select_compaction_horizon_for_share = "SELECT compaction_horizon
             FROM process_park_clock
             WHERE singleton = TRUE
             FOR SHARE";

        /// The allocated sequence under a row lock, read before compaction:
        /// locking the clock first serializes against concurrent bumps, and
        /// clamping `through` to it keeps the horizon from rising past events
        /// the feed has not yet committed.
        select_current_for_update = "SELECT current_seq
             FROM process_park_clock
             WHERE singleton = TRUE
             FOR UPDATE";

        /// Raise the compaction horizon to `?1`, never lowering it.
        raise_compaction_horizon = "UPDATE process_park_clock
             SET compaction_horizon = GREATEST(compaction_horizon, ?1)
             WHERE singleton = TRUE";
    }
}

/// Every process-family statement, rendered for PostgreSQL.
pub(crate) struct ProcessSql {
    /// `processes` statements both backends issue verbatim.
    pub(crate) process: ProcessStatements,
    /// `processes` statements only PostgreSQL issues.
    pub(crate) process_postgres: ProcessPostgresStatements,
    /// Registry-wide statements only PostgreSQL issues.
    pub(crate) registry_postgres: ProcessRegistryPostgresStatements,
    /// `process_definitions` statements, all of them shared.
    pub(crate) definition: DefinitionStatements,
    /// `process_events` statements both backends issue verbatim.
    pub(crate) event: EventStatements,
    /// `process_leases` statements both backends issue verbatim.
    pub(crate) lease: LeaseStatements,
    /// `process_leases` statements only PostgreSQL issues.
    pub(crate) lease_postgres: LeasePostgresStatements,
    /// `process_observers` statements both backends issue verbatim.
    pub(crate) observer: ObserverStatements,
    /// `process_observers` statements only PostgreSQL issues.
    pub(crate) observer_postgres: ObserverPostgresStatements,
    /// `process_segment_handovers` statements both backends issue verbatim.
    pub(crate) handover: SegmentHandoverStatements,
    /// `process_segment_handovers` statements only PostgreSQL issues.
    pub(crate) handover_postgres: SegmentHandoverPostgresStatements,
    /// `process_tombstones` statements both backends issue verbatim.
    pub(crate) tombstone: TombstoneStatements,
    /// `process_tombstones` statements only PostgreSQL issues.
    pub(crate) tombstone_postgres: TombstonePostgresStatements,
    /// `process_wake_deliveries` statements both backends issue verbatim.
    pub(crate) wake: WakeDeliveryStatements,
    /// `process_wake_deliveries` statements only PostgreSQL issues.
    pub(crate) wake_postgres: WakeDeliveryPostgresStatements,
    /// `process_change_clock` statements, all of them PostgreSQL's own.
    pub(crate) clock_postgres: ChangeClockPostgresStatements,
    /// `process_park_events` statements both backends issue verbatim.
    pub(crate) park_event: ProcessParkEventStatements,
    /// `process_park_clock` statements, all of them PostgreSQL's own.
    pub(crate) park_clock_postgres: ProcessParkClockPostgresStatements,
    /// `process_artifact_cleanup` statements both backends issue verbatim.
    pub(crate) cleanup: ArtifactCleanupStatements,
    /// `process_artifact_cleanup` statements only PostgreSQL issues.
    pub(crate) cleanup_postgres: ArtifactCleanupPostgresStatements,
    /// `parent_end_plans` statements both backends issue verbatim.
    pub(crate) plan: ParentEndPlanStatements,
    /// `parent_end_plans` statements only PostgreSQL issues.
    pub(crate) plan_postgres: ParentEndPlanPostgresStatements,
    /// `wake_allocation_floors` statements both backends issue verbatim.
    pub(crate) floor: WakeAllocationFloorStatements,
    /// `wake_allocation_floors` statements only PostgreSQL issues.
    pub(crate) floor_postgres: WakeAllocationFloorPostgresStatements,
    /// `wake_redelivery_fences` statements both backends issue verbatim.
    pub(crate) fence: WakeRedeliveryFenceStatements,
    /// `wake_redelivery_fences` statements only PostgreSQL issues.
    pub(crate) fence_postgres: WakeRedeliveryFencePostgresStatements,
}

static PROCESS_SQL: LazyLock<ProcessSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres().with_vocabulary(PROCESS_LIFECYCLE);
    ProcessSql {
        process: ProcessStatements::render(dialect),
        process_postgres: ProcessPostgresStatements::render(dialect),
        registry_postgres: ProcessRegistryPostgresStatements::render(dialect),
        definition: DefinitionStatements::render(dialect),
        event: EventStatements::render(dialect),
        lease: LeaseStatements::render(dialect),
        lease_postgres: LeasePostgresStatements::render(dialect),
        observer: ObserverStatements::render(dialect),
        observer_postgres: ObserverPostgresStatements::render(dialect),
        handover: SegmentHandoverStatements::render(dialect),
        handover_postgres: SegmentHandoverPostgresStatements::render(dialect),
        tombstone: TombstoneStatements::render(dialect),
        tombstone_postgres: TombstonePostgresStatements::render(dialect),
        wake: WakeDeliveryStatements::render(dialect),
        wake_postgres: WakeDeliveryPostgresStatements::render(dialect),
        clock_postgres: ChangeClockPostgresStatements::render(dialect),
        park_event: ProcessParkEventStatements::render(dialect),
        park_clock_postgres: ProcessParkClockPostgresStatements::render(dialect),
        cleanup: ArtifactCleanupStatements::render(dialect),
        cleanup_postgres: ArtifactCleanupPostgresStatements::render(dialect),
        plan: ParentEndPlanStatements::render(dialect),
        plan_postgres: ParentEndPlanPostgresStatements::render(dialect),
        floor: WakeAllocationFloorStatements::render(dialect),
        floor_postgres: WakeAllocationFloorPostgresStatements::render(dialect),
        fence: WakeRedeliveryFenceStatements::render(dialect),
        fence_postgres: WakeRedeliveryFencePostgresStatements::render(dialect),
    }
});

/// The process-family statements, rendered once at first use and never again.
pub(crate) fn process_sql() -> &'static ProcessSql {
    &PROCESS_SQL
}

/// The list statement this filter asks for.
///
/// The optional clauses are conjuncts that are either present or absent — an
/// `($n IS NULL OR …)` over them would cost the planner the partial indexes
/// they exist to use — so each combination is its own named statement rather
/// than a template with a hole. The caller binds the always-bound ten
/// parameters and then, in this same order, the clauses' own.
pub(crate) fn list_processes_sql(filter: &lash_core_execution::ProcessListFilter) -> &'static str {
    let statements = &process_sql().process_postgres;
    match (
        filter.parent_scope.is_some(),
        filter.cancel_pending_before_ms.is_some(),
    ) {
        (false, false) => statements.list.sql(),
        (true, false) => statements.list_by_parent_scope.sql(),
        (false, true) => statements.list_pending_cancel.sql(),
        (true, true) => statements.list_by_parent_scope_pending_cancel.sql(),
    }
}
