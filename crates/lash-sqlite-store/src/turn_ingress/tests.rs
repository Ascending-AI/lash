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

use super::{tool_intent_sql, turn_ingress_sql};

/// A catalog with this crate's real durable-core schema, in memory: the
/// session schema and the fragments the durable core database carries beside
/// it (the retention delete reads the root family's input bindings).
fn catalog() -> Connection {
    let conn = Connection::open_in_memory().expect("in-memory database opens");
    conn.execute_batch(crate::schema::SCHEMA)
        .expect("session schema applies");
    for fragment in [
        crate::schema_fragments::SESSION_INGRESS_TABLE,
        crate::schema_fragments::SESSION_ROOTS_TABLES,
    ] {
        conn.execute_batch(fragment)
            .expect("durable-core fragment applies");
    }
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
        sql.pending_inputs.earliest_next_turn_candidate_seq.sql(),
        sql.pending_inputs.select_admitted_by_step.sql(),
        sql.pending_inputs.admit.sql(),
        sql.pending_inputs.settle_admitted.sql(),
        sql.pending_inputs.release_admitted.sql(),
        sql.pending_inputs.release_root.sql(),
        sql.pending_inputs.delete_withdrawn.sql(),
        sql.pending_inputs.delete_by_session.sql(),
        sql.pending_inputs.insert_new.sql(),
        sql.pending_inputs.select_id_by_source_key.sql(),
        sql.pending_inputs.select_session_by_input_id.sql(),
        sql.pending_inputs_sqlite.settlement_facts.sql(),
        sql.pending_inputs_sqlite.select_suffix.sql(),
        sql.pending_inputs_sqlite.select_pending_active.sql(),
        sql.pending_inputs_sqlite
            .admission_candidates_next_turn
            .sql(),
        sql.pending_inputs_sqlite
            .admission_candidates_active_turn_after_work
            .sql(),
        sql.pending_inputs_sqlite
            .admission_candidates_active_turn_before_completion
            .sql(),
        sql.queued_batches.select_by_id.sql(),
        sql.queued_batches.select_id_by_source_key.sql(),
        sql.queued_batches.list_by_session.sql(),
        sql.queued_batches.list_open.sql(),
        sql.queued_batches.select_admitted_by_step.sql(),
        sql.queued_batches.admit.sql(),
        sql.queued_batches.deliver_open_command.sql(),
        sql.queued_batches.select_admitted_batch_head_payload.sql(),
        sql.queued_batches.settle_admitted.sql(),
        sql.queued_batches.settle_command.sql(),
        sql.queued_batches.release_admitted.sql(),
        sql.queued_batches.release_root.sql(),
        sql.queued_batches.delete_by_session.sql(),
        sql.queued_batches_sqlite.insert_new.sql(),
        sql.queued_batches_sqlite.settlement_facts.sql(),
        sql.queued_batches_sqlite.select_cancelable.sql(),
        sql.queued_batches_sqlite.delete_cancelled.sql(),
        sql.queued_batches_sqlite.admission_candidates_idle.sql(),
        sql.queued_batches_sqlite
            .admission_candidates_turn_lane
            .sql(),
        sql.queued_batches_sqlite
            .admission_candidates_boundary
            .sql(),
        sql.queued_items.insert_new.sql(),
        sql.queued_items.list_by_batch.sql(),
        sql.queued_items_sqlite.list_by_batches.sql(),
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
fn an_admission_candidate_scan_seeks_the_open_row_index() {
    // Each named shape replaced one `format!` that spliced the mode's filter
    // in per call, and each has to seek the partial index over open rows
    // (FIG-3927): the settled rows a session keeps forever are outside it, so
    // an admission's cost does not grow with the session's history.
    let conn = catalog();
    let statements = &turn_ingress_sql().pending_inputs_sqlite;
    for statement in [
        &statements.select_pending_active,
        &statements.admission_candidates_next_turn,
        &statements.admission_candidates_active_turn_after_work,
        &statements.admission_candidates_active_turn_before_completion,
    ] {
        assert_uses(&conn, statement, "idx_pending_turn_inputs_open_state");
    }
}

#[test]
fn every_open_input_read_seeks_the_state_index() {
    let conn = catalog();
    let sql = turn_ingress_sql();
    for statement in [
        &sql.pending_inputs.list_undelivered,
        &sql.pending_inputs.earliest_next_turn_candidate_seq,
        &sql.family.has_claimable_work,
        &sql.family.pending_session_work_ordering,
        &sql.family_sqlite.checkpoint_work_pending_after_work,
        &sql.family_sqlite.checkpoint_work_pending_before_completion,
    ] {
        assert_uses(&conn, statement, "idx_pending_turn_inputs_open_state");
    }
}

#[test]
fn every_open_queued_work_read_seeks_the_admission_index() {
    let conn = catalog();
    let sql = turn_ingress_sql();
    for statement in [
        &sql.queued_batches.list_open,
        &sql.queued_batches_sqlite.admission_candidates_idle,
        &sql.queued_batches_sqlite.admission_candidates_turn_lane,
        &sql.queued_batches_sqlite.admission_candidates_boundary,
    ] {
        assert_uses(&conn, statement, "idx_queued_work_admission_order");
    }
}

#[test]
fn reopening_replaces_obsolete_ingress_indexes() {
    let conn = catalog();
    conn.execute_batch(
        "CREATE INDEX idx_queued_work_admitted
             ON queued_work_batches(session_id, admitted_root);
         CREATE INDEX idx_pending_turn_inputs_session
             ON pending_turn_inputs(session_id, state, enqueue_seq);
         CREATE INDEX idx_pending_turn_input_order
             ON pending_turn_inputs(session_id, state, enqueued_at_ms, enqueue_seq);
         CREATE INDEX idx_pending_turn_inputs_open
             ON pending_turn_inputs(session_id, state, enqueue_seq)
             WHERE admitted_root IS NULL
               AND state IN ('pending_active', 'deferred_next_turn');
         CREATE INDEX idx_pending_turn_inputs_admitted
             ON pending_turn_inputs(session_id, admitted_root);",
    )
    .expect("install the previous index set");
    conn.execute_batch(crate::schema::SCHEMA)
        .expect("reopen applies the current index set");
    let mut names = conn
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'index'
               AND tbl_name IN ('queued_work_batches', 'pending_turn_inputs')",
        )
        .expect("inspect ingress indexes");
    let names = names
        .query_map([], |row| row.get::<_, String>(0))
        .expect("list ingress indexes")
        .collect::<Result<Vec<_>, _>>()
        .expect("decode ingress indexes");
    for old in [
        "idx_queued_work_admitted",
        "idx_pending_turn_inputs_session",
        "idx_pending_turn_input_order",
        "idx_pending_turn_inputs_open",
        "idx_pending_turn_inputs_admitted",
    ] {
        assert!(
            !names.iter().any(|name| name == old),
            "old index remains: {old}"
        );
    }
    for current in [
        "idx_queued_work_admission_order",
        "idx_pending_turn_inputs_open_state",
        "idx_pending_turn_inputs_bound_root",
    ] {
        assert!(
            names.iter().any(|name| name == current),
            "current index is missing: {current}"
        );
    }
}

#[test]
fn a_root_release_seeks_the_admission_index() {
    // A root's terminal write releases every row it still holds; the
    // `(session_id, admitted_root)` index is what keeps that one seek per
    // table rather than a scan of the session's history.
    let conn = catalog();
    let sql = turn_ingress_sql();
    assert_uses(
        &conn,
        &sql.pending_inputs.release_root,
        "idx_pending_turn_inputs_bound_root",
    );
    assert_uses(
        &conn,
        &sql.queued_batches.release_root,
        "idx_queued_work_admission_order",
    );
}

#[test]
fn the_multi_batch_item_read_seeks_its_batch() {
    // The admission path hydrates a run of batches in one page; if the `json_each`
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
        // the vocabulary, and `idx_pending_turn_inputs_open_state` is only usable by
        // a predicate that repeats its own terms.
        let sql = turn_ingress_sql();
        assert!(
            sql.pending_inputs.delete_withdrawn.sql().contains(
                &vocabulary::cancelled_turn_input_state_predicate_sql("state")
            ),
            "the retention delete no longer spells the generated cancelled state",
        );
        assert!(
            sql.pending_inputs_sqlite
                .admission_candidates_next_turn
                .sql()
                .contains(&vocabulary::undelivered_turn_input_state_predicate_sql(
                    "state"
                )),
            "the next-turn admission scan no longer spells the open-row index's state set",
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
            "the open-work probe no longer spells the generated deferred state",
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
                    .admission_candidates_active_turn_after_work,
                lash_core_execution::CheckpointKind::AfterWork,
            ),
            (
                &sql.pending_inputs_sqlite
                    .admission_candidates_active_turn_before_completion,
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
    fn the_schema_still_declares_the_checks_the_release_statements_depend_on() {
        // Every release path clears both admission columns because these
        // CHECKs refuse a row that carries one without the other, and a
        // settled row that still names a root. If either constraint were ever
        // dropped, the spelling would stop being load-bearing and this test
        // would say so.
        let conn = catalog();
        for (table, check) in [
            (
                "pending_turn_inputs",
                "ck_pending_turn_inputs_admission_all_or_none",
            ),
            (
                "pending_turn_inputs",
                "ck_pending_turn_inputs_settled_unadmitted",
            ),
            (
                "queued_work_batches",
                "ck_queued_work_batches_admission_all_or_none",
            ),
        ] {
            let declared: String = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .expect("the table is declared");
            assert!(declared.contains(check), "`{check}` is gone:\n{declared}");
        }
    }
}
