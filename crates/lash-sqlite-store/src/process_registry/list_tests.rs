use super::*;

use sql::roster_query;

#[test]
fn retired_roster_page_uses_live_and_recency_indexes() {
    let conn = rusqlite::Connection::open_in_memory().expect("open query-plan database");
    conn.execute_batch(crate::schema::PROCESS_SCHEMA)
        .expect("install process schema");
    let (sql, values) = roster_query(
        &lash_execution_contracts::process::ProcessListFilter {
            status: lash_execution_contracts::process::ProcessStatusFilter::Any,
            retired_since_ms: Some(100),
            ..Default::default()
        },
        None,
        None,
        Some("p_"),
        "~",
        257,
    );
    let mut stmt = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("prepare recently retired page query plan");
    let plan = stmt
        .query_map(rusqlite::params_from_iter(values.iter()), |row| {
            row.get::<_, String>(3)
        })
        .expect("explain recently retired query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect recently retired query plan");

    assert!(
        plan.iter().any(|step| {
            step.contains("idx_processes_status") || step.contains("idx_processes_non_terminal")
        }),
        "live branch must use a live/status index, plan: {plan:?}"
    );
    assert!(
        plan.iter()
            .any(|step| step.contains("idx_processes_recent_retired")),
        "retired branch must seek the recency index, plan: {plan:?}"
    );
    assert!(
        plan.iter()
            .all(|step| !step.contains("sqlite_autoindex_processes_1")),
        "bounded poll must not scan the all-history primary-key index, plan: {plan:?}"
    );
}

#[test]
fn observed_recently_retired_query_seeks_recency_before_observer_history() {
    let conn = rusqlite::Connection::open_in_memory().expect("open query-plan database");
    conn.execute_batch(crate::schema::PROCESS_SCHEMA)
        .expect("install process schema");
    let mut stmt = conn
        .prepare(&format!(
            "EXPLAIN QUERY PLAN {}",
            process_sql()
                .registry_sqlite
                .list_observed_recent_retired
                .sql()
        ))
        .expect("prepare observed query plan");
    let plan = stmt
        .query_map(params!["session", Option::<String>::None, 100_i64], |row| {
            row.get::<_, String>(3)
        })
        .expect("explain observed query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect plan");
    assert!(
        plan.iter()
            .any(|step| step.contains("idx_processes_recent_retired")),
        "retired branch must seek recency: {plan:?}"
    );
    assert!(
        plan.iter().any(|step| step.contains("idx_processes_status")
            || step.contains("idx_processes_non_terminal")),
        "live branch must use a live index: {plan:?}"
    );
    assert!(
        !plan.iter().any(|step| step.contains("SCAN o")),
        "observer history must use keyed probes: {plan:?}"
    );
}

#[test]
fn pending_cancel_roster_page_seeks_the_partial_cancel_index() {
    let conn = rusqlite::Connection::open_in_memory().expect("open query-plan database");
    conn.execute_batch(crate::schema::PROCESS_SCHEMA)
        .expect("install process schema");
    let (sql, values) = roster_query(
        &lash_execution_contracts::process::ProcessListFilter {
            status: lash_execution_contracts::process::ProcessStatusFilter::Any,
            cancel_pending_before_ms: Some(1_700_000_000_000),
            ..lash_execution_contracts::process::ProcessListFilter::default()
        },
        None,
        None,
        Some("p_"),
        "~",
        257,
    );
    let mut stmt = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("prepare pending-cancel query plan");
    let plan = stmt
        .query_map(rusqlite::params_from_iter(values.iter()), |row| {
            row.get::<_, String>(3)
        })
        .expect("explain pending-cancel query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect pending-cancel query plan");
    assert!(
        plan.iter()
            .any(|step| step.contains("idx_processes_pending_cancel")),
        "a populated pending-cancel bound must seek the partial index, plan: {plan:?}"
    );
}

#[test]
fn until_scope_roster_page_seeks_the_lifetime_scope_index() {
    let conn = rusqlite::Connection::open_in_memory().expect("open query-plan database");
    conn.execute_batch(crate::schema::PROCESS_SCHEMA)
        .expect("install process schema");
    let (sql, values) = roster_query(
        &lash_execution_contracts::process::ProcessListFilter {
            status: lash_execution_contracts::process::ProcessStatusFilter::Any,
            until: Some(lash_core_store::scope_identity::ScopeId::turn(
                lash_vocabulary::identity::SessionId::from("plan-session"),
                lash_vocabulary::identity::TurnId::from("plan-turn"),
            )),
            ..lash_execution_contracts::process::ProcessListFilter::default()
        },
        None,
        None,
        Some("p_"),
        "~",
        257,
    );
    let mut stmt = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("prepare until-scope query plan");
    let plan = stmt
        .query_map(rusqlite::params_from_iter(values.iter()), |row| {
            row.get::<_, String>(3)
        })
        .expect("explain until-scope query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect until-scope query plan");
    assert!(
        plan.iter()
            .any(|step| step.contains("idx_processes_lifetime_scope")),
        "a populated until scope must seek the scope index, plan: {plan:?}"
    );
    assert!(
        plan.iter().all(|step| !step.contains("SCAN processes")),
        "the scope lookup must not degrade to a table scan, plan: {plan:?}"
    );
}

#[test]
fn retention_keeps_terminal_process_until_cascade_drains() {
    let conn = rusqlite::Connection::open_in_memory().expect("open retention fixture");
    conn.execute_batch(crate::schema::PROCESS_SCHEMA)
        .expect("create schema");
    conn.execute_batch("INSERT INTO processes (process_id, originator_id, identity_kind, created_at_ms, updated_at_ms, change_seq, lifetime, start_json, execution_json, lifecycle_json, cascade_cursor) VALUES ('parent', 'origin', 'standard', 0, 0, 1, 'detached', '{}', '{}', '{\"last_event_sequence\":0,\"lifecycle\":{\"state\":\"terminal\",\"occurred_at_ms\":0,\"outcome\":{\"type\":\"settled\",\"output\":{\"outcome\":{\"status\":\"success\"}}}}}', 'child')").expect("terminal with unfinished cascade");
    let sql = process_sql().process_sqlite.list_prunable_terminal.sql();
    let count = |conn: &rusqlite::Connection| {
        conn.prepare(sql)
            .expect("prepare candidates")
            .query_map(params![1, Option::<i64>::None], |_| Ok(()))
            .expect("candidates")
            .count()
    };
    assert_eq!(
        count(&conn),
        0,
        "an unfinished cancellation cascade must retain its parent"
    );
    conn.execute_batch("UPDATE processes SET cascade_cursor = NULL")
        .expect("drain cascade");
    assert_eq!(
        count(&conn),
        1,
        "a drained terminal is eligible for retention"
    );
}

/// PROCESS-LIFECYCLE-WRITE-SIZE: input size cannot affect lifecycle write
/// bytes; a batch saves once and replay saves nothing, while reads stay rich.
#[test]
fn process_lifecycle_write_size() {
    use lash_core_store::store::FleetFormat;
    use lash_execution_contracts::process::EngineLaunch;
    use lash_execution_contracts::process::Lifetime;
    use lash_execution_contracts::process::ProcessInput;
    use lash_execution_contracts::process::ProcessLifecycleState;
    use lash_execution_contracts::process::ProcessProvenance;
    use lash_execution_contracts::process::ProcessRegistration;
    use lash_execution_contracts::process::ProcessWaits;
    use lash_execution_contracts::process::WaitKind;
    use lash_execution_contracts::process::WaitState;
    use lash_protocol_types::tool_output::CancelOrigin;
    use lash_protocol_types::tool_output::CancelRequest;
    use std::cell::RefCell;

    thread_local! {
        static WRITES: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
    }
    fn capture(event: rusqlite::trace::TraceEvent<'_>) {
        if let rusqlite::trace::TraceEvent::Stmt(statement, _) = event {
            let Some(sql) = statement.expanded_sql() else {
                return;
            };
            if sql.starts_with("UPDATE processes\n") {
                WRITES.with(|writes| writes.borrow_mut().push(sql.len()));
            }
        }
    }
    fn exercise(payload: String) -> Vec<usize> {
        let conn = Connection::open_in_memory().expect("open lifecycle law");
        conn.execute_batch(crate::schema::PROCESS_SCHEMA)
            .expect("schema");
        let registration = ProcessRegistration::new(
            ProcessInput::Engine {
                kind: "write-size".into(),
                payload: serde_json::json!({"body": payload, "escapes": "\0'雪"}),
                launch: EngineLaunch::new(
                    lash_execution_contracts::testing::process_execution_env_fixture_ref(),
                    None,
                ),
            },
            ProcessProvenance::host(),
            Lifetime::Detached,
        );
        let mut record = ProcessRecord::from_prepared_registration(
            prepare_process_registration(registration).expect("prepare"),
            ProcessId::fixture("write-size"),
            10,
        );
        conn.execute(
            process_sql().process_sqlite.insert_registration.sql(),
            params![
                record.id.as_str(),
                Option::<String>::None,
                record.originator_id(),
                record.identity.kind.as_str(),
                record.identity.label,
                10_i64,
                10_i64,
                1_i64,
                Option::<String>::None,
                Option::<String>::None,
                "detached",
                process_encode_json(&record.lifecycle_storage()).expect("record"),
                Option::<String>::None,
                Option::<String>::None,
                Option::<String>::None,
                Option::<bool>::None,
                process_encode_json(&record.start_storage()).expect("start"),
                process_encode_json(&record.execution_storage()).expect("execution")
            ],
        )
        .expect("insert start");
        let wait = WaitState {
            kind: WaitKind::Call {
                resume_at_ms: None,
                call_id: lash_protocol_types::ToolCallId::fixture("write-size"),
                tool_id: lash_vocabulary::tool_contract::ToolId::from("write-size"),
            },
            since_ms: 20,
            site: None,
        };
        let enter = ProcessEventAppendRequest::wait_entered(&record.id, &wait);
        let clear = ProcessEventAppendRequest::wait_cleared(&record.id, &wait);
        let next_wait = WaitState {
            since_ms: 30,
            ..wait.clone()
        };
        let cancel = CancelRequest::new(CancelOrigin::OperatorRequested, "write-size", 40);
        let batches = [
            vec![enter, clear],
            vec![ProcessEventAppendRequest::wait_entered(
                &record.id, &next_wait,
            )],
            vec![ProcessEventAppendRequest::wait_cleared(
                &record.id, &next_wait,
            )],
            vec![ProcessEventAppendRequest::cancel_requested(
                &record.id, &cancel,
            )],
        ];
        // Creation facts never move, even when a successor changes the
        // execution pair. The successor and its two wait facts still save once.
        conn.execute_batch(
            "CREATE TRIGGER immutable_start BEFORE UPDATE OF start_json ON processes
            BEGIN SELECT RAISE(ABORT, 'creation metadata is immutable'); END;",
        )
        .expect("guard creation metadata");
        let successor = lash_execution_contracts::process::ProcessSuccessorRequest {
            payload: serde_json::json!({"successor": record.input}),
            env: lash_execution_contracts::testing::process_execution_env_fixture_ref(),
            definition_id: Some(
                lash_protocol_types::ProcessDefinitionId::from_sha256_digest([3; 32]),
            ),
        };
        let setup_wait = WaitState {
            since_ms: 11,
            ..wait.clone()
        };
        let prelude = [
            ProcessEventAppendRequest::succeeded(&record.id, &successor),
            ProcessEventAppendRequest::wait_entered(&record.id, &setup_wait),
            ProcessEventAppendRequest::wait_cleared(&record.id, &setup_wait),
        ];
        WRITES.with(|writes| writes.borrow_mut().clear());
        conn.trace_v2(
            rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
            Some(capture),
        );
        conn.execute_batch("BEGIN IMMEDIATE")
            .expect("begin successor");
        let mut batch = ProcessEventBatch::for_fleet(FleetFormat::current());
        for request in &prelude {
            batch
                .stage(&conn, &mut record, request.clone(), 11)
                .expect("successor prelude");
        }
        batch.commit(&conn, &record).expect("save successor batch");
        conn.execute_batch("COMMIT").expect("publish successor");
        assert_eq!(
            WRITES.with(|writes| writes.borrow().len()),
            1,
            "one successor batch save"
        );
        assert_eq!(record.identity.definition_id, successor.definition_id);
        assert_eq!(
            SqliteProcessRegistry::load_process_conn(&conn, &record.id)
                .expect("read successor")
                .expect("retained"),
            record
        );
        let mut replay = ProcessEventBatch::for_fleet(FleetFormat::current());
        for request in prelude {
            replay
                .stage(&conn, &mut record, request, 12)
                .expect("replay successor");
        }
        replay
            .commit(&conn, &record)
            .expect("successor replay commit");
        assert_eq!(
            WRITES.with(|writes| writes.borrow().len()),
            1,
            "successor replay saves nothing"
        );
        WRITES.with(|writes| writes.borrow_mut().clear());
        conn.trace_v2(
            rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
            Some(capture),
        );
        for (index, requests) in batches.into_iter().enumerate() {
            let now = 20 + index as u64 * 10;
            let before = WRITES.with(|writes| writes.borrow().len());
            conn.execute_batch("BEGIN IMMEDIATE").expect("begin");
            let mut batch = ProcessEventBatch::for_fleet(FleetFormat::current());
            for request in &requests {
                batch
                    .stage(&conn, &mut record, request.clone(), now)
                    .expect("stage");
            }
            batch.commit(&conn, &record).expect("commit projection");
            conn.execute_batch("COMMIT").expect("publish");
            assert_eq!(
                WRITES.with(|writes| writes.borrow().len()),
                before + 1,
                "one lifecycle write per changed batch"
            );
            assert_eq!(record.last_event_sequence, index as u64 + 5);
            assert_eq!(
                record.lifecycle,
                if index == 1 {
                    ProcessLifecycleState::Waiting {
                        waits: ProcessWaits::new(next_wait.clone()),
                    }
                } else {
                    ProcessLifecycleState::running()
                }
            );
            assert_eq!(
                record.cancel_request.as_deref(),
                (index == 3).then_some(&cancel)
            );
            assert_eq!(
                SqliteProcessRegistry::load_process_conn(&conn, &record.id)
                    .expect("read")
                    .expect("retained"),
                record,
                "complete read"
            );
            let payload: String = conn
                .query_row(
                    process_sql().process_sqlite.list_changes_after.sql(),
                    params![0_i64, 100_i64],
                    |row| row.get(2),
                )
                .expect("feed");
            assert_eq!(
                serde_json::from_str::<ProcessRecord>(&payload).expect("rich feed"),
                record
            );
            let sequence: i64 = conn
                .query_row(process_sql().clock_sqlite.select_current.sql(), [], |row| {
                    row.get(0)
                })
                .expect("clock");
            let mut replay = ProcessEventBatch::for_fleet(FleetFormat::current());
            for request in requests {
                replay
                    .stage(&conn, &mut record, request, now + 100)
                    .expect("replay");
            }
            replay.commit(&conn, &record).expect("replay commit");
            assert_eq!(
                WRITES.with(|writes| writes.borrow().len()),
                before + 1,
                "replay writes no projection"
            );
            let after: i64 = conn
                .query_row(process_sql().clock_sqlite.select_current.sql(), [], |row| {
                    row.get(0)
                })
                .expect("clock");
            assert_eq!(after, sequence, "replay consumes no change sequence");
        }
        conn.trace_v2(rusqlite::trace::TraceEventCodes::empty(), None);
        WRITES.with(|writes| writes.borrow().clone())
    }
    let small = exercise("x".into());
    let large = exercise("x".repeat(128 * 1024));
    eprintln!("lifecycle UPDATE bytes: small={small:?}, large={large:?}");
    assert_eq!(
        small, large,
        "immutable input must not be resubmitted by lifecycle UPDATEs"
    );
}
