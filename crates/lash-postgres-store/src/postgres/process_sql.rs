//! Every process-family statement this store issues, rendered once.
//!
//! The shared halves live in `lash_store_sql::process`; the sets below are the
//! statements only PostgreSQL issues. Most of the forks are one of four
//! things: a `FOR UPDATE` or
//! `FOR SHARE` lock suffix that SQLite does not need under `BEGIN IMMEDIATE`,
//! an `ON CONFLICT` clause that detects a race SQLite's write lock makes
//! unreachable, an array parameter where SQLite binds a JSON list, and the
//! `BOOLEAN TRUE` singleton flag where SQLite writes `INTEGER 1`.

use std::sync::LazyLock;

use lash_core_execution::store_backend_support as vocabulary;
use lash_store_sql::process::{
    abandoned_consumer_holds::AbandonedConsumerHoldStatements,
    event_horizons::EventHorizonStatements, events::EventStatements, observers::ObserverStatements,
    parent_end_plans::ParentEndPlanStatements, processes::ProcessStatements,
    tombstones::TombstoneStatements,
};
use lash_store_sql::{Dialect, Vocabulary, VocabularyTerm};

/// The process family's domain vocabulary, as this backend supplies it.
///
/// Every expansion is generated from `lash_core_execution::ProcessStatus`. The
/// term names are the domain's, not a dialect's, and are identical in the
/// SQLite store.
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
]);

lash_store_sql::statements! {
    /// `processes` statements only PostgreSQL issues.
    pub(crate) struct ProcessPostgresStatements @ "process" {
        /// The preflight's started-process walk (FIG-3571): every live
        /// process strictly after key `?1` (`NULL` from the start), at most
        /// `?2`, in key order.
        list_live_for_preflight = "SELECT process_id, status, wake_session_id, record_json
             FROM processes
             WHERE {{live_process_status(status)}}
               AND (?1::text IS NULL OR process_id > ?1::text)
             ORDER BY process_id
             LIMIT ?2";

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
        /// start under the same key won.
        ///
        /// `ON CONFLICT DO NOTHING` on the start-key index is how that race is
        /// detected: under `READ COMMITTED` the read that found no retained
        /// process for the key and this insert take different snapshots, so
        /// the loser has to be told rather than raise the unique violation.
        /// The minted id itself never collides. SQLite reads the absence under
        /// the lock it inserts under.
        insert_registration = "INSERT INTO processes (
                process_id, start_key, originator_id, wake_session_id,
                identity_kind, identity_label,
                created_at_ms, updated_at_ms, last_event_sequence,
                change_seq, status,
                lifetime_scope_kind, lifetime_scope_id, lifetime, cancel_requested_at_ms,
                record_json, consumer_hold_key, consumer_hold_scope_kind, consumer_hold_scope_id,
                consumer_hold_cancels
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
             ON CONFLICT (start_key) WHERE start_key IS NOT NULL DO NOTHING";

        /// How many processes are live.
        ///
        /// No `INDEXED BY`: PostgreSQL has no such hint and its planner picks
        /// `idx_lash_processes_non_terminal` from the statistics, which
        /// `non_terminal_page_plans_put_both_cursor_bounds_in_the_partial_index_condition`
        /// pins.
        count_non_terminal_processes = "SELECT COUNT(*) FROM processes WHERE {{live_process_status(status)}}";

        /// The id the first non-terminal page is bounded by.
        select_max_non_terminal_process_id = "SELECT MAX(process_id) FROM processes WHERE {{live_process_status(status)}}";

        /// The first `?2` live processes at or below `?1`.
        ///
        /// One parameter fewer than SQLite's, which binds an unused cursor
        /// slot so that both non-terminal page queries take the same three binds.
        list_first_non_terminal_process_page = "SELECT record_json FROM processes
     WHERE {{live_process_status(status)}} AND process_id <= ?1
     ORDER BY process_id ASC LIMIT ?2";

        /// The next `?3` live processes in `(?2, ?1]`.
        list_next_non_terminal_process_page = "SELECT record_json FROM processes
     WHERE {{live_process_status(status)}}
       AND process_id <= ?1 AND process_id > ?2
     ORDER BY process_id ASC LIMIT ?3";

        /// Prune candidates: retired rows older than `?1`, at or below change
        /// sequence `?2`, with no consumer hold. The survey half, which locks
        /// nothing.
        list_prunable_terminal = "SELECT record_json FROM processes
         WHERE {{retired_process_status(status)}}
           AND updated_at_ms < ?1
           AND (?2::BIGINT IS NULL OR change_seq <= ?2)
           AND consumer_hold_key IS NULL
               AND cascade_cursor IS NULL
         ORDER BY process_id ASC";

        /// The same predicate, locking every candidate row for the prune that
        /// follows.
        ///
        /// Two statements rather than one built with a suffix: the survey must
        /// not lock, and the prune must hold its candidates from selection
        /// through the delete.
        list_prunable_terminal_for_update = "SELECT record_json FROM processes
         WHERE {{retired_process_status(status)}}
           AND updated_at_ms < ?1
           AND (?2::BIGINT IS NULL OR change_seq <= ?2)
           AND consumer_hold_key IS NULL
               AND cascade_cursor IS NULL
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
                    (record_json::JSONB #> '{identity,definition_id}') = ?5)
               AND (?6::TEXT IS NULL OR
                    (record_json::JSONB #>> '{provenance,caused_by,occurrence_id}') = ?6)
               AND (?7::TEXT IS NULL OR
                    (record_json::JSONB #>> '{provenance,caused_by,subscription_id}') = ?7)
               AND (?8::BIGINT IS NULL OR created_at_ms >= ?8)
               AND (?9::BIGINT IS NULL OR created_at_ms < ?9)
               AND (?10::BIGINT IS NULL OR {{live_process_status(status)}}
                    OR updated_at_ms >= ?10)
             ORDER BY process_id ASC";
        /// The same, narrowed to lifetime scope `?11` / `?12`.
        list_by_lifetime_scope = "SELECT record_json FROM processes
             WHERE (?1::TEXT[] IS NULL OR status = ANY(?1))
               AND (?2::TEXT IS NULL OR originator_id = ?2)
               AND (?3::TEXT IS NULL OR identity_kind = ?3)
               AND (?4::TEXT IS NULL OR identity_label = ?4)
               AND (?5::JSONB IS NULL OR
                    (record_json::JSONB #> '{identity,definition_id}') = ?5)
               AND (?6::TEXT IS NULL OR
                    (record_json::JSONB #>> '{provenance,caused_by,occurrence_id}') = ?6)
               AND (?7::TEXT IS NULL OR
                    (record_json::JSONB #>> '{provenance,caused_by,subscription_id}') = ?7)
               AND (?8::BIGINT IS NULL OR created_at_ms >= ?8)
               AND (?9::BIGINT IS NULL OR created_at_ms < ?9)
               AND (?10::BIGINT IS NULL OR {{live_process_status(status)}}
                    OR updated_at_ms >= ?10)
               AND lifetime_scope_kind = ?11
               AND lifetime_scope_id IS NOT DISTINCT FROM ?12::TEXT
             ORDER BY process_id ASC";
        /// The same, narrowed to rows whose cancel request is older than `?11`
        /// and whose outcome is still open.
        list_pending_cancel = "SELECT record_json FROM processes
             WHERE (?1::TEXT[] IS NULL OR status = ANY(?1))
               AND (?2::TEXT IS NULL OR originator_id = ?2)
               AND (?3::TEXT IS NULL OR identity_kind = ?3)
               AND (?4::TEXT IS NULL OR identity_label = ?4)
               AND (?5::JSONB IS NULL OR
                    (record_json::JSONB #> '{identity,definition_id}') = ?5)
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
        list_by_lifetime_scope_pending_cancel = "SELECT record_json FROM processes
             WHERE (?1::TEXT[] IS NULL OR status = ANY(?1))
               AND (?2::TEXT IS NULL OR originator_id = ?2)
               AND (?3::TEXT IS NULL OR identity_kind = ?3)
               AND (?4::TEXT IS NULL OR identity_label = ?4)
               AND (?5::JSONB IS NULL OR
                    (record_json::JSONB #> '{identity,definition_id}') = ?5)
               AND (?6::TEXT IS NULL OR
                    (record_json::JSONB #>> '{provenance,caused_by,occurrence_id}') = ?6)
               AND (?7::TEXT IS NULL OR
                    (record_json::JSONB #>> '{provenance,caused_by,subscription_id}') = ?7)
               AND (?8::BIGINT IS NULL OR created_at_ms >= ?8)
               AND (?9::BIGINT IS NULL OR created_at_ms < ?9)
               AND (?10::BIGINT IS NULL OR {{live_process_status(status)}}
                    OR updated_at_ms >= ?10)
               AND lifetime_scope_kind = ?11
               AND lifetime_scope_id IS NOT DISTINCT FROM ?12::TEXT
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
        /// sequences from that bump in candidate order, and the process delete
        /// runs only if the tombstone count matches. Splitting it would let a
        /// status writer land between the parts.
        ///
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
                 process_id, terminal_label, pruned_at_ms, pruned_change_seq
             )
             SELECT candidate.process_id,
                    process.status,
                    ?2,
                    clock.current_seq - (SELECT count(*) FROM candidates) + candidate.ordinality
             FROM candidates AS candidate
             JOIN processes AS process USING (process_id)
             CROSS JOIN advanced_clock AS clock
             CROSS JOIN event_count
             WHERE event_count.value >= 0
             ORDER BY candidate.ordinality
             RETURNING process_id
         ),
         deleted_processes AS (
             DELETE FROM processes AS process
             USING candidates AS candidate,
                   (SELECT count(*) FROM inserted_tombstones) AS tombstones
             WHERE process.process_id = candidate.process_id
               AND tombstones.count = (SELECT count(*) FROM candidates)
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
        insert_if_absent = "INSERT INTO process_observers (session_id, process_id)
             VALUES (?1, ?2) ON CONFLICT DO NOTHING";
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
        /// after the process row has been pruned.
        select_max_compactable_change_seq = "SELECT MAX(pruned_change_seq) FROM process_tombstones
             WHERE pruned_at_ms < ?1
               AND (?2::BIGINT IS NULL OR pruned_change_seq <= ?2)
               AND NOT (process_id = ANY(?3::TEXT[]))";

        /// Delete exactly the rows
        /// [`TombstonePostgresStatements::select_max_compactable_change_seq`]
        /// measured.
        delete_compactable = "DELETE FROM process_tombstones
             WHERE pruned_at_ms < ?1
               AND (?2::BIGINT IS NULL OR pruned_change_seq <= ?2)
               AND NOT (process_id = ANY(?3::TEXT[]))";
    }
}

lash_store_sql::statements! {
    /// `parent_end_plans` statements only PostgreSQL issues.
    pub(crate) struct ParentEndPlanPostgresStatements @ "parent_end_plan" {
        /// Reclaim plans of scopes that ended before `?1` and that no live
        /// child still names.
        ///
        /// The alias is the fork: PostgreSQL cannot name the table it is
        /// deleting from inside a correlated subquery without one, and SQLite
        /// cannot use one at all in a `DELETE`.
        delete_reclaimable = "DELETE FROM parent_end_plans AS plan
         WHERE plan.ended_at_ms < ?1
           AND NOT EXISTS (
               SELECT 1 FROM processes AS child
               WHERE child.lifetime_scope_kind = plan.parent_kind
                 AND child.lifetime_scope_id = plan.parent_id
                 AND {{live_process_status(child.status)}}
           )";
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
    /// `process_events` statements both backends issue verbatim.
    pub(crate) event: EventStatements,
    /// `process_event_horizons` statements both backends issue verbatim.
    pub(crate) event_horizon: EventHorizonStatements,
    /// `process_observers` statements both backends issue verbatim.
    pub(crate) observer: ObserverStatements,
    /// `process_observers` statements only PostgreSQL issues.
    pub(crate) observer_postgres: ObserverPostgresStatements,
    /// `process_tombstones` statements both backends issue verbatim.
    pub(crate) tombstone: TombstoneStatements,
    /// `process_tombstones` statements only PostgreSQL issues.
    pub(crate) tombstone_postgres: TombstonePostgresStatements,
    /// `process_change_clock` statements, all of them PostgreSQL's own.
    pub(crate) clock_postgres: ChangeClockPostgresStatements,
    /// `parent_end_plans` statements both backends issue verbatim.
    pub(crate) plan: ParentEndPlanStatements,
    /// `abandoned_consumer_holds` statements, all of them shared.
    pub(crate) abandoned_hold: AbandonedConsumerHoldStatements,
    /// `parent_end_plans` statements only PostgreSQL issues.
    pub(crate) plan_postgres: ParentEndPlanPostgresStatements,
}

static PROCESS_SQL: LazyLock<ProcessSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres().with_vocabulary(PROCESS_LIFECYCLE);
    ProcessSql {
        process: ProcessStatements::render(dialect),
        process_postgres: ProcessPostgresStatements::render(dialect),
        registry_postgres: ProcessRegistryPostgresStatements::render(dialect),
        event: EventStatements::render(dialect),
        event_horizon: EventHorizonStatements::render(dialect),
        observer: ObserverStatements::render(dialect),
        observer_postgres: ObserverPostgresStatements::render(dialect),
        tombstone: TombstoneStatements::render(dialect),
        tombstone_postgres: TombstonePostgresStatements::render(dialect),
        clock_postgres: ChangeClockPostgresStatements::render(dialect),
        plan: ParentEndPlanStatements::render(dialect),
        abandoned_hold: AbandonedConsumerHoldStatements::render(dialect),
        plan_postgres: ParentEndPlanPostgresStatements::render(dialect),
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
        filter.until.is_some(),
        filter.cancel_pending_before_ms.is_some(),
    ) {
        (false, false) => statements.list.sql(),
        (true, false) => statements.list_by_lifetime_scope.sql(),
        (false, true) => statements.list_pending_cancel.sql(),
        (true, true) => statements.list_by_lifetime_scope_pending_cancel.sql(),
    }
}
