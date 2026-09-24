//! What the turn-ingress family's PostgreSQL statements are held to without a
//! server.
//!
//! The behavioural proofs live in the conformance and cross-backend suites,
//! which need a real PostgreSQL. What can be proved here is what the renderer
//! produced: that every predicate generated from the lifecycle vocabulary is
//! byte-identical to what its one generator spells, and that the statements
//! carry the placeholder style and table prefix this backend was rendered for.

use super::turn_ingress_sql;
use lash_core_execution::store_backend_support as vocabulary;

#[test]
fn every_statement_renders_for_this_backend() {
    // Rendering happens once, lazily; touching the set is what makes a
    // malformed neutral statement a test failure here rather than a startup
    // failure in front of a caller.
    let sql = turn_ingress_sql();
    assert!(
        sql.pending_inputs
            .select_by_id
            .sql()
            .contains("lash_pending_turn_inputs")
    );
    assert!(sql.pending_inputs.select_by_id.sql().contains("$1"));
    assert!(!sql.pending_inputs.select_by_id.sql().contains("?1"));
    assert!(
        sql.queued_batches
            .list_by_session
            .sql()
            .contains("lash_queued_work_batches")
    );
}

#[test]
fn a_state_token_renders_to_the_predicate_its_generator_spells() {
    // A `{{term(column)}}` token is only worth having if it renders to exactly
    // what the generator produces: the enum stays the one source of the
    // vocabulary, and the two backends' predicates cannot drift apart.
    let sql = turn_ingress_sql();
    assert!(sql.pending_inputs.delete_terminal.sql().contains(
        &vocabulary::terminal_turn_input_state_predicate_sql("state")
    ),);
    assert!(sql.pending_inputs.settle_unclaimed.sql().contains(
        &vocabulary::nonterminal_turn_input_state_predicate_sql("state")
    ),);
    assert!(sql.pending_inputs.list_undelivered.sql().contains(
        &vocabulary::undelivered_turn_input_state_predicate_sql("state")
    ),);
    assert!(
        sql.pending_inputs_postgres
            .select_active_turn_rows
            .sql()
            .contains(&vocabulary::active_turn_input_state_predicate_sql("state")),
    );
}

#[test]
fn a_checkpoint_statement_spells_the_boundary_its_generator_spells() {
    // The minimum-boundary predicate cannot be a vocabulary token: its column
    // is this backend's `jsonb` extraction, not an identifier. So the
    // statements spell it, and this holds the spelling to
    // `admitted_min_boundary_sql` — the one place the boundary enum reaches
    // SQL.
    let sql = turn_ingress_sql();
    let expression = "ingress_json::jsonb ->> 'min_boundary'";
    for (statement, checkpoint) in [
        (
            &sql.pending_inputs_postgres
                .claim_candidates_active_turn_after_work,
            lash_core_execution::CheckpointKind::AfterWork,
        ),
        (
            &sql.pending_inputs_postgres
                .claim_candidates_active_turn_before_completion,
            lash_core_execution::CheckpointKind::BeforeCompletion,
        ),
        (
            &sql.family_postgres.checkpoint_work_pending_after_work,
            lash_core_execution::CheckpointKind::AfterWork,
        ),
        (
            &sql.family_postgres
                .checkpoint_work_pending_before_completion,
            lash_core_execution::CheckpointKind::BeforeCompletion,
        ),
    ] {
        let admitted = vocabulary::admitted_min_boundary_sql(expression, checkpoint);
        let spelled = statement
            .sql()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let admitted = admitted.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            spelled.contains(&admitted),
            "`{}` does not spell `{admitted}`",
            statement.name(),
        );
    }
}

#[test]
fn the_checkpoint_probe_keeps_the_server_clock_as_its_fallback() {
    // The probe runs outside a transaction, so it cannot share a sampled
    // `transaction_timestamp()` with a sibling statement. It binds an injected
    // test epoch and falls back to the server clock, which is what it always
    // read; a production build binds NULL. The indexed column stays on the
    // left of the comparison so the ready index is still seekable.
    let sql = turn_ingress_sql();
    for statement in [
        &sql.family_postgres.checkpoint_work_pending_after_work,
        &sql.family_postgres
            .checkpoint_work_pending_before_completion,
    ] {
        let spelled = statement
            .sql()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            spelled.contains(
                "available_at_ms <= COALESCE( $4, FLOOR(EXTRACT(EPOCH FROM \
                 transaction_timestamp()) * 1000))"
            ),
            "`{}` no longer falls back to the server clock:\n{spelled}",
            statement.name(),
        );
    }
    // With no lease clock injected the probe binds NULL and reads the server
    // clock; in a production build the seam has no parameter at all.
    assert_eq!(super::injected_lease_epoch_ms(None), None);
}
