//! Byte-identity witnesses for the SQLite statements FIG-2844 generated.
//!
//! Each assertion pins the exact predicate text — indentation included — that
//! the statement carried before the cutover, so a generated fragment cannot
//! change a query's bytes without failing here. The fragments themselves are
//! pinned in `lash_core_execution::store_backend_support::process_lifecycle_sql`.
//!
//! FIG-3384 moved these statements onto the single-owner layout: the
//! predicates are now `{{term(column)}}` tokens the renderer expands at
//! startup rather than `format!` holes. The assertions are unchanged, and
//! that is the point — they now witness that the rendered statement is
//! character for character what the `format!` site produced.

use crate::process_registry::sql::process_sql;

#[test]
fn worklist_statements_keep_their_previous_bytes() {
    let process = &process_sql().process_sqlite;
    assert_eq!(
        process.count_live_worklist.sql(),
        "SELECT COUNT(*) FROM processes INDEXED BY idx_processes_live_worklist
     WHERE status IN ('running', 'waiting')"
    );
    assert_eq!(
        process.select_max_worklist_process_id.sql(),
        "SELECT MAX(process_id) FROM processes INDEXED BY idx_processes_live_worklist
     WHERE status IN ('running', 'waiting')"
    );
    assert_eq!(
        process.list_first_worklist_page.sql(),
        "SELECT record_json FROM processes
     INDEXED BY idx_processes_live_worklist
     WHERE status IN ('running', 'waiting') AND process_id <= ?1
     ORDER BY process_id ASC LIMIT ?3"
    );
    assert_eq!(
        process.list_next_worklist_page.sql(),
        "SELECT record_json FROM processes
     INDEXED BY idx_processes_live_worklist
     WHERE status IN ('running', 'waiting')
       AND process_id <= ?1 AND process_id > ?2
     ORDER BY process_id ASC LIMIT ?3"
    );
    assert_eq!(
        process_sql().process.collect_non_terminal_records.sql(),
        "SELECT record_json FROM processes
                         WHERE status IN ('running', 'waiting')
                         ORDER BY process_id ASC"
    );
}

#[test]
fn list_and_prune_statements_keep_their_previous_predicates() {
    let sql = process_sql();
    let recent_retired = sql.process_sqlite.list_recent_retired.sql();
    assert!(recent_retired.contains("         WHERE status IN ('running', 'waiting')\n"));
    assert!(recent_retired.contains(
        "         WHERE status NOT IN ('running', 'waiting')\n           AND updated_at_ms >= ?10\n"
    ));
    let observed_recent_retired = sql.registry_sqlite.list_observed_recent_retired.sql();
    assert!(
        observed_recent_retired.contains("         WHERE p.status IN ('running', 'waiting')\n")
    );
    assert!(observed_recent_retired.contains(
        "         WHERE p.status NOT IN ('running', 'waiting') AND p.updated_at_ms >= ?3\n"
    ));
    assert!(sql.registry_sqlite.list_observed.sql().contains(
        "                       AND (?3 IS NULL OR p.status IN ('running', 'waiting')\n"
    ));
    let prunable = sql.process_sqlite.list_prunable_terminal.sql();
    assert!(prunable.contains("             WHERE status NOT IN ('running', 'waiting')\n"));
    assert!(
        prunable.contains("                     AND delivery.state IN ('pending', 'enqueuing')\n")
    );
}

#[test]
fn preflight_statements_keep_their_previous_predicates() {
    let sql = process_sql();
    assert!(
        sql.handover_sqlite
            .list_parked_segments
            .sql()
            .contains("    WHERE processes.status IN ('running', 'waiting')\n")
    );
    assert!(
        sql.wake_sqlite
            .list_undelivered_for_walk
            .sql()
            .contains("WHERE state IN ('pending', 'enqueuing')\n")
    );
}

#[test]
fn wake_delivery_statements_keep_their_previous_bytes() {
    let sql = process_sql();
    assert_eq!(
        sql.wake.reclaim_lapsed_claims.sql(),
        "UPDATE process_wake_deliveries
                         SET state = 'pending', claim_token = NULL
                         WHERE state = 'enqueuing' AND next_attempt_at_ms <= ?1"
    );
    assert_eq!(
        sql.wake.start_enqueuing.sql(),
        "UPDATE process_wake_deliveries
                             SET state = 'enqueuing',
                                 claim_token = ?4,
                                 attempts = attempts + 1,
                                 first_attempt_ms = COALESCE(first_attempt_ms, ?2),
                                 next_attempt_at_ms = ?3
                             WHERE delivery_id = ?1 AND state = 'pending'"
    );
    assert_eq!(
        sql.wake.redrive_discarded.sql(),
        "UPDATE process_wake_deliveries
                             SET state = 'pending', attempts = 0, first_attempt_ms = NULL,
                                 claim_token = NULL, next_attempt_at_ms = ?3, expires_at_ms = ?2,
                                 discard_reason = NULL
                             WHERE delivery_id = ?1 AND state = 'discarded'"
    );
    assert_eq!(
        sql.wake.release_claim.sql(),
        "UPDATE process_wake_deliveries
                             SET state = 'pending', claim_token = NULL, next_attempt_at_ms = ?3
                             WHERE delivery_id = ?1 AND state = 'enqueuing' AND claim_token = ?2"
    );
    assert_eq!(
        sql.wake.settle_claim.sql(),
        "UPDATE process_wake_deliveries
                     SET state = ?3, claim_token = NULL, discard_reason = ?4
                     WHERE delivery_id = ?1 AND state = 'enqueuing' AND claim_token = ?2"
    );
    assert_eq!(
        sql.wake.discard_target_gone.sql(),
        "UPDATE process_wake_deliveries
                             SET state = 'discarded', discard_reason = 'target_gone'
                             WHERE target_session_id = ?1 AND state = 'pending'"
    );
    assert_eq!(
        sql.wake.discard_retargeted.sql(),
        "UPDATE process_wake_deliveries
                             SET state = 'discarded', discard_reason = 'retargeted'
                             WHERE process_id = ?1 AND target_session_id = ?2 AND state = 'pending'"
    );
    assert_eq!(
        sql.wake_sqlite.insert_pending.sql(),
        "INSERT OR IGNORE INTO process_wake_deliveries (
                delivery_id, process_id, process_incarnation, target_session_id, sequence, state,
                claim_token, attempts, first_attempt_ms, next_attempt_at_ms, expires_at_ms,
                discard_reason, delivery_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', NULL, 0, NULL, ?6, ?7, NULL, ?8)"
    );
    let claimable = sql.wake_sqlite.select_claimable.sql();
    assert!(claimable.contains("WHERE candidate.state = 'pending'\n"));
    assert!(claimable.contains("WHERE earlier.state <> 'enqueued'\n"));
    assert!(claimable.contains("earlier.state = 'discarded'\n"));
}

/// FIG-3399: the same predicates, reached through the renderer's vocabulary
/// axis instead of a `format!`.
///
/// A neutral statement names a lifecycle predicate as a `{{term(column)}}`
/// token; the backend supplies the expansions from the one source this
/// repository has for them, and the renderer expands them once. These tests
/// are the proof that the axis is byte-faithful: the rendered predicates are
/// character for character what the schema's partial indexes declare. The
/// statement-level half of that proof is the assertions above, which pin the
/// bytes the `format!` sites produced before FIG-3384 moved them here.
mod vocabulary_tokens {
    use lash_core_execution::store_backend_support as vocabulary;
    use lash_store_sql::{Dialect, Vocabulary, VocabularyTerm, render};

    /// How a backend registers its expansions: one entry per term, each the
    /// `lash-core` helper that already generates it from the enum.
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
    ]);

    /// The process registry lives on one connection, so its tables are
    /// addressed unqualified — the spelling its `INDEXED BY` plans were
    /// measured against.
    fn rendered(neutral: &str) -> String {
        render(
            neutral,
            Dialect::sqlite_unqualified().with_vocabulary(PROCESS_LIFECYCLE),
            &["processes", "process_wake_deliveries"],
        )
        .expect("neutral statement renders")
    }

    /// Every partial index whose `WHERE` is lifecycle vocabulary, matched
    /// against what a token renders. A partial index only helps a query whose
    /// predicate matches it byte for byte, so this is the assertion the
    /// vocabulary axis has to hold to be usable at all.
    #[test]
    fn every_vocabulary_partial_index_predicate_is_what_a_token_renders() {
        let schema = crate::schema::PROCESS_SCHEMA;
        for (index, declared) in [
            (
                "idx_processes_live_worklist",
                format!(
                    "    ON processes(process_id) WHERE {};",
                    rendered("{{live_process_status(status)}}")
                ),
            ),
            (
                "idx_processes_recent_retired",
                format!(
                    "    ON processes(updated_at_ms, process_id)\n    WHERE {};",
                    rendered("{{retired_process_status(status)}}")
                ),
            ),
            (
                "idx_processes_pending_cancel",
                format!(
                    "    WHERE cancel_requested_at_ms IS NOT NULL\n      AND {};",
                    rendered("{{nonterminal_process_status(status)}}")
                ),
            ),
            (
                "idx_processes_parent_end_pending",
                format!(
                    "      AND cancel_requested_at_ms IS NULL\n      AND {};",
                    rendered("{{live_process_status(status)}}")
                ),
            ),
            (
                "idx_wake_deliveries_pending",
                format!(
                    "    WHERE {};",
                    rendered("{{undelivered_wake_delivery_state(state)}}")
                ),
            ),
        ] {
            assert!(
                schema.contains(&declared),
                "`{index}`'s declared predicate is not what the token renders:\n{declared}"
            );
        }
    }
}
