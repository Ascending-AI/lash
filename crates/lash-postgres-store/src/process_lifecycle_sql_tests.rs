//! Byte-identity witnesses for the PostgreSQL statements FIG-2844 generated.
//!
//! Each assertion pins the exact predicate text — indentation included — that
//! the statement carried before the cutover, so a generated fragment cannot
//! change a query's bytes without failing here. The fragments themselves are
//! pinned in `lash_core::store_backend_support::process_lifecycle_sql`.

use crate::preflight::walk::{PARKED_SEGMENT_SQL, PENDING_WAKE_SQL};
use crate::process_helpers::INSERT_WAKE_DELIVERY_SQL;
use crate::process_registry::prune_api::PRUNABLE_TERMINAL_SELECT;
use crate::process_registry::wake_delivery::{
    RECLAIM_LAPSED_WAKE_CLAIMS_SQL, SELECT_CLAIMABLE_WAKE_SQL, SETTLE_WAKE_CLAIM_SQL,
    START_WAKE_ENQUEUING_SQL,
};
use crate::process_registry::worklist::{
    COLLECT_NON_TERMINAL_SQL, CONTINUE_WORKLIST_PAGE_SQL, COUNT_NON_TERMINAL_SQL,
    FIRST_WORKLIST_PAGE_SQL, MAX_WORKLIST_PROCESS_ID_SQL,
};
use crate::process_registry::{
    DISCARD_RETARGETED_WAKES_SQL, DISCARD_TARGET_GONE_WAKES_SQL, LIST_OBSERVED_SQL,
    LIST_PROCESSES_SQL, REDRIVE_DISCARDED_WAKE_SQL, RELEASE_WAKE_CLAIM_SQL,
};

#[test]
fn worklist_statements_keep_their_previous_bytes() {
    assert_eq!(
        COUNT_NON_TERMINAL_SQL.as_str(),
        "SELECT COUNT(*) FROM lash_processes WHERE status IN ('running', 'waiting')"
    );
    assert_eq!(
        MAX_WORKLIST_PROCESS_ID_SQL.as_str(),
        "SELECT MAX(process_id) FROM lash_processes WHERE status IN ('running', 'waiting')"
    );
    assert_eq!(
        FIRST_WORKLIST_PAGE_SQL.as_str(),
        "SELECT record_json FROM lash_processes
     WHERE status IN ('running', 'waiting') AND process_id <= $1
     ORDER BY process_id ASC LIMIT $2"
    );
    assert_eq!(
        CONTINUE_WORKLIST_PAGE_SQL.as_str(),
        "SELECT record_json FROM lash_processes
     WHERE status IN ('running', 'waiting')
       AND process_id <= $1 AND process_id > $2
     ORDER BY process_id ASC LIMIT $3"
    );
    assert_eq!(
        COLLECT_NON_TERMINAL_SQL.as_str(),
        "SELECT record_json FROM lash_processes
         WHERE status IN ('running', 'waiting')
         ORDER BY process_id ASC"
    );
}

#[test]
fn list_and_prune_statements_keep_their_previous_predicates() {
    assert!(LIST_PROCESSES_SQL.contains(
        "               AND ($10::BIGINT IS NULL OR status IN ('running', 'waiting')\n                    OR updated_at_ms >= $10)\n"
    ));
    assert!(
        LIST_PROCESSES_SQL
            .contains("(record_json::JSONB #> '{identity,definition,definition}') = $5")
    );
    assert!(LIST_OBSERVED_SQL.contains(
        "               AND ($3::BIGINT IS NULL OR p.status IN ('running', 'waiting')\n                    OR p.updated_at_ms >= $3)\n"
    ));
    assert!(
        PRUNABLE_TERMINAL_SELECT.contains("         WHERE status NOT IN ('running', 'waiting')\n")
    );
    assert!(
        PRUNABLE_TERMINAL_SELECT
            .contains("                 AND delivery.state IN ('pending', 'enqueuing')\n")
    );
}

#[test]
fn preflight_statements_keep_their_previous_predicates() {
    assert!(PARKED_SEGMENT_SQL.contains("     WHERE processes.status IN ('running', 'waiting')\n"));
    assert!(PENDING_WAKE_SQL.contains("     WHERE state IN ('pending', 'enqueuing')\n"));
}

#[test]
fn wake_delivery_statements_keep_their_previous_bytes() {
    assert_eq!(
        RECLAIM_LAPSED_WAKE_CLAIMS_SQL.as_str(),
        "UPDATE lash_process_wake_deliveries
         SET state = 'pending', claim_token = NULL
         WHERE state = 'enqueuing' AND next_attempt_at_ms <= $1"
    );
    assert_eq!(
        START_WAKE_ENQUEUING_SQL.as_str(),
        "UPDATE lash_process_wake_deliveries
             SET state = 'enqueuing',
                 claim_token = $4,
                 attempts = attempts + 1,
                 first_attempt_ms = COALESCE(first_attempt_ms, $2),
                 next_attempt_at_ms = $3
             WHERE delivery_id = $1 AND state = 'pending'"
    );
    assert_eq!(
        SETTLE_WAKE_CLAIM_SQL.as_str(),
        "UPDATE lash_process_wake_deliveries
         SET state = $3, claim_token = NULL, discard_reason = $4
         WHERE delivery_id = $1 AND state = 'enqueuing' AND claim_token = $2"
    );
    assert_eq!(
        REDRIVE_DISCARDED_WAKE_SQL.as_str(),
        "UPDATE lash_process_wake_deliveries
             SET state = 'pending', attempts = 0, first_attempt_ms = NULL,
                 claim_token = NULL, next_attempt_at_ms = $3, expires_at_ms = $2,
                 discard_reason = NULL
             WHERE delivery_id = $1 AND state = 'discarded'"
    );
    assert_eq!(
        RELEASE_WAKE_CLAIM_SQL.as_str(),
        "UPDATE lash_process_wake_deliveries
             SET state = 'pending', claim_token = NULL, next_attempt_at_ms = $3
             WHERE delivery_id = $1 AND state = 'enqueuing' AND claim_token = $2"
    );
    assert_eq!(
        DISCARD_TARGET_GONE_WAKES_SQL.as_str(),
        "UPDATE lash_process_wake_deliveries
             SET state = 'discarded', discard_reason = 'target_gone'
             WHERE target_session_id = $1 AND state = 'pending'"
    );
    assert_eq!(
        DISCARD_RETARGETED_WAKES_SQL.as_str(),
        "UPDATE lash_process_wake_deliveries
                 SET state = 'discarded', discard_reason = 'retargeted'
                 WHERE process_id = $1 AND target_session_id = $2 AND state = 'pending'"
    );
    assert_eq!(
        INSERT_WAKE_DELIVERY_SQL.as_str(),
        "INSERT INTO lash_process_wake_deliveries (
            delivery_id, process_id, process_incarnation, target_session_id, sequence, state,
            claim_token, attempts, first_attempt_ms, next_attempt_at_ms, expires_at_ms,
            discard_reason, delivery_json
         ) VALUES ($1, $2, $3, $4, $5, 'pending', NULL, 0, NULL, $6, $7, NULL, $8)
         ON CONFLICT (delivery_id) DO NOTHING"
    );
    assert!(SELECT_CLAIMABLE_WAKE_SQL.contains("         WHERE candidate.state = 'pending'\n"));
    assert!(
        SELECT_CLAIMABLE_WAKE_SQL.contains("               WHERE earlier.state <> 'enqueued'\n")
    );
    assert!(
        SELECT_CLAIMABLE_WAKE_SQL.contains("                     earlier.state = 'discarded'\n")
    );
}

/// FIG-3399: the same predicates, reached through the renderer's vocabulary
/// axis instead of a `format!`.
///
/// A neutral statement names a lifecycle predicate as a `{{term(column)}}`
/// token; the backend supplies the expansions from the one source this
/// repository has for them, and the renderer expands them once. These tests
/// are the proof that the axis is byte-faithful: the rendered statements are
/// character for character what the `format!` sites produce today, and the
/// rendered predicates are character for character what the schema's partial
/// indexes declare. No production statement moves here — FIG-3384 owns that.
mod vocabulary_tokens {
    use super::{
        COLLECT_NON_TERMINAL_SQL, CONTINUE_WORKLIST_PAGE_SQL, COUNT_NON_TERMINAL_SQL,
        FIRST_WORKLIST_PAGE_SQL, MAX_WORKLIST_PROCESS_ID_SQL,
    };
    use lash_core::store_backend_support as vocabulary;
    use lash_store_sql::{Dialect, Vocabulary, VocabularyTerm, render};

    /// How a backend registers its expansions: one entry per term, each the
    /// `lash-core` helper that already generates it from the enum. The term
    /// names are the same on both backends, because the vocabulary is the
    /// domain's, not a dialect's.
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

    fn rendered(neutral: &str) -> String {
        render(
            neutral,
            Dialect::postgres().with_vocabulary(PROCESS_LIFECYCLE),
            &["processes", "process_wake_deliveries"],
        )
        .expect("neutral statement renders")
    }

    #[test]
    fn a_worklist_statement_renders_to_the_bytes_the_format_site_produces() {
        assert_eq!(
            rendered("SELECT COUNT(*) FROM processes WHERE {{live_process_status(status)}}"),
            COUNT_NON_TERMINAL_SQL.as_str()
        );
        assert_eq!(
            rendered("SELECT MAX(process_id) FROM processes WHERE {{live_process_status(status)}}"),
            MAX_WORKLIST_PROCESS_ID_SQL.as_str()
        );
        assert_eq!(
            rendered(
                "SELECT record_json FROM processes
     WHERE {{live_process_status(status)}} AND process_id <= ?1
     ORDER BY process_id ASC LIMIT ?2"
            ),
            FIRST_WORKLIST_PAGE_SQL.as_str()
        );
        assert_eq!(
            rendered(
                "SELECT record_json FROM processes
     WHERE {{live_process_status(status)}}
       AND process_id <= ?1 AND process_id > ?2
     ORDER BY process_id ASC LIMIT ?3"
            ),
            CONTINUE_WORKLIST_PAGE_SQL.as_str()
        );
        assert_eq!(
            rendered(
                "SELECT record_json FROM processes
         WHERE {{live_process_status(status)}}
         ORDER BY process_id ASC"
            ),
            COLLECT_NON_TERMINAL_SQL.as_str()
        );
    }

    /// Every partial index whose `WHERE` is lifecycle vocabulary, matched
    /// against what a token renders. A partial index only helps a query whose
    /// predicate matches it byte for byte, so this is the assertion the
    /// vocabulary axis has to hold to be usable at all.
    #[test]
    fn every_vocabulary_partial_index_predicate_is_what_a_token_renders() {
        let schema = crate::PostgresStorage::schema_ddl();
        for (index, declared) in [
            (
                "idx_lash_processes_live_worklist",
                format!(
                    "    ON lash_processes(process_id) WHERE {};",
                    rendered("{{live_process_status(status)}}")
                ),
            ),
            (
                "idx_lash_processes_pending_cancel",
                format!(
                    "    WHERE cancel_requested_at_ms IS NOT NULL\n      AND {};",
                    rendered("{{nonterminal_process_status(status)}}")
                ),
            ),
            (
                "idx_lash_processes_parent_end_pending",
                format!(
                    "      AND cancel_requested_at_ms IS NULL\n      AND {};",
                    rendered("{{live_process_status(status)}}")
                ),
            ),
            (
                "idx_lash_wake_deliveries_pending",
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
