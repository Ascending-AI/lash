//! What the turn-ingress family's SQLite statements are held to.
//!
//! Two classes. The **plan** tests run `EXPLAIN QUERY PLAN` against the real
//! schema and assert each named statement still seeks the index it was written
//! for: the statements that replaced a `format!` used to splice their filter in
//! per call, and the whole reason each filter shape has its own name is that a
//! spliced optional predicate cannot seek. The **byte-identity** tests pin the
//! predicates the renderer now produces to the one source that generates them,
//! the way FIG-2844's witnesses do for the process family.

use rusqlite::Connection;

use super::{closure_participant_sql, tool_intent_sql, turn_ingress_sql};
use crate::scope_fence::Schema;

/// A catalog with this crate's real session schema, in memory.
fn catalog() -> Connection {
    let conn = Connection::open_in_memory().expect("in-memory database opens");
    conn.execute_batch(crate::schema::SCHEMA)
        .expect("session schema applies");
    conn
}

/// The plan `sql` produces, as one line per step.
///
/// `EXPLAIN QUERY PLAN` is the only way to see whether a predicate seeks: a
/// statement that scans and a statement that seeks return the same rows, and
/// only the plan tells them apart.
fn plan(conn: &Connection, sql: &str) -> String {
    let mut statement = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap_or_else(|error| panic!("statement prepares: {error}\n{sql}"));
    // A plan is produced from the statement's shape, not its values, but SQLite
    // still wants every parameter bound before it will produce one.
    let bindings = (1..=statement.parameter_count())
        .map(|_| rusqlite::types::Value::Null)
        .collect::<Vec<_>>();
    let rows = statement
        .query_map(rusqlite::params_from_iter(bindings.iter()), |row| {
            row.get::<_, String>(3)
        })
        .expect("plan rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("plan rows decode");
    rows.join("\n")
}

fn assert_uses(conn: &Connection, statement: &lash_store_sql::Rendered, index: &str) {
    let plan = plan(conn, statement.sql());
    assert!(
        plan.contains(index),
        "`{}` no longer uses `{index}`:\n{plan}",
        statement.name(),
    );
}

#[test]
fn every_turn_ingress_statement_prepares_against_the_real_schema() {
    // Rendering proves the neutral text is well-formed; only the database
    // proves it is SQL over the columns this schema has. A statement that
    // names a dropped column renders happily and fails here.
    let conn = catalog();
    let sql = turn_ingress_sql();
    for statement in [
        sql.family.has_claimable_work.sql(),
        sql.family.pending_session_work_ordering.sql(),
        sql.family_sqlite.checkpoint_work_pending_after_work.sql(),
        sql.family_sqlite
            .checkpoint_work_pending_before_completion
            .sql(),
        sql.pending_inputs.select_by_id.sql(),
        sql.pending_inputs.select_by_source_key.sql(),
        sql.pending_inputs.list_undelivered.sql(),
        sql.pending_inputs.cancel.sql(),
        sql.pending_inputs.defer_to_next_turn.sql(),
        sql.pending_inputs.claim.sql(),
        sql.pending_inputs.settle_claimed.sql(),
        sql.pending_inputs.settle_unclaimed.sql(),
        sql.pending_inputs.delete_terminal.sql(),
        sql.pending_inputs.delete_by_session.sql(),
        sql.pending_inputs_sqlite.insert_new.sql(),
        sql.pending_inputs_sqlite.select_id_by_source_key.sql(),
        sql.pending_inputs_sqlite.select_session_by_input_id.sql(),
        sql.pending_inputs_sqlite.settlement_facts.sql(),
        sql.pending_inputs_sqlite.select_suffix.sql(),
        sql.pending_inputs_sqlite.select_active_turn_claims.sql(),
        sql.pending_inputs_sqlite.select_active_turn_rows.sql(),
        sql.pending_inputs_sqlite.select_pending_active.sql(),
        sql.pending_inputs_sqlite.claim_candidates_next_turn.sql(),
        sql.pending_inputs_sqlite
            .claim_candidates_active_turn_after_work
            .sql(),
        sql.pending_inputs_sqlite
            .claim_candidates_active_turn_before_completion
            .sql(),
        sql.pending_inputs_sqlite.abandon_claim.sql(),
        sql.pending_inputs_sqlite.abandon_claims.sql(),
        sql.queued_batches.select_by_id.sql(),
        sql.queued_batches.select_id_by_source_key.sql(),
        sql.queued_batches.list_by_session.sql(),
        sql.queued_batches.list_unclaimed.sql(),
        sql.queued_batches.claim.sql(),
        sql.queued_batches.abandon_claim.sql(),
        sql.queued_batches.settle_claimed.sql(),
        sql.queued_batches.delete_by_session.sql(),
        sql.queued_batches_sqlite.insert_new.sql(),
        sql.queued_batches_sqlite.settlement_facts.sql(),
        sql.queued_batches_sqlite.select_cancelable.sql(),
        sql.queued_batches_sqlite.delete_cancelled.sql(),
        sql.queued_batches_sqlite.select_head_candidate.sql(),
        sql.queued_batches_sqlite.exists_deferred.sql(),
        sql.queued_batches_sqlite.claim_candidates_idle.sql(),
        sql.queued_batches_sqlite.claim_candidates_boundary.sql(),
        sql.queued_batches_sqlite.select_present_ids.sql(),
        sql.queued_batches_sqlite.select_by_ids.sql(),
        sql.queued_batches_sqlite.select_by_claim_ids.sql(),
        sql.queued_batches_sqlite.select_span.sql(),
        sql.queued_items.insert_new.sql(),
        sql.queued_items.list_by_batch.sql(),
        sql.queued_items_sqlite.list_by_batches.sql(),
        sql.leases.select_by_session.sql(),
        sql.leases.acquire.sql(),
        sql.leases.reenter.sql(),
        sql.leases.renew.sql(),
        sql.leases.release.sql(),
        sql.leases.delete_by_session.sql(),
        sql.cancel_requests.delete_by_session.sql(),
        sql.cancel_requests.advance_intent_revision.sql(),
        sql.cancel_requests_sqlite.insert_first.sql(),
        sql.cancel_requests_sqlite.select_record.sql(),
        sql.cancel_requests_sqlite.select_record_with_revision.sql(),
        sql.cancel_requests_sqlite.update_record.sql(),
        sql.cancel_requests_sqlite.upsert_record.sql(),
        sql.bindings.select_by_session.sql(),
        sql.bindings.delete_by_session.sql(),
        sql.bindings_sqlite.insert_new.sql(),
        sql.closures.insert_new.sql(),
        sql.closures.list_by_session.sql(),
        sql.closures.list_all.sql(),
        sql.closures.count_by_session.sql(),
        sql.closures.delete_by_turn.sql(),
        sql.closures.delete_settled.sql(),
        sql.closures.delete_by_session.sql(),
        sql.closures_sqlite.select_by_turn.sql(),
        sql.retired_scopes.exists_for_scope.sql(),
        sql.retired_scopes_sqlite.insert_new.sql(),
    ] {
        conn.prepare(statement)
            .unwrap_or_else(|error| panic!("statement prepares: {error}\n{statement}"));
    }
}

#[test]
fn every_closure_participant_statement_prepares_against_the_effect_schema() {
    // The cancellation-closure participant ledger lives in the effect journal's
    // database, which a host reaches as its own `main` and a catalog reaches as
    // an attached `effect_journal`; the statement is rendered once per schema.
    let conn = Connection::open_in_memory().expect("in-memory database opens");
    conn.execute_batch(crate::schema::EFFECT_SCHEMA)
        .expect("effect schema applies");
    let sql = closure_participant_sql(Schema::Main);
    for statement in [
        sql.insert_new.sql(),
        sql.delete_participant.sql(),
        sql.exists_for_scope.sql(),
    ] {
        conn.prepare(statement)
            .unwrap_or_else(|error| panic!("statement prepares: {error}\n{statement}"));
    }
}

#[test]
fn every_tool_intent_statement_prepares_against_the_process_schema() {
    // The tool-intent ledger lives in the process registry's own database, so
    // it is rendered against that schema, unqualified, and proved against it.
    let conn = Connection::open_in_memory().expect("in-memory database opens");
    conn.execute_batch(crate::schema::PROCESS_SCHEMA)
        .expect("process schema applies");
    let sql = tool_intent_sql();
    for statement in [
        sql.shared.select_by_replay_key.sql(),
        sql.shared.update_submission.sql(),
        sql.sqlite.insert_new.sql(),
    ] {
        conn.prepare(statement)
            .unwrap_or_else(|error| panic!("statement prepares: {error}\n{statement}"));
    }
}

#[test]
fn a_claim_candidate_scan_seeks_its_session_index() {
    // These three replaced one `format!` that spliced the mode's filter in per
    // call, including a `? AND state = '…'` disjunct a planner cannot use. Each
    // named shape has to seek `idx_pending_turn_inputs_session`, or naming them
    // separately bought nothing.
    let conn = catalog();
    let statements = &turn_ingress_sql().pending_inputs_sqlite;
    for statement in [
        &statements.claim_candidates_next_turn,
        &statements.claim_candidates_active_turn_after_work,
        &statements.claim_candidates_active_turn_before_completion,
    ] {
        assert_uses(&conn, statement, "idx_pending_turn_inputs_session");
    }
}

#[test]
fn abandoning_a_batch_of_claims_seeks_the_claim_index() {
    // The batch abandon binds its `(session_id, claim_id, claim_token)` triples
    // as a JSON array instead of building one `IN ((?,?,?), …)` per arity. The
    // row-value `IN (SELECT …)` is what keeps that seekable; a correlated
    // `EXISTS` over `json_each` would scan the table once per claim.
    let conn = catalog();
    assert_uses(
        &conn,
        &turn_ingress_sql().pending_inputs_sqlite.abandon_claims,
        "idx_pending_turn_inputs_claim",
    );
}

#[test]
fn a_json_array_list_bind_still_seeks_the_primary_key() {
    // The three exact-claim reads bound their batch ids as `IN (?, ?, …)` and
    // now bind one JSON array. A `json_each` list that the planner cannot turn
    // into a seek would make an exact claim scan the session's whole queue.
    let conn = catalog();
    let statements = &turn_ingress_sql().queued_batches_sqlite;
    for statement in [
        &statements.select_present_ids,
        &statements.select_by_ids,
        &statements.select_by_claim_ids,
    ] {
        let plan = plan(&conn, statement.sql());
        assert!(
            plan.contains("USING INDEX") || plan.contains("USING PRIMARY KEY"),
            "`{}` no longer seeks an index:\n{plan}",
            statement.name(),
        );
    }
}

#[test]
fn the_multi_batch_item_read_seeks_its_batch() {
    // The claim path hydrates a run of batches in one page; if the `json_each`
    // bind scans `queued_work_items` the page costs the whole table.
    let conn = catalog();
    let plan = plan(
        &conn,
        turn_ingress_sql().queued_items_sqlite.list_by_batches.sql(),
    );
    assert!(
        plan.contains("USING INDEX") || plan.contains("USING PRIMARY KEY"),
        "the multi-batch item read no longer seeks:\n{plan}"
    );
}

/// The predicates the renderer produces, against the one source that generates
/// them.
mod byte_identity {
    use super::{catalog, turn_ingress_sql};
    use lash_core_execution::store_backend_support as vocabulary;

    #[test]
    fn a_state_token_renders_to_the_predicate_its_generator_spells() {
        // A `{{term(column)}}` token is only worth having if it renders to
        // exactly what the generator produces: the enum stays the one source of
        // the vocabulary, and `idx_pending_turn_inputs_session` is only helped
        // by a predicate the planner can match against the column.
        let sql = turn_ingress_sql();
        assert!(
            sql.pending_inputs.delete_terminal.sql().contains(
                &vocabulary::terminal_turn_input_state_predicate_sql("state")
            ),
            "the retention delete no longer spells the generated terminal set",
        );
        assert!(
            sql.pending_inputs.settle_unclaimed.sql().contains(
                &vocabulary::nonterminal_turn_input_state_predicate_sql("state")
            ),
            "the unclaimed settlement no longer spells the generated open set",
        );
        assert!(
            sql.pending_inputs.list_undelivered.sql().contains(
                &vocabulary::undelivered_turn_input_state_predicate_sql("state")
            ),
            "the undelivered list no longer spells the generated undelivered set",
        );
        assert!(
            sql.family.has_claimable_work.sql().contains(
                &vocabulary::deferred_next_turn_turn_input_state_predicate_sql("pti.state")
            ),
            "the claimable-work probe no longer spells the generated deferred state",
        );
    }

    #[test]
    fn a_checkpoint_statement_spells_the_boundary_its_generator_spells() {
        // The minimum-boundary predicate cannot be a vocabulary token: its
        // column is a dialect-specific JSON extraction, not an identifier. So
        // the statements spell it, and this is what holds the spelling to
        // `admitted_min_boundary_sql` — the one place the boundary enum reaches
        // SQL.
        let sql = turn_ingress_sql();
        let expression = "json_extract(ingress_json, '$.min_boundary')";
        for (statement, checkpoint) in [
            (
                &sql.pending_inputs_sqlite
                    .claim_candidates_active_turn_after_work,
                lash_core_execution::CheckpointKind::AfterWork,
            ),
            (
                &sql.pending_inputs_sqlite
                    .claim_candidates_active_turn_before_completion,
                lash_core_execution::CheckpointKind::BeforeCompletion,
            ),
            (
                &sql.family_sqlite.checkpoint_work_pending_after_work,
                lash_core_execution::CheckpointKind::AfterWork,
            ),
            (
                &sql.family_sqlite.checkpoint_work_pending_before_completion,
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
    fn the_schema_still_declares_the_check_the_release_statements_depend_on() {
        // Every release path clears the whole four-column claim identity
        // because this CHECK refuses a row that carries part of it. If the
        // constraint were ever dropped, the spelling would stop being
        // load-bearing and this test would say so.
        let conn = catalog();
        let declared: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                ["pending_turn_inputs"],
                |row| row.get(0),
            )
            .expect("the table is declared");
        assert!(
            declared.contains("ck_pending_turn_inputs_claim_identity_all_or_none"),
            "the all-or-none claim identity CHECK is gone:\n{declared}"
        );
    }
}
