//! The schema's named-`CHECK` and fragment laws, split from `schema.rs` to
//! keep the production file inside its line budget.

use super::*;

fn assert_check_rejects(connection: &Connection, statement: &str, constraint: &str) {
    let error = connection
        .execute_batch(statement)
        .expect_err("an illegal durable vocabulary must violate its schema CHECK");
    assert!(
        error.to_string().contains(constraint),
        "SQLite reported the wrong CHECK for {constraint}: {error}"
    );
}

#[test]
fn sqlite_checks_reject_every_registered_illegal_vocabulary_cluster() {
    let core = Connection::open_in_memory().expect("open durable-core constraint fixture");
    core.execute_batch(SCHEMA)
        .expect("create durable-core constraint fixture");
    // The three illegal scope/state pairs must name the correlation CHECK.
    // An ingress_json without a scope key passes both CHECKs under SQL NULL
    // semantics; serde cannot emit it, so both backends behave identically.
    assert_check_rejects(
        &core,
        "INSERT INTO pending_turn_inputs (enqueue_seq,
             input_id, session_id, ingress_json, state, input_json,
             submission_digest, enqueued_at_ms
         ) VALUES (1,
             'bad-turn-input-state', 'session',
             '{\"scope\":\"active_turn\",\"turn_id\":\"turn\"}',
             'waiting', '{}', 'digest', 0
         )",
        "ck_pending_turn_inputs_state",
    );
    assert_check_rejects(
        &core,
        "INSERT INTO pending_turn_inputs (enqueue_seq,
             input_id, session_id, ingress_json, state, input_json,
             submission_digest, enqueued_at_ms
         ) VALUES (1,
             'bad-turn-input-pair', 'session', '{\"scope\":\"next_turn\"}',
             'pending_active', '{}', 'digest', 0
         )",
        "ck_pending_turn_inputs_state_ingress",
    );
    assert_check_rejects(
        &core,
        "INSERT INTO pending_turn_inputs (enqueue_seq,
             input_id, session_id, ingress_json, state, input_json,
             submission_digest, enqueued_at_ms
         ) VALUES (1,
             'bad-turn-input-accepted-pair', 'session', '{\"scope\":\"next_turn\"}',
             'accepted', '{}', 'digest', 0
         )",
        "ck_pending_turn_inputs_state_ingress",
    );
    assert_check_rejects(
        &core,
        "INSERT INTO pending_turn_inputs (enqueue_seq,
             input_id, session_id, ingress_json, state, input_json,
             submission_digest, enqueued_at_ms
         ) VALUES (1,
             'bad-turn-input-deferred-pair', 'session',
             '{\"scope\":\"active_turn\",\"turn_id\":\"turn\"}',
             'deferred_next_turn', '{}', 'digest', 0
         )",
        "ck_pending_turn_inputs_state_ingress",
    );
    assert_check_rejects(
        &core,
        "INSERT INTO session_meta (session_id, relation_kind, retention_kind) VALUES ('bad-relation', 'sibling', 'until_gc')",
        "ck_session_meta_relation_kind",
    );
    assert_check_rejects(
        &core,
        "INSERT INTO session_meta (session_id, relation_kind, parent_session_id, caused_by_kind, retention_kind) VALUES ('bad-cause', 'child', 'parent', 'timer', 'until_gc')",
        "ck_session_meta_caused_by_kind",
    );
    core.execute_batch(
        "INSERT INTO session_meta (session_id, relation_kind, parent_session_id, caused_by_kind, caused_by_effect_id, retention_kind) VALUES ('effect-address-cause', 'child', 'parent', 'effect_address', '{}', 'until_gc')",
    )
    .expect("current effect-address discriminator is admitted");
    assert_check_rejects(
        &core,
        "INSERT INTO session_meta (session_id, relation_kind, parent_session_id, caused_by_kind, retention_kind) VALUES ('legacy-effect-cause', 'child', 'parent', 'effect', 'until_gc')",
        "ck_session_meta_caused_by_kind",
    );

    assert_check_rejects(
        &core,
        "INSERT INTO session_meta (session_id, relation_kind, retention_kind) VALUES ('childless-child', 'child', 'until_gc')",
        "ck_session_meta_relation_family",
    );
    assert_check_rejects(
        &core,
        "INSERT INTO session_meta (session_id, relation_kind, caused_by_kind, caused_by_session_id, caused_by_turn_id, retention_kind) VALUES ('caused-run', 'root', 'turn', 'cause-session', 'cause-turn', 'until_gc')",
        "ck_session_meta_relation_family",
    );
    assert_check_rejects(
        &core,
        "INSERT INTO session_meta (session_id, relation_kind, parent_session_id, caused_by_kind, retention_kind) VALUES ('bare-discriminator', 'child', 'parent', 'turn', 'until_gc')",
        "ck_session_meta_caused_by_family",
    );
    assert_check_rejects(
        &core,
        "INSERT INTO session_meta (session_id, relation_kind, parent_session_id, caused_by_kind, caused_by_session_id, caused_by_turn_id, caused_by_node_id, retention_kind) VALUES ('crossed-family', 'child', 'parent', 'turn', 'cause-session', 'cause-turn', 'stray-node', 'until_gc')",
        "ck_session_meta_caused_by_family",
    );
    assert_check_rejects(
        &core,
        "INSERT INTO session_meta (session_id, relation_kind, parent_session_id, caused_by_session_id, retention_kind) VALUES ('kindless-payload', 'child', 'parent', 'cause-session', 'until_gc')",
        "ck_session_meta_caused_by_family",
    );

    let process = Connection::open_in_memory().expect("open process constraint fixture");
    process
        .execute_batch(PROCESS_SCHEMA)
        .expect("create process constraint fixture");
    // A process's status is its record's alone:
    // `a_process_row_takes_its_lifecycle_columns_from_its_record_alone`.
    let process_columns = PROCESS_FIXTURE_COLUMNS;
    assert_check_rejects(
        &process,
        &format!(
            "INSERT INTO processes ({process_columns}) VALUES
             ('bad-lifetime', 'originator', 'standard', 0, 0, 0,
              NULL, NULL, 'abandon', '{{\"last_event_sequence\":0,\"lifecycle\":{{\"state\":\"running\"}}}}')"
        ),
        "ck_processes_lifetime",
    );
    assert_check_rejects(
        &process,
        &format!(
            "INSERT INTO processes ({process_columns}) VALUES
             ('bad-scope-kind', 'originator', 'standard', 0, 0, 0,
              'host', 'scope', 'until', '{{\"last_event_sequence\":0,\"lifecycle\":{{\"state\":\"running\"}}}}')"
        ),
        "ck_processes_lifetime_scope",
    );
    assert_check_rejects(
        &process,
        &format!(
            "INSERT INTO processes ({process_columns}) VALUES
             ('detached-with-scope', 'originator', 'standard', 0, 0, 0,
              'turn', 'scope', 'detached', '{{\"last_event_sequence\":0,\"lifecycle\":{{\"state\":\"running\"}}}}')"
        ),
        "ck_processes_lifetime_scope",
    );
    assert_check_rejects(
        &process,
        &format!(
            "INSERT INTO processes ({process_columns}) VALUES
             ('until-without-id', 'originator', 'standard', 0, 0, 0,
              'session', NULL, 'until', '{{\"last_event_sequence\":0,\"lifecycle\":{{\"state\":\"running\"}}}}')"
        ),
        "ck_processes_lifetime_scope",
    );
    assert_check_rejects(
        &process,
        "INSERT INTO parent_end_plans (parent_kind, parent_id, parent_payload, ended_at_ms)
         VALUES ('host', 'scope', '{}', 0)",
        "ck_parent_end_plans_kind",
    );
    process
        .execute_batch(&format!(
            "INSERT INTO processes ({process_columns}) VALUES
             ('wake-parent', 'originator', 'standard', 0, 0, 0,
              NULL, NULL, 'detached', '{{\"last_event_sequence\":0,\"lifecycle\":{{\"state\":\"running\"}}}}')"
        ))
        .expect("insert valid wake parent");
    assert_check_rejects(
        &process,
        "INSERT INTO process_event_horizons (process_id, released_through)
         VALUES ('wake-parent', 0)",
        "ck_process_event_horizons_positive",
    );
    // A tombstone names the retired status its process was pruned in.
    for label in ["running", "waiting", "finished"] {
        assert_check_rejects(
            &process,
            &format!(
                "INSERT INTO process_tombstones (
                     process_id, terminal_label, pruned_at_ms, pruned_change_seq
                 ) VALUES ('tombstone-{label}', '{label}', 0, 1)"
            ),
            "ck_process_tombstones_terminal_label",
        );
    }
}

#[test]
fn turn_cancellation_shape_is_guarded() {
    let conn = Connection::open_in_memory().expect("open cancellation fixture");
    conn.execute_batch(SCHEMA).expect("create schema");
    conn.execute_batch("INSERT INTO turn_cancel_requests (session_id, turn_id, request_id, disposition, mode, intent_revision) VALUES ('session', 'turn', 'request', 'defer', 'immediate', 1)").expect("record relational request");
    for (column, value, constraint) in [
        (
            "disposition",
            "discard",
            "ck_turn_cancel_requests_disposition",
        ),
        ("mode", "later", "ck_turn_cancel_requests_mode"),
        (
            "intent_revision",
            "0",
            "ck_turn_cancel_requests_intent_revision",
        ),
    ] {
        assert_check_rejects(
            &conn,
            &format!("UPDATE turn_cancel_requests SET {column} = '{value}'"),
            constraint,
        );
    }
}

/// The columns a statement may write on a process row.
const PROCESS_FIXTURE_COLUMNS: &str = "process_id, originator_id,
        identity_kind, created_at_ms, updated_at_ms, change_seq,
        lifetime_scope_kind, lifetime_scope_id, lifetime, record_json";

/// A process record's JSON, as far as its row's columns read it: the event
/// sequence, the lifecycle and the cancel request, each in its own codec.
fn process_record_json(
    status: lash_core_execution::ProcessStatus,
    last_event_sequence: u64,
    cancel_requested_at_ms: Option<u64>,
) -> String {
    let mut record = serde_json::json!({
        "last_event_sequence": last_event_sequence,
        "lifecycle": lash_core_execution::ProcessLifecycleState::fixture(status),
    });
    if let Some(at) = cancel_requested_at_ms {
        record["cancel_request"] = serde_json::json!(lash_core_execution::CancelRequest::new(
            lash_core_execution::CancelOrigin::OperatorRequested,
            "operator",
            at,
        ));
    }
    record.to_string()
}

/// F1 (FIG-5557): `status`, `last_event_sequence` and
/// `cancel_requested_at_ms` are the database's projections of `record_json`.
/// No statement can write one, so a row whose columns disagree with its
/// record cannot be written; a write of the record moves all three; and a
/// record whose lifecycle names no status, or that lacks its sequence or
/// its cancel's time, is refused. A record whose strings hold a NUL is
/// still written.
#[test]
fn a_process_row_takes_its_lifecycle_columns_from_its_record_alone() {
    use lash_core_execution::ProcessStatus;

    let process = Connection::open_in_memory().expect("open process projection fixture");
    process
        .execute_batch(PROCESS_SCHEMA)
        .expect("create process projection fixture");
    let insert = |id: &str, record: &str| {
        process.execute(
            &format!(
                "INSERT INTO processes ({PROCESS_FIXTURE_COLUMNS}) VALUES
                 (?1, 'originator', 'standard', 0, 0, 0, NULL, NULL, 'detached', ?2)"
            ),
            rusqlite::params![id, record],
        )
    };
    let columns = |id: &str| {
        process
            .query_row(
                "SELECT status, last_event_sequence, cancel_requested_at_ms
                   FROM processes WHERE process_id = ?1",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .expect("read the projected columns")
    };

    // Every status the record's lifecycle can be is the row's status.
    let statuses = [
        ProcessStatus::Running,
        ProcessStatus::Waiting,
        ProcessStatus::Completed,
        ProcessStatus::Failed,
        ProcessStatus::Cancelled,
        ProcessStatus::Abandoned,
    ];
    for (sequence, status) in (0_u64..).zip(statuses) {
        let id = format!("is-{}", status.label());
        insert(&id, &process_record_json(status, sequence, None)).expect("insert the record");
        assert_eq!(
            columns(&id),
            (status.label().to_owned(), sequence as i64, None),
            "{id}"
        );
    }

    // A write of the record alone moves every projection with it.
    insert(
        "moves",
        &process_record_json(ProcessStatus::Running, 1, None),
    )
    .expect("insert a running record");
    process
        .execute(
            "UPDATE processes SET record_json = ?1 WHERE process_id = 'moves'",
            [process_record_json(ProcessStatus::Cancelled, 7, Some(42))],
        )
        .expect("save the cancelled record");
    assert_eq!(columns("moves"), ("cancelled".to_owned(), 7, Some(42)));

    // No statement writes a projection, with the record or without it.
    for column in ["status", "last_event_sequence", "cancel_requested_at_ms"] {
        let value = if column == "status" {
            "'completed'"
        } else {
            "9"
        };
        for statement in [
            format!(
                "INSERT INTO processes ({PROCESS_FIXTURE_COLUMNS}, {column}) VALUES
                 ('disagrees', 'originator', 'standard', 0, 0, 0, NULL, NULL, 'detached',
                  '{}', {value})",
                process_record_json(ProcessStatus::Running, 0, None)
            ),
            format!("UPDATE processes SET {column} = {value} WHERE process_id = 'moves'"),
        ] {
            let error = process
                .execute_batch(&statement)
                .expect_err("a projection is not a statement's to write");
            assert!(
                error.to_string().contains("generated column"),
                "{column}: {error}"
            );
        }
    }
    assert_eq!(columns("moves"), ("cancelled".to_owned(), 7, Some(42)));

    // A record that names no status, or lacks what a column reads, is
    // refused whole.
    let running = process_record_json(ProcessStatus::Running, 0, None);
    for (what, record) in [
        ("no lifecycle", "{\"last_event_sequence\":0}".to_owned()),
        (
            "an unknown state",
            running.replace("\"running\"", "\"paused\""),
        ),
        (
            "an unknown outcome",
            process_record_json(ProcessStatus::Abandoned, 0, None)
                .replace("\"abandoned\"", "\"vanished\""),
        ),
        (
            "an unknown settlement",
            process_record_json(ProcessStatus::Completed, 0, None)
                .replace("\"success\"", "\"shrugged\""),
        ),
        (
            "no event sequence",
            running.replace("\"last_event_sequence\"", "\"sequence\""),
        ),
        (
            "a cancel without its time",
            process_record_json(ProcessStatus::Running, 0, Some(42))
                .replace("\"requested_at_ms\"", "\"at\""),
        ),
        ("text that is not a record", "not json".to_owned()),
    ] {
        assert!(
            insert("refused", &record).is_err(),
            "a record with {what} was written: {record}"
        );
        assert!(
            process
                .execute(
                    "UPDATE processes SET record_json = ?1 WHERE process_id = 'moves'",
                    [&record],
                )
                .is_err(),
            "a record with {what} was saved: {record}"
        );
    }
    assert_eq!(columns("moves"), ("cancelled".to_owned(), 7, Some(42)));

    // A record whose strings hold a NUL character is written and projected
    // like any other.
    let mut with_nul: serde_json::Value =
        serde_json::from_str(&running).expect("decode the fixture record");
    with_nul["input"] = serde_json::json!("a\u{0}b\\u0000");
    insert("holds-a-nul", &with_nul.to_string()).expect("a record holding a NUL is written");
    assert_eq!(columns("holds-a-nul"), ("running".to_owned(), 0, None));
}
