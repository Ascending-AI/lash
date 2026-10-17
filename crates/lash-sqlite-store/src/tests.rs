// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use crate::session_listing::list_session_views;
use std::collections::BTreeMap;

use lash_core_store::store::RunStore as _;
use lash_core_store::store::SessionCatalogStore as _;
use lash_core_store::store::SessionHistoryStore as _;
use lash_protocol_types::{ProcessId, SessionId};
use std::sync::atomic::Ordering;

static CHECKPOINT_DATA_STATEMENT_COUNT: AtomicUsize = AtomicUsize::new(0);
static SESSION_LIST_STATEMENT_COUNT: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn input_run_sql_fixture() -> Connection {
    let connection = Connection::open_in_memory().expect("SQLite");
    for ddl in crate::schema::provisioning_statements() {
        connection.execute_batch(ddl).expect("schema");
    }
    connection
        .execute_batch(
            "INSERT INTO session_runs (session_id, run) VALUES ('session', 'run');
         INSERT INTO pending_turn_inputs (enqueue_seq, input_id, session_id, source_key,
             ingress_json, state, input_json, submission_digest, enqueued_at_ms)
         VALUES (1, 'input', 'session', 'alias', '{\"scope\":\"next_turn\"}',
             'deferred_next_turn', '{}', 'digest', 0);",
        )
        .expect("input and run");
    connection
}

/// INPUT-RUN-SURVIVES-SETTLEMENT: the retained binding also resolves an input pin.
#[test]
fn a_settled_inputs_run_and_pinned_revision_survive_without_a_binding_table() {
    let connection = input_run_sql_fixture();
    let session = SessionId::from("session");
    let input = lash_protocol_types::InputId::from("input");
    let run = lash_protocol_types::TurnId::from("run");
    crate::session_runs::bind_run_inputs_conn(
        &connection,
        &session,
        &run,
        std::slice::from_ref(&input),
    )
    .expect("bind");
    connection
        .execute(
            crate::turn_ingress::turn_ingress_sql()
                .pending_inputs
                .settle_admitted
                .sql(),
            params!["session", "input", "completed", "run", 42],
        )
        .expect("settle");
    let status = connection
        .query_row(
            crate::session_runs::session_runs_sql()
                .inputs
                .select_status
                .sql(),
            params![session.as_str(), input.as_str()],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .expect("keyed input status");
    assert_eq!(status, (Some(run.to_string()), false, None, None));
    assert_eq!(
        crate::session_runs::run_binding_conn(&connection, &session, &input).expect("run of input"),
        Some(run)
    );
    connection
        .execute_batch(crate::durable::GRAPH_TABLES)
        .expect("graph schema");
    connection
        .execute(
            "UPDATE session_runs SET terminal_kind = 'answered', terminal_cause_json = '{\"cause\":\"commands_applied\"}',
                 terminal_at_ms = 42, terminal_head_revision = 1",
            [],
        )
        .expect("terminal");
    for revision in [1, 2] {
        crate::revisions::record_revision_conn(&connection, &session, revision, None, None, "{}")
            .expect("revision");
    }
    connection
        .execute(
            "INSERT INTO session_head (session_id, head_revision) VALUES ('session', 2)",
            [],
        )
        .expect("head");
    crate::revisions::pin_conn(
        &connection,
        &session,
        &lash_core_store::store::Target::Input(input),
    )
    .expect("pin");
    let retained_revisions = || {
        connection
            .prepare("SELECT head_revision FROM session_revisions WHERE session_id = 'session' ORDER BY head_revision")
            .expect("retained revisions")
            .query_map([], |row| row.get::<_, i64>(0))
            .expect("read revisions")
            .collect::<Result<Vec<_>, _>>()
            .expect("revision rows")
    };
    // A host collection: the release, then the retirement of what it held.
    let collect = || {
        let now = lash_durable::DurableInstant(0);
        crate::revisions::release_unretained_conn(&connection, true, Some(&session), now)
            .expect("collect");
        let budget = lash_durable::domain::pages::RetirementBudget { nodes: 64 };
        while crate::durable::retire_step(&connection, budget, now)
            .expect("a retirement step")
            .holds_advanced
            > 0
        {}
    };
    collect();
    assert_eq!(
        retained_revisions(),
        vec![1, 2],
        "the input pin retains revision 1"
    );
    connection.execute("DELETE FROM pins", []).expect("unpin");
    assert_eq!(
        retained_revisions(),
        vec![1, 2],
        "unpin alone does not release a revision"
    );
    // Revision 1 is released here, but its config body is still rooted by
    // head revision 2.
    collect();
    assert_eq!(
        retained_revisions(),
        vec![2],
        "collection releases the unpinned revision"
    );
    let root: String = connection.query_row(
        "SELECT config_ref FROM session_revisions WHERE session_id = 'session' AND head_revision = 2", [], |row| row.get(0)
    ).expect("the head retains its config graph");
    let root = lash_core_store::checkpoint_graph::NodeId::from_hex(&root).expect("node id");
    let limits = lash_core_store::checkpoint_graph::DecodeLimits::within_total(1 << 20);
    let graph = crate::durable::graph_closure(&connection, root, &limits)
        .expect("SQL")
        .expect("closure");
    let value: serde_json::Value = lash_core_store::checkpoint_graph::config::load(
        &mut lash_core_store::checkpoint_graph::GraphDecoder::new(&graph, limits),
        root,
    )
    .expect("recorded config");
    assert_eq!(value, serde_json::json!({}));
    collect();
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'session_run_inputs'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .expect("schema"),
        0
    );
}

/// FORK-REBIND: releasing a live input changes its next owner in one column.
#[test]
fn a_fork_rebinds_only_the_inputs_the_old_run_released() {
    let connection = input_run_sql_fixture();
    let session = SessionId::from("session");
    let input = lash_protocol_types::InputId::from("input");
    let run = lash_protocol_types::TurnId::from("run");
    crate::session_runs::bind_run_inputs_conn(
        &connection,
        &session,
        &run,
        std::slice::from_ref(&input),
    )
    .expect("bind");
    connection
        .execute(
            crate::turn_ingress::turn_ingress_sql()
                .pending_inputs
                .release_run
                .sql(),
            params!["session", "run"],
        )
        .expect("fork release");
    let fork = lash_protocol_types::TurnId::from("fork");
    crate::session_runs::bind_run_inputs_conn(
        &connection,
        &session,
        &fork,
        std::slice::from_ref(&input),
    )
    .expect("fork bind");
    assert_eq!(
        crate::session_runs::run_binding_conn(&connection, &session, &input).expect("binding"),
        Some(fork)
    );
    assert!(
        crate::session_runs::bind_run_inputs_conn(&connection, &session, &run, &[input]).is_err(),
        "a competing run cannot steal the fork's input"
    );
}

/// TERMINAL-FENCES-LATE-CHILD: a turn's terminal is its only close fact.
#[test]
fn a_terminal_turn_refuses_registration_without_a_parent_end_row() {
    let connection = rusqlite::Connection::open_in_memory().expect("SQLite");
    for ddl in crate::schema::provisioning_statements() {
        connection.execute_batch(ddl).expect("schema");
    }
    connection.execute(
        "INSERT INTO session_runs (session_id, run, terminal_kind, terminal_cause_json, terminal_at_ms)
         VALUES ('session', 'run', 'answered', '{\"cause\":\"commands_applied\"}', 42)",
        [],
    ).expect("terminal");
    connection.execute(
        "INSERT INTO pending_turn_inputs (enqueue_seq, input_id, session_id, source_key, ingress_json,
             state, input_json, submission_digest, enqueued_at_ms, admitted_run, terminal_at_ms)
         VALUES (1, 'input', 'session', 'alias', '{\"scope\":\"next_turn\"}', 'completed', '{}', 'digest', 0, 'run', 42)", []
    ).expect("settled alias");
    let scope = lash_core_store::scope_identity::ScopeId::turn("session", "run");
    crate::process_registry::parent_end::record_conn(
        &connection,
        &scope,
        42,
        lash_core_store::store::FleetFormat::current(),
    )
    .expect("close");
    let plans: i64 = connection
        .query_row("SELECT count(*) FROM parent_end_plans", [], |row| {
            row.get(0)
        })
        .expect("plans");
    assert_eq!(plans, 0, "turn closure must come from its terminal");
    for scope in [
        scope,
        lash_core_store::scope_identity::ScopeId::turn("session", "alias"),
    ] {
        for lifetime in [
            lash_execution_contracts::process::LifetimeDecision::Detached,
            lash_execution_contracts::process::LifetimeDecision::Until {
                scope: scope.clone(),
                grant: lash_execution_contracts::process::ScopeGrant::Ancestor,
            },
        ] {
            let mut registration = lash_execution_contracts::testing::held_engine_registration(
                serde_json::json!({}),
                lash_execution_contracts::process::ProcessProvenance::host(),
                lash_execution_contracts::process::Lifetime::Detached,
            );
            registration.ancestry =
                lash_execution_contracts::process::Ancestry::from_scopes([scope.clone()]);
            registration.lifetime = lifetime;
            assert!(matches!(
                SqliteProcessRegistry::apply_registration_conn(
                    &connection, registration, vec![], ProcessId::fixture("late-child"), false, 43,
                    lash_core_store::store::FleetFormat::current(),
                ),
                Err(lash_execution_contracts::plugin_error::PluginError::ParentEnded { parent, .. }) if parent == scope
            ));
        }
    }
}

#[test]
fn queued_work_checks_reject_illegal_vocabulary() {
    let connection = rusqlite::Connection::open_in_memory().expect("open SQLite CHECK witness");
    connection
        .execute_batch(crate::schema::SCHEMA)
        .expect("apply SQLite schema to CHECK witness");
    let assert_rejected = |statement: &str, constraint: &str| {
        let error = connection
            .execute(statement, [])
            .expect_err("an illegal queued-work row must violate its schema CHECK");
        assert!(
            error.to_string().contains(constraint),
            "SQLite reported the wrong CHECK: {error}"
        );
    };

    assert_rejected(
        "INSERT INTO queued_work_batches (enqueue_seq,
             batch_id, session_id, delivery_policy, authority_json,
             submission_digest, enqueued_at_ms, payload_json
         ) VALUES (1, 'bad-policy', 'session', 'eventually', '{}', 'digest', 0, json_object('type', 'session_command'))",
        "ck_queued_work_batches_delivery_policy",
    );
}

#[test]
fn ingress_admission_binding_must_be_all_or_none() {
    let connection = rusqlite::Connection::open_in_memory().expect("open SQLite CHECK witness");
    connection
        .execute_batch(crate::schema::SCHEMA)
        .expect("apply SQLite schema to CHECK witness");
    // A row is open or admitted to a run by a recorded step: a run without
    // its step, or a step without its run, is unrepresentable (FIG-3927).
    for (fields, values) in [("admitted_run", "'root'"), ("admitted_by", "'admit'")] {
        let error = connection
            .execute(
                &format!(
                    "INSERT INTO pending_turn_inputs (enqueue_seq,
                         input_id, session_id, ingress_json, state, input_json,
                         submission_digest, enqueued_at_ms, {fields}
                     ) VALUES (1, 'input', 'session', '{{\"scope\":\"next_turn\"}}',
                               'deferred_next_turn', '{{}}', 'digest', 0, {values})"
                ),
                [],
            )
            .expect_err("a half-bound pending input must be rejected");
        assert!(
            error
                .to_string()
                .contains("ck_pending_turn_inputs_admission_all_or_none"),
            "SQLite reported the wrong CHECK: {error}"
        );
        let error = connection
            .execute(
                &format!(
                    "INSERT INTO queued_work_batches (enqueue_seq,
                         batch_id, session_id, delivery_policy, authority_json,
                         submission_digest, enqueued_at_ms, payload_json, {fields}
                     ) VALUES (1, 'batch', 'session', 'earliest_safe_boundary',
                               '{{}}', 'digest', 0, json_object('type', 'session_command'), {values})"
                ),
                [],
            )
            .expect_err("a half-bound batch must be rejected");
        assert!(
            error
                .to_string()
                .contains("ck_queued_work_batches_admission_all_or_none"),
            "SQLite reported the wrong CHECK: {error}"
        );
    }
    // A settled input is answered, so no run holds it.
    let error = connection
        .execute(
            "INSERT INTO pending_turn_inputs (enqueue_seq,
                 input_id, session_id, ingress_json, state, input_json,
                 submission_digest, enqueued_at_ms,
                 admitted_run, admitted_by
             ) VALUES (1, 'settled', 'session', '{\"scope\":\"next_turn\"}',
                       'completed', '{}', 'digest', 0, 'root', 'admit')",
            [],
        )
        .expect_err("a settled input still bound to a run must be rejected");
    assert!(
        error
            .to_string()
            .contains("ck_pending_turn_inputs_settled_unadmitted"),
        "SQLite reported the wrong CHECK: {error}"
    );
}

/// FIG-3886: the `(session_id, source_key)` dedup on both admission tables
/// aborts instead of ignoring — which stored row an id names is the admission
/// verdict's call, never the constraint's — and a violation the pre-insert
/// read missed maps to the same typed identity conflict an `input_id` reuse
/// gets, the one PostgreSQL's `23505` already produces.
#[test]
fn pending_turn_inputs_reject_a_duplicate_source_key_insert() {
    let connection =
        rusqlite::Connection::open_in_memory().expect("open SQLite constraint witness");
    connection
        .execute_batch(crate::schema::SCHEMA)
        .expect("apply SQLite schema to constraint witness");
    let insert = "INSERT INTO pending_turn_inputs (enqueue_seq,
             input_id, session_id, source_key, ingress_json, state, input_json,
             submission_digest, enqueued_at_ms
         ) VALUES (?1, ?2, 'session', ?3, '{\"scope\":\"next_turn\"}',
                   'deferred_next_turn', '{}', 'digest', 0)";
    connection
        .execute(insert, rusqlite::params![1, "first", "key"])
        .expect("admit the first row");
    let error = connection
        .execute(insert, rusqlite::params![2, "second", "key"])
        .expect_err("a duplicate (session_id, source_key) insert must abort");
    let mapped = crate::sqlite_pending_turn_input_insert_error(
        error,
        &SessionId::from("session"),
        &lash_protocol_types::InputId::from("second"),
    );
    assert!(
        matches!(
            &mapped,
            StoreError::PendingTurnInputIdConflict { session_id, input_id }
                if session_id.as_str() == "session" && input_id.as_str() == "second"
        ),
        "the duplicate source-key insert maps to the typed identity conflict: {mapped:?}"
    );
    let error = connection
        .execute(insert, rusqlite::params![3, "first", "other-key"])
        .expect_err("a duplicate input_id insert must abort");
    let mapped = crate::sqlite_pending_turn_input_insert_error(
        error,
        &SessionId::from("session"),
        &lash_protocol_types::InputId::from("first"),
    );
    assert!(
        matches!(&mapped, StoreError::PendingTurnInputIdConflict { .. }),
        "the duplicate input_id insert maps to the typed identity conflict: {mapped:?}"
    );
}

#[test]
fn queued_work_batches_reject_a_duplicate_source_key_insert() {
    let connection =
        rusqlite::Connection::open_in_memory().expect("open SQLite constraint witness");
    connection
        .execute_batch(crate::schema::SCHEMA)
        .expect("apply SQLite schema to constraint witness");
    let insert = "INSERT INTO queued_work_batches (enqueue_seq,
             batch_id, session_id, source_key, delivery_policy,
             authority_json, submission_digest, enqueued_at_ms, payload_json
         ) VALUES (?1, ?2, 'session', 'key', 'earliest_safe_boundary',
                   '{}', 'digest', 0, json_object('type', 'session_command'))";
    connection
        .execute(insert, rusqlite::params![1, "first"])
        .expect("admit the first row");
    let error = connection
        .execute(insert, rusqlite::params![2, "second"])
        .expect_err("a duplicate (session_id, source_key) insert must abort");
    assert!(
        error.to_string().contains(
            "UNIQUE constraint failed: queued_work_batches.session_id, queued_work_batches.source_key"
        ),
        "the duplicate source-key insert names its constraint: {error}"
    );
}

/// D-DEFAULTS2: a file store set runs under the `synchronous` mode its host
/// states, on the connection its writes commit through.
#[tokio::test]
async fn a_file_store_set_runs_under_the_synchronous_mode_its_host_states() {
    for (stated, pragma) in [
        (SqliteSynchronous::Off, 0),
        (SqliteSynchronous::Normal, 1),
        (SqliteSynchronous::Full, 2),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let stores = crate::SqliteStoreSet::open(dir.path().join("lash.db"), stated)
            .await
            .expect("open a file store set");
        assert_eq!(stores.options().store.connection_policy.synchronous, stated);
        let applied = stores
            .process_env_store()
            .conn
            .call(|connection| {
                connection.query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0))
            })
            .await
            .expect("read the pragma");
        assert_eq!(applied, pragma, "{stated:?}");
    }
}

#[tokio::test]
async fn store_options_apply_connection_policy_on_connection_thread() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("policy.db");
    let store = SqliteStore::open_file_with_options_for_testing(
        &path,
        StoreOptions {
            blob_profile: BuiltinBlobProfile::Balanced,
            connection_policy: SqliteConnectionPolicy {
                read_connections: std::num::NonZeroUsize::new(4).expect("four is nonzero"),
                busy_timeout: std::time::Duration::from_millis(321),
                synchronous: SqliteSynchronous::Full,
                wal_autocheckpoint_pages: 17,
                cache_size: -4096,
                ..SqliteConnectionPolicy::standard(SqliteSynchronous::Full)
            },
        },
    )
    .await
    .expect("open store with connection policy");

    let pragmas = store
        .conn
        .call(|connection| {
            Ok((
                connection.query_row("PRAGMA busy_timeout", [], |row| row.get::<_, i64>(0))?,
                connection.query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0))?,
                connection
                    .query_row("PRAGMA wal_autocheckpoint", [], |row| row.get::<_, i64>(0))?,
                connection.query_row("PRAGMA cache_size", [], |row| row.get::<_, i64>(0))?,
                connection.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?,
            ))
        })
        .await
        .expect("read connection policy pragmas on connection thread");

    assert_eq!(pragmas, (321, 2, 17, -4096, "wal".to_string()));
}

fn count_checkpoint_data_statement(event: rusqlite::trace::TraceEvent<'_>) {
    if let rusqlite::trace::TraceEvent::Stmt(_, sql) = event {
        let sql = sql.trim_start();
        if ["SELECT", "INSERT", "UPDATE", "DELETE", "WITH"]
            .iter()
            .any(|prefix| sql.starts_with(prefix))
        {
            CHECKPOINT_DATA_STATEMENT_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn count_session_list_statement(event: rusqlite::trace::TraceEvent<'_>) {
    // Count EVERY statement in the traced window, not just the catalog CTE:
    // the invariant is that listing issues one statement total, so a
    // reintroduced per-session read must be visible to this counter.
    if let rusqlite::trace::TraceEvent::Stmt(..) = event {
        SESSION_LIST_STATEMENT_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

async fn traced_session_list(store: &SqliteStore) -> (Vec<SessionView>, usize) {
    let conn = SqliteConnection::open_readonly(store.location.target())
        .await
        .expect("open session catalog for statement tracing");
    SESSION_LIST_STATEMENT_COUNT.store(0, Ordering::Relaxed);
    let summaries = conn
        .call(|conn| {
            conn.trace_v2(
                rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
                Some(count_session_list_statement as fn(rusqlite::trace::TraceEvent<'_>)),
            );
            let result = list_session_views(conn, &SessionListFilter::default());
            conn.trace_v2(rusqlite::trace::TraceEventCodes::empty(), None);
            result
        })
        .await
        .expect("list session summaries under statement trace");
    (
        summaries,
        SESSION_LIST_STATEMENT_COUNT.load(Ordering::Relaxed),
    )
}

#[tokio::test]
async fn session_listing_statement_count_is_session_count_invariant() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SqliteStore::open(
        &dir.path().join("lash.db"),
        crate::SqliteSynchronous::Normal,
    )
    .await
    .expect("open catalog");
    let mut expected_relations = BTreeMap::new();

    for index in 0..8 {
        let session_id = SessionId::fixture(format!("listing-statement-count-{index}"));
        let relation = if index == 0 {
            lash_core_store::session_identity::SessionRelation::Root
        } else {
            lash_core_store::session_identity::SessionRelation::Fork {
                source_session_id: SessionId::from("listing-statement-count-0"),
                source_node_id: Some(lash_core::NodeId::fixture(format!("source-node-{index}"))),
            }
        };
        store
            .admit_session(&SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: if index == 0 {
                    Vec::new()
                } else {
                    vec![
                        lash_core_store::session_identity::SessionObserverIntent::host_requested(
                            ProcessId::fixture(&format!("intent-process-{index}")),
                        ),
                    ]
                },
                session_id: session_id.clone(),
                relation: relation.clone(),
                config: lash_core_store::session_graph::PersistedSessionConfig::new(
                    lash_protocol_types::session_model::TurnBudget::Unbounded,
                    lash_protocol_types::session_model::MaxToolCalls::new(1024),
                    lash_core::NoProgressBudget::bounded(12),
                ),
                head: lash_core_store::session_store_factory_types::SessionCreationHead::Config,
                retention: lash_core_store::store::Retention::UntilGc,
            })
            .await
            .expect("create session listing fixture");
        expected_relations.insert(session_id, relation);

        if index == 0 {
            let (single, statement_count) = traced_session_list(&store).await;
            assert_eq!(single.len(), 1);
            assert_eq!(statement_count, 1);
        }
    }

    let (many, statement_count) = traced_session_list(&store).await;
    assert_eq!(statement_count, 1);
    assert_eq!(many.len(), expected_relations.len());
    for summary in many {
        assert_eq!(
            summary.relation(),
            expected_relations.get(&summary.session_id)
        );
    }
}

async fn set_checkpoint_statement_trace(store: &SqliteStore, enabled: bool) {
    store
        .conn
        .call(move |conn| {
            conn.trace_v2(
                if enabled {
                    rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT
                } else {
                    rusqlite::trace::TraceEventCodes::empty()
                },
                enabled.then_some(
                    count_checkpoint_data_statement as fn(rusqlite::trace::TraceEvent<'_>),
                ),
            );
            Ok(())
        })
        .await
        .expect("configure SQLite checkpoint statement trace");
}

fn checkpoint_with_changed_components(depth: usize) -> HydratedSessionCheckpoint {
    HydratedSessionCheckpoint {
        components: (0..depth)
            .map(|index| {
                (
                    format!("arbitrary/depth-invariance/{index:05}"),
                    lash_core_store::store::HydratedCheckpointComponent::changed(
                        format!("depth-invariance-body-{index:05}").into_bytes(),
                    ),
                )
            })
            .collect(),
        ..Default::default()
    }
}

fn checkpoint_with_unchanged_components(manifest: &SessionCheckpoint) -> HydratedSessionCheckpoint {
    HydratedSessionCheckpoint {
        turn_state: manifest.turn_state.clone(),
        components: manifest
            .components
            .iter()
            .map(|(key, descriptor)| {
                (
                    key.clone(),
                    lash_core_store::store::HydratedCheckpointComponent::unchanged(descriptor),
                )
            })
            .collect(),
        execution: None,
        namespace_nodes: Default::default(),
    }
}

async fn durable_state(
    store: &SqliteStore,
    session_id: &SessionId,
) -> lash_core_store::session_state::RuntimeSessionState {
    let state = lash_core_store::session_state::RuntimeSessionState {
        session_id: session_id.clone(),
        ..lash_core_store::session_state::RuntimeSessionState::fixture(
            lash_core_store::session_policy::SessionPolicy::new(
                lash_protocol_types::session_model::TurnBudget::Unbounded,
                lash_protocol_types::session_model::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            ),
        )
    };
    store
        .admit_session(&SessionStoreCreateRequest {
            session_id: session_id.clone(),
            relation: lash_core_store::session_identity::SessionRelation::Root,
            config: lash_core_store::session_graph::PersistedSessionConfig::from_policy(
                &state.policy.clone(),
            ),
            head: lash_core_store::session_store_factory_types::SessionCreationHead::Config,
            pending_observer_intents: Vec::new(),
            owning_process_id: None,
            retention: lash_core_store::store::Retention::UntilGc,
        })
        .await
        .expect("admit SQLite test session");
    state
}

lash_conformance::checkpoint_admission_probe_tests!({
    let store = crate::test_support::sqlite_memory_store()
        .await
        .expect("open counter store");
    let counting_store = Arc::clone(&store);
    (
        (),
        store as Arc<dyn lash_core_store::store::RuntimeStore>,
        SessionId::from("sqlite-checkpoint-counter"),
        move || counting_store.checkpoint_admission_counts(),
        // An in-memory SQLite store needs no session teardown.
        async {},
    )
});

#[tokio::test]
async fn checkpoint_component_statement_count_is_depth_invariant() {
    let mut observed = Vec::new();
    for depth in [10, 100, 1_000, 4_000] {
        let store = crate::test_support::sqlite_memory_store()
            .await
            .expect("open depth-invariance store");
        let mut state = durable_state(
            &store,
            &SessionId::fixture(format!("sqlite-checkpoint-depth-{depth}")),
        )
        .await;
        let mut seed = RuntimeCommit::persisted_state_for_test(&state);
        seed.checkpoint = checkpoint_with_changed_components(depth);
        let seeded = store
            .commit_runtime_state(seed)
            .await
            .expect("seed checkpoint component bodies");
        state.head_revision = seeded.head_revision;
        let mut unchanged = RuntimeCommit::persisted_state_for_test(&state);
        unchanged.checkpoint = checkpoint_with_unchanged_components(&seeded.manifest);
        assert!(
            unchanged
                .checkpoint
                .components
                .values()
                .all(|component| component.body().is_none()),
            "measured commit must carry zero changed component bodies"
        );

        CHECKPOINT_DATA_STATEMENT_COUNT.store(0, Ordering::Relaxed);
        set_checkpoint_statement_trace(&store, true).await;
        let commit_started = std::time::Instant::now();
        store
            .commit_runtime_state(unchanged)
            .await
            .expect("commit unchanged checkpoint component refs");
        let commit_elapsed = commit_started.elapsed();
        set_checkpoint_statement_trace(&store, false).await;
        let commit_statements = CHECKPOINT_DATA_STATEMENT_COUNT.load(Ordering::Relaxed);

        CHECKPOINT_DATA_STATEMENT_COUNT.store(0, Ordering::Relaxed);
        set_checkpoint_statement_trace(&store, true).await;
        let load_started = std::time::Instant::now();
        let loaded = store
            .load_session_window(
                &state.session_id,
                lash_core_store::store::WindowSelector::Current,
            )
            .await
            .expect("load checkpoint component bodies")
            .expect("stored checkpoint session");
        let load_elapsed = load_started.elapsed();
        set_checkpoint_statement_trace(&store, false).await;
        let load_statements = CHECKPOINT_DATA_STATEMENT_COUNT.load(Ordering::Relaxed);

        assert_eq!(
            loaded
                .checkpoint
                .expect("loaded checkpoint")
                .components
                .len(),
            depth
        );
        observed.push((depth, commit_statements, load_statements));
        eprintln!(
            "sqlite checkpoint depth={depth} commit_statements={commit_statements} load_statements={load_statements} commit_ms={:.3} load_ms={:.3}",
            commit_elapsed.as_secs_f64() * 1_000.0,
            load_elapsed.as_secs_f64() * 1_000.0,
        );
    }
    assert!(
        observed
            .iter()
            .all(|(_, commit, load)| { *commit == observed[0].1 && *load == observed[0].2 }),
        "checkpoint commit/load statement counts must be independent of component depth: {observed:?}"
    );
}

/// One pair of runtime commits over a fresh file store: a same-operation
/// pair, or a head CAS pair from one head. Committed one after the other or,
/// `grouped`, queued behind a held connection thread into one write batch.
/// Returns each commit's answer, the head after both, and the commits and
/// rollbacks that ended the pair's transactions.
async fn commit_pair(same_operation: bool, grouped: bool) -> (Vec<String>, u64, (usize, usize)) {
    use std::future::Future as _;
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SqliteStore::open_file_with_options_for_testing(
        &dir.path().join("pair.db"),
        StoreOptions::standard(SqliteSynchronous::Full),
    )
    .await
    .expect("open writer");
    let session = SessionId::from("grouped-pair");
    let state = durable_state(&store, &session).await;
    let commit = |name: &str| {
        let mut state = state.clone();
        state.append_active_read_delta(&[lash_protocol_types::session_model::Message {
            id: name.to_owned(),
            role: lash_protocol_types::session_model::MessageRole::User,
            origin: None,
            parts: Arc::new(vec![lash_protocol_types::session_model::Part::text(
                format!("{name}.p0"),
                name.to_owned(),
                None,
            )]),
            reply_marker: None,
        }]);
        RuntimeCommit::persisted_state_with_operation(
            &mut state,
            lash_core_store::store::OperationId::turn(
                &session,
                lash_protocol_types::TurnId::fixture(name.to_owned()),
                "commit",
            ),
        )
        .expect("prepare the commit")
        .0
    };
    let (first, second) = if same_operation {
        (commit("same"), commit("same"))
    } else {
        (commit("left"), commit("right"))
    };
    let commits = Arc::new(AtomicUsize::new(0));
    let rollbacks = Arc::new(AtomicUsize::new(0));
    let (counted_commits, counted_rollbacks) = (Arc::clone(&commits), Arc::clone(&rollbacks));
    store
        .conn
        .call(move |conn| {
            conn.commit_hook(Some(move || {
                counted_commits.fetch_add(1, Ordering::SeqCst);
                false
            }));
            conn.rollback_hook(Some(move || {
                counted_rollbacks.fetch_add(1, Ordering::SeqCst);
            }));
            Ok(())
        })
        .await
        .expect("count the writer's transaction ends");
    let results = if grouped {
        let (entered, held) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let conn = store.conn.clone();
        let holding = tokio::spawn(async move {
            conn.call(move |_| {
                let _ = entered.send(());
                let _ = released.recv();
                Ok(())
            })
            .await
        });
        held.await.expect("the connection thread holds");
        let mut left = Box::pin(store.commit_runtime_state(first));
        let mut right = Box::pin(store.commit_runtime_state(second));
        std::future::poll_fn(|cx| {
            assert!(left.as_mut().poll(cx).is_pending());
            assert!(right.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(release);
        holding.await.expect("hold task").expect("the hold returns");
        vec![left.await, right.await]
    } else {
        vec![
            store.commit_runtime_state(first).await,
            store.commit_runtime_state(second).await,
        ]
    };
    let answers = results
        .into_iter()
        .map(|result| match result {
            Ok(receipt) => format!(
                "head {} replayed {}",
                receipt.head_revision, receipt.receipt_replayed
            ),
            Err(error) => format!("{error:?}"),
        })
        .collect();
    let head = store
        .load_session_head_meta(&session)
        .await
        .expect("head after the pair")
        .expect("the session has a head")
        .head_revision;
    (
        answers,
        head,
        (
            commits.load(Ordering::SeqCst),
            rollbacks.load(Ordering::SeqCst),
        ),
    )
}

/// FIG-5970: two runtime commits grouped into one write batch, sharing one
/// transaction that ends in one `COMMIT` and no rollback, answer exactly as
/// the same pair committed one after the other: a same-operation pair
/// publishes once and replays once, and a head CAS pair from one head lands
/// once and refuses the other with `HeadRevisionConflict`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grouped_runtime_commits_answer_exactly_as_their_solo_commits() {
    for same_operation in [true, false] {
        let (alone, alone_head, _) = commit_pair(same_operation, false).await;
        let (grouped, grouped_head, ends) = commit_pair(same_operation, true).await;
        assert_eq!(ends, (1, 0), "the pair shares one transaction");
        assert_eq!(grouped, alone, "a grouped commit answers as alone");
        assert_eq!(grouped_head, alone_head);
        assert_eq!(grouped_head, 1, "exactly one commit of the pair applied");
        if same_operation {
            assert_eq!(alone, ["head 1 replayed false", "head 1 replayed true"]);
        } else {
            assert_eq!(alone[0], "head 1 replayed false");
            assert!(
                alone[1].starts_with("HeadRevisionConflict"),
                "the second CAS is refused: {alone:?}"
            );
        }
    }
}

/// Real SQLITE_FULL on the owning writer publishes neither head nor
/// receipt; after capacity returns the identical commit applies once.
#[tokio::test]
async fn real_page_exhaustion_rolls_back_and_the_exact_retry_commits_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("full.db");
    let store = SqliteStore::open_file_with_options_for_testing(
        &path,
        StoreOptions {
            blob_profile: BuiltinBlobProfile::LowLatency,
            ..StoreOptions::standard(SqliteSynchronous::Normal)
        },
    )
    .await
    .expect("open writer");
    let session = SessionId::from("real-full");
    let state = durable_state(&store, &session).await;
    let before = store
        .load_session_head_meta(&session)
        .await
        .expect("head before")
        .expect("created head");
    let mut commit = RuntimeCommit::persisted_state_for_test(&state);
    commit.checkpoint.components.insert(
        "full/body".to_owned(),
        lash_core_store::store::HydratedCheckpointComponent::changed(vec![b'x'; 524_288]),
    );
    store
        .conn
        .call(|conn| {
            let pages: u64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
            let limit: u64 =
                conn.query_row(&format!("PRAGMA max_page_count = {pages}"), [], |row| {
                    row.get(0)
                })?;
            assert_eq!(limit, pages);
            Ok(())
        })
        .await
        .expect("limit the actual writer to its current pages");
    let error = store
        .commit_runtime_state(commit.clone())
        .await
        .expect_err("SQLite must exhaust its pages");
    assert!(
        matches!(&error, StoreError::StorageFailure { backend: "sqlite", message } if message == "database or disk is full"),
        "exact SQLite FULL refusal: {error:?}",
    );
    let after = store
        .load_session_head_meta(&session)
        .await
        .expect("head after refusal")
        .expect("created head retained");
    assert_eq!(after.head_revision, before.head_revision);
    assert_eq!(after.checkpoint_ref, before.checkpoint_ref);
    assert_eq!(after.leaf_node_id, before.leaf_node_id);
    let receipts = store
        .conn
        .call(|conn| {
            conn.query_row(
                "SELECT count(*) FROM runtime_turn_commits WHERE session_id = 'real-full'",
                [],
                |row| row.get::<_, i64>(0),
            )
        })
        .await
        .expect("receipt count after refusal");
    assert_eq!(receipts, 0);
    store
        .conn
        .call(|conn| {
            conn.query_row("PRAGMA max_page_count = 2147483646", [], |row| {
                row.get::<_, u64>(0)
            })
        })
        .await
        .expect("restore writer capacity");
    let receipt = store
        .commit_runtime_state(commit.clone())
        .await
        .expect("retry the identical valid commit");
    assert_eq!(receipt.head_revision, 1);
    assert!(!receipt.receipt_replayed);
    drop(store);
    let reopened = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("reopen committed database");
    let head = reopened
        .load_session_head_meta(&session)
        .await
        .expect("reopened head")
        .expect("head exists");
    assert_eq!(head.head_revision, receipt.head_revision);
    assert_eq!(head.checkpoint_ref, Some(receipt.checkpoint_ref.clone()));
    let replayed = reopened
        .commit_runtime_state(commit)
        .await
        .expect("same receipt after reopen");
    assert!(replayed.receipt_replayed);
    assert_eq!(replayed.head_revision, receipt.head_revision);
    assert_eq!(replayed.checkpoint_ref, receipt.checkpoint_ref);
    let receipts = reopened
        .conn
        .call(|conn| {
            conn.query_row(
                "SELECT count(*) FROM runtime_turn_commits WHERE session_id = 'real-full'",
                [],
                |row| row.get::<_, i64>(0),
            )
        })
        .await
        .expect("receipt count after retry and replay");
    assert_eq!(receipts, 1);
}

#[tokio::test]
async fn real_locked_catalog_surfaces_typed_contention() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("contended.db");
    let store = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("open store");
    let state = durable_state(&store, &SessionId::from("contended")).await;
    store
        .conn
        .call(|conn| {
            conn.busy_timeout(std::time::Duration::ZERO)?;
            Ok(())
        })
        .await
        .expect("disable busy wait");

    let locker = rusqlite::Connection::open(&path).expect("open lock holder");
    locker
        .execute_batch("BEGIN IMMEDIATE")
        .expect("hold catalog writer lock");
    let result = store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
        .await;
    locker
        .execute_batch("ROLLBACK")
        .expect("release writer lock");

    assert!(matches!(result, Err(StoreError::Contended)));
}

#[tokio::test]
async fn live_attachment_refs_aborts_on_unreadable_catalog() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("sessions.db");
    std::fs::write(&root, b"corrupt not-a-db").expect("write corrupt");

    let result = SqliteStore::open(&root, crate::SqliteSynchronous::Normal).await;
    assert!(
        result.is_err(),
        "an unreadable durable-core catalog must refuse open"
    );
}

#[tokio::test]
async fn catalog_lookup_and_repeated_admission_share_a_readable_snapshot() {
    let store = crate::test_support::sqlite_memory_store()
        .await
        .expect("open memory catalog");
    let request = SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("catalog-lookup"),
        relation: lash_core_store::session_identity::SessionRelation::Root,
        config: lash_core_store::session_graph::PersistedSessionConfig::new(
            lash_protocol_types::session_model::TurnBudget::Unbounded,
            lash_protocol_types::session_model::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ),
        head: lash_core_store::session_store_factory_types::SessionCreationHead::Config,
        retention: lash_core_store::store::Retention::UntilGc,
    };
    assert!(matches!(
        store.admit_session(&request).await.expect("admit session"),
        lash_core_store::store::SessionAdmission::Created
    ));
    assert!(matches!(
        store
            .lookup_session(&request.session_id)
            .await
            .expect("look up live session"),
        lash_core_store::store::SessionLookup::Live(_)
    ));
    assert!(matches!(
        store
            .admit_session(&request)
            .await
            .expect("readmit session"),
        lash_core_store::store::SessionAdmission::Rebound
    ));
}

#[tokio::test]
async fn lookup_session_aborts_on_unreadable_requested_session_meta() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("sessions");
    let store = SqliteStore::open(&root, crate::SqliteSynchronous::Normal)
        .await
        .expect("open catalog");
    let request = SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("corrupt-session-meta"),
        relation: lash_core_store::session_identity::SessionRelation::Root,
        config: lash_core_store::session_graph::PersistedSessionConfig::new(
            lash_protocol_types::session_model::TurnBudget::Unbounded,
            lash_protocol_types::session_model::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ),
        head: lash_core_store::session_store_factory_types::SessionCreationHead::Config,
        retention: lash_core_store::store::Retention::UntilGc,
    };

    store
        .admit_session(&request)
        .await
        .expect("create requested session");
    let raw = rusqlite::Connection::open(&root).expect("open raw catalog");
    // `ck_session_meta_relation_kind` forbids this row on any ordinary write.
    // The refusal below is still the contract for a catalog that carries one
    // anyway — restored from a pre-CHECK dump, or ALTERed by a host.
    raw.pragma_update(None, "ignore_check_constraints", true)
        .expect("permit manufacturing a row the DDL now forbids");
    raw.execute(
        "UPDATE session_meta SET relation_kind = 'corrupt'
             WHERE session_id = ?1",
        params![request.session_id.as_str()],
    )
    .expect("corrupt requested session metadata");
    raw.pragma_update(None, "ignore_check_constraints", false)
        .expect("restore CHECK enforcement");
    drop(raw);

    let result = store.lookup_session(&request.session_id).await;
    assert!(
        result.is_err(),
        "unreadable requested session metadata must not look absent"
    );
}

#[tokio::test]
async fn concurrent_admission_creates_both_sessions_in_one_catalog() {
    let dir = tempfile::tempdir().expect("admission tempdir");
    let store = SqliteStore::open(
        &dir.path().join("lash.db"),
        crate::SqliteSynchronous::Normal,
    )
    .await
    .expect("open catalog");
    let request = |session_id: &str| SessionStoreCreateRequest {
        session_id: SessionId::fixture(session_id),
        relation: lash_core_store::session_identity::SessionRelation::Root,
        config: lash_core_store::session_graph::PersistedSessionConfig::new(
            lash_protocol_types::session_model::TurnBudget::Unbounded,
            lash_protocol_types::session_model::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ),
        head: lash_core_store::session_store_factory_types::SessionCreationHead::Config,
        pending_observer_intents: Vec::new(),
        owning_process_id: None,
        retention: lash_core_store::store::Retention::UntilGc,
    };
    let first = request("admission-a");
    let second = request("admission-b");
    let (a, b) = tokio::join!(store.admit_session(&first), store.admit_session(&second));
    assert!(matches!(
        a,
        Ok(lash_core_store::store::SessionAdmission::Created)
    ));
    assert!(matches!(
        b,
        Ok(lash_core_store::store::SessionAdmission::Created)
    ));
    for session_id in [&first.session_id, &second.session_id] {
        assert!(matches!(
            store
                .lookup_session(session_id)
                .await
                .expect("lookup admitted session"),
            lash_core_store::store::SessionLookup::Live(_)
        ));
    }
}

/// FIG-4649: a registry or trigger SQL fault reaches the plugin boundary
/// through the store's mapper, so it keeps its class: a busy or locked
/// database and a failed substrate are retried, never a session error a
/// recorded step would journal as its answer.
#[test]
fn a_registry_sql_fault_is_a_retryable_store_fault_at_the_plugin_boundary() {
    use lash_core_store::store::StoreFault;
    use lash_execution_contracts::plugin_error::PluginError;

    for code in [rusqlite::ffi::SQLITE_BUSY, rusqlite::ffi::SQLITE_LOCKED] {
        let fault = process_sqlite_error(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            Some("database is locked".to_string()),
        ));
        assert!(
            matches!(
                fault,
                PluginError::StoreUnavailable {
                    fault: StoreFault::Contended
                }
            ),
            "{code}: {fault:?}"
        );
        assert!(fault.is_retryable() && !fault.is_terminal(), "{code}");
    }
    let fault = process_sqlite_error(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
        Some("disk I/O error".to_string()),
    ));
    assert!(
        matches!(
            &fault,
            PluginError::StoreUnavailable {
                fault: StoreFault::StorageFailure { backend, .. }
            } if backend == "sqlite"
        ),
        "{fault:?}"
    );
    assert!(fault.is_retryable() && !fault.is_terminal(), "{fault:?}");
}

/// A session's delete and fork run on the store's own writer connection, so
/// they keep the store set's connection hooks: with inline calls each one
/// answers at its first poll, before its caller's runtime runs anything
/// else, and a simulation's clock never moves while one is pending
/// (FIG-5284).
#[tokio::test]
async fn a_session_delete_and_fork_answer_inline_on_the_store_connection() {
    let stores = SqliteStoreSet::memory_with_options_and_clock(
        SqliteStoreSetOptions {
            inline_calls: true,
            ..SqliteStoreSetOptions::memory()
        },
        Arc::new(lash_core_ids::clock::SystemClock),
    )
    .await
    .expect("open an inline store set");
    let store = stores.session_store_factory();
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());

    let never_made = SessionId::from("never-made");
    let mut deleted = std::pin::pin!(store.delete_session(&never_made));
    assert!(
        deleted.as_mut().poll(&mut context).is_ready(),
        "a session delete answers at its first poll"
    );
    let request = lash_core_store::session_store_factory_types::ForkSessionRequest {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("fork"),
        source_session_id: SessionId::from("never-made-either"),
        head_revision: 0,
        relation: lash_core_store::session_identity::SessionRelation::Root,
        config: lash_core_store::session_graph::PersistedSessionConfig::new(
            lash_protocol_types::session_model::TurnBudget::Unbounded,
            lash_protocol_types::session_model::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ),
        retention: lash_core_store::store::Retention::UntilGc,
    };
    let mut forked = std::pin::pin!(store.fork_session(&request));
    assert!(
        forked.as_mut().poll(&mut context).is_ready(),
        "a session fork answers at its first poll"
    );
}

/// A `COMMIT` SQLite refuses, after every statement of the transaction
/// succeeded, is the write's storage failure, and the transaction leaves
/// nothing behind: a reopened catalog has never held the session, and the
/// same admission then creates it. SQLite's commit hook turns the store's
/// own `COMMIT` into a rollback, so only the commit fails.
#[tokio::test]
async fn a_commit_sqlite_refuses_is_a_storage_failure_that_leaves_nothing_behind() {
    let dir = tempfile::tempdir().expect("commit-fault tempdir");
    let path = dir.path().join("lash.db");
    let store = SqliteStore::open(&path, crate::SqliteSynchronous::Normal)
        .await
        .expect("open catalog");
    let request = SessionStoreCreateRequest {
        session_id: SessionId::fixture("commit-refused"),
        relation: lash_core_store::session_identity::SessionRelation::Root,
        config: lash_core_store::session_graph::PersistedSessionConfig::new(
            lash_protocol_types::session_model::TurnBudget::Unbounded,
            lash_protocol_types::session_model::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ),
        head: lash_core_store::session_store_factory_types::SessionCreationHead::Config,
        pending_observer_intents: Vec::new(),
        owning_process_id: None,
        retention: lash_core_store::store::Retention::UntilGc,
    };
    let refuse_commits = |refuse: bool| {
        store.conn.call(move |connection| {
            connection.commit_hook(refuse.then_some(|| true));
            Ok(())
        })
    };

    refuse_commits(true).await.expect("refuse every COMMIT");
    let refused = store.admit_session(&request).await;
    refuse_commits(false).await.expect("accept COMMIT again");
    assert!(
        matches!(
            refused,
            Err(StoreError::StorageFailure {
                backend: "sqlite",
                ..
            })
        ),
        "an admission whose COMMIT failed answers the storage failure: {refused:?}"
    );

    let reopened = SqliteStore::open(&path, crate::SqliteSynchronous::Normal)
        .await
        .expect("reopen catalog");
    assert!(
        matches!(
            reopened
                .lookup_session(&request.session_id)
                .await
                .expect("look the session up"),
            lash_core_store::store::SessionLookup::Absent
        ),
        "the refused transaction left no session behind"
    );
    assert!(
        matches!(
            store.admit_session(&request).await,
            Ok(lash_core_store::store::SessionAdmission::Created)
        ),
        "the same admission creates the session once COMMIT lands"
    );
    assert!(
        matches!(
            reopened
                .lookup_session(&request.session_id)
                .await
                .expect("look the admitted session up"),
            lash_core_store::store::SessionLookup::Live(_)
        ),
        "the committed admission is complete"
    );
}
/// V07: even a corrupt stored row cannot be read as a conflicting status.
#[test]
fn partial_run_terminals_are_corruption_at_decode() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for statements in crate::schema::provisioning_statements() {
        conn.execute_batch(statements).unwrap();
    }
    conn.pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    let session = SessionId::try_from("s".to_owned()).unwrap();
    let run = lash_protocol_types::TurnId::try_from("r".to_owned()).unwrap();
    for bits in 0..8 {
        conn.execute("DELETE FROM session_runs", []).unwrap();
        conn.execute(
            "INSERT INTO session_runs (session_id, run, terminal_kind, terminal_cause_json, terminal_at_ms) VALUES ('s', 'r', ?1, ?2, ?3)",
            rusqlite::params![(bits & 4 != 0).then_some("answered"), (bits & 2 != 0).then_some(r#"{"cause":"commands_applied"}"#), (bits & 1 != 0).then_some(1_i64)],
        ).unwrap();
        let decoded = crate::session_runs::run_terminal_conn(&conn, &session, &run);
        if bits == 0 {
            assert!(decoded.unwrap().is_none());
        } else if bits == 7 {
            assert!(decoded.unwrap().is_some());
        } else {
            assert!(
                matches!(decoded, Err(StoreError::StoredDataCorrupt { .. })),
                "presence bits {bits:03b}: {decoded:?}"
            );
        }
    }
    conn.execute("UPDATE session_runs SET terminal_kind = 'failed'", [])
        .unwrap();
    assert!(matches!(
        crate::session_runs::run_terminal_conn(&conn, &session, &run),
        Err(StoreError::StoredDataCorrupt { .. })
    ));
}

/// The bytes the store holds for every terminal's window: each terminal
/// row's window revision, an 8-byte integer.
async fn terminal_window_bytes(store: &SqliteStore) -> i64 {
    store
        .conn
        .call(|conn| {
            conn.query_row(
                "SELECT coalesce(sum(CASE WHEN terminal_window_revision IS NULL THEN 0 ELSE 8 END), 0)
                 FROM session_runs",
                [],
                |row| row.get(0),
            )
        })
        .await
        .expect("count terminal window bytes")
}

/// FIG-5931, FIG-6606: a terminal records the window it ended on by its
/// revision, so the bytes it writes for its window do not grow with the
/// session's history: every terminal writes the same 8 bytes, while the
/// window each names is the one it ended on through later commits. A host
/// collection under `until_gc` releases every revision but the head, and
/// with them their windows: those reads are refused as released, the
/// head's window reads unchanged. Before FIG-5931 each terminal wrote its
/// whole hydrated window.
#[tokio::test]
async fn a_terminal_writes_its_window_in_constant_bytes_at_every_history_position() {
    let store = crate::test_support::sqlite_memory_store()
        .await
        .expect("open terminal-window store");
    let session = SessionId::fixture("terminal-window-bytes");
    let mut state = durable_state(&store, &session).await;
    let mut written = Vec::new();
    let mut runs = Vec::new();
    for position in 0..24_u64 {
        state.append_active_read_delta(&[lash_protocol_types::session_model::Message {
            id: format!("history-{position}"),
            role: lash_protocol_types::session_model::MessageRole::User,
            origin: None,
            parts: Arc::new(vec![lash_protocol_types::session_model::Part::text(
                format!("history-{position}.p0"),
                format!("{position}:{}", "h".repeat(2_048)),
                None,
            )]),
            reply_marker: None,
        }]);
        let run = lash_protocol_types::TurnId::fixture(format!("terminal-window-{position}"));
        let (commit, appended) = RuntimeCommit::persisted_state_with_operation(
            &mut state,
            lash_core_store::store::OperationId::turn(&session, run.clone(), "commit"),
        )
        .expect("prepare a longer history");
        let receipt = store
            .commit_runtime_state(commit)
            .await
            .expect("commit a longer history");
        state.apply_persisted_commit_result(receipt);
        state.mark_node_ids_persisted(appended);
        let before = terminal_window_bytes(&store).await;
        store
            .end_command_run(&session, &run, position)
            .await
            .expect("end the run");
        written.push(terminal_window_bytes(&store).await - before);
        runs.push(run);
    }
    assert!(
        written.iter().all(|bytes| *bytes == 8),
        "a terminal's window bytes are not its 8-byte revision: {written:?}"
    );
    let window = |store: Arc<SqliteStore>, run: lash_protocol_types::TurnId| {
        let session = session.clone();
        async move {
            store
                .load_session_window(
                    &session,
                    lash_core_store::store::WindowSelector::Terminal(run),
                )
                .await
                .map(|window| {
                    let window = window.expect("a terminal names its window");
                    (window.head_revision, window.window.nodes.len())
                })
        }
    };
    let mut ended = Vec::new();
    for run in &runs {
        ended.push(
            window(Arc::clone(&store), run.clone())
                .await
                .expect("an unreleased window reads"),
        );
    }
    assert!(
        ended.windows(2).all(|pair| pair[0].1 < pair[1].1),
        "each terminal names the window it ended on: {ended:?}"
    );
    store
        .gc_unreachable()
        .await
        .expect("the host collection completes");
    let (last, earlier) = runs.split_last().expect("24 runs");
    assert_eq!(
        window(Arc::clone(&store), last.clone())
            .await
            .expect("the head's window is retained"),
        ended[23],
        "a host collection changed the head's window"
    );
    for (run, (revision, _)) in earlier.iter().zip(&ended) {
        let read = window(Arc::clone(&store), run.clone()).await;
        assert!(
            matches!(
                &read,
                Err(StoreError::RunWindowReleased { run: released, revision: at, .. })
                    if released == run && at == revision
            ),
            "a collected window is refused as released, never another state: {read:?}"
        );
    }
}

/// FIG-5948: unchanged configuration has one content-addressed body even
/// while every published revision remains available for historical forks.
#[tokio::test]
async fn unchanged_config_is_stored_once_across_retained_revisions() {
    let store = crate::test_support::sqlite_memory_store()
        .await
        .expect("store");
    let session = SessionId::from("shared-revision-config");
    let mut state = durable_state(&store, &session).await;
    store.conn.call(|conn| {
        conn.execute_batch("CREATE TRIGGER refuse_config_resubmission BEFORE INSERT ON blobs
            WHEN EXISTS (SELECT 1 FROM session_revisions WHERE config_ref = NEW.hash)
            BEGIN SELECT RAISE(ABORT, 'RECORDED-CONFIG-SHARING: known config body resubmitted'); END")?;
        Ok(())
    }).await.expect("guard already-held config bodies");
    for _ in 0..8 {
        let result = store
            .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
            .await
            .expect("publish unchanged config");
        state.head_revision = result.head_revision;
    }
    let head = store
        .load_session_head_meta(&session)
        .await
        .expect("read")
        .expect("head");
    let mut staging = lash_core_store::checkpoint_graph::GraphStaging::new(
        lash_core_store::checkpoint_graph::EncodeBudget::new(
            lash_core_store::checkpoint_graph::ExplicitLimit::new(u64::MAX),
        ),
    );
    let root = lash_core_store::checkpoint_graph::config::stage(&mut staging, &head.payload())
        .expect("head graph");
    let reference = root.blob_ref();
    let body = staging.get(&root).expect("root").bytes.to_vec();
    let (revisions, bodies) = store
        .conn
        .call(move |conn| {
            Ok((
                conn.query_row(
                    "SELECT count(*) FROM session_revisions WHERE session_id = ?1",
                    [session.as_str()],
                    |row| row.get::<_, i64>(0),
                )?,
                conn.query_row(
                    "SELECT count(*) FROM blobs WHERE hash = ?1 AND content = ?2",
                    params![reference.as_str(), body],
                    |row| row.get::<_, i64>(0),
                )?,
            ))
        })
        .await
        .expect("count durable copies");
    assert_eq!(revisions, 9, "creation and all eight commits stay retained");
    assert_eq!(
        bodies, 1,
        "all retained revisions must name one config body"
    );
}

/// FIG-5948: the same four retained candidates cost one prune statement
/// and bounded SQL work irrespective of the number of historical runs.
#[test]
fn last_turns_pruning_work_and_statement_count_ignore_old_runs() {
    let mut work = Vec::new();
    for size in [32_i64, 512] {
        let conn = Connection::open_in_memory().expect("SQLite");
        conn.execute_batch(crate::schema::SCHEMA).expect("schema");
        conn.execute_batch(crate::schema_fragments::SESSION_RUNS_TABLES)
            .expect("runs schema");
        conn.execute_batch(crate::durable::GRAPH_TABLES)
            .expect("graph schema");
        conn.execute("INSERT INTO session_meta (session_id, relation_kind, retention_kind, retention_last_turns) VALUES ('prune', 'root', 'last_turns', 2)", []).expect("policy");
        for revision in 1..=size {
            conn.execute("INSERT INTO session_runs (session_id, run, terminal_kind, terminal_cause_json, terminal_at_ms, terminal_head_revision) VALUES ('prune', ?1, 'answered', '{\"cause\":\"commands_applied\"}', 0, ?2)", params![format!("run-{revision}"), revision]).expect("old run");
        }
        for revision in size - 3..=size {
            crate::revisions::record_revision_conn(
                &conn,
                &SessionId::from("prune"),
                revision,
                None,
                None,
                "{}",
            )
            .expect("retained candidate");
        }
        conn.execute("INSERT INTO session_head (session_id, head_revision, terminal_ordinal) VALUES ('prune', ?1, ?1)", [size]).expect("head");
        for other in 0..size {
            let unrelated = SessionId::fixture(format!("unrelated-{other}"));
            crate::revisions::record_revision_conn(&conn, &unrelated, 0, None, None, "{}")
                .expect("unrelated revision");
            conn.execute(
                "INSERT INTO session_head (session_id, head_revision) VALUES (?1, 0)",
                [unrelated.as_str()],
            )
            .expect("unrelated head");
        }
        conn.execute(
            "UPDATE session_revisions SET terminal_ordinal = head_revision WHERE session_id = 'prune'",
            [],
        )
        .expect("ordinals");
        let mut statement = conn
            .prepare(
                crate::session_sql::session_sql()
                    .revisions
                    .prune_unretained
                    .sql(),
            )
            .expect("prune statement");
        let removed = statement
            .query_map(params![0, "prune"], |_| Ok(()))
            .expect("prune")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("rows");
        assert_eq!(removed.len(), 2);
        let steps = statement.get_status(rusqlite::StatementStatus::VmStep);
        work.push(steps);
        eprintln!(
            "LAST_TURNS runs={size} unrelated_heads={size} prune_statements=1 vm_steps={steps}"
        );
    }
    assert!(
        work[1] <= work[0] * 2,
        "old runs must not multiply prune work: {work:?}"
    );
}

/// FIG-5948: scalar receipt facts have one authoritative representation;
/// the original answer survives later head publication and revision release.
#[tokio::test]
async fn receipt_replay_uses_scalar_columns_and_preserves_the_original_answer() {
    let store = crate::test_support::sqlite_memory_store()
        .await
        .expect("store");
    let session = SessionId::from("compact-receipt");
    let mut state = durable_state(&store, &session).await;
    let mut commit = RuntimeCommit::persisted_state_for_test(&state);
    commit.turn_commit = lash_core_store::store::RuntimeTurnCommitStamp::new(
        lash_core_store::store::OperationId::new(
            lash_core_execution::ExecutionScope::turn(
                session.clone(),
                lash_protocol_types::TurnId::from("receipt-turn"),
            ),
            "final",
        ),
    );
    commit.outcome = Some(lash_core_store::store::TurnCommitOutcome::Completed);
    commit.failure_evidence = vec![
        lash_core_store::turn_failure_evidence::TurnFailureEvidence {
            partial_output: Some(
                lash_core_store::turn_failure_evidence::TurnFailurePartialOutput::Complete {
                    text: "original partial output".to_owned(),
                },
            ),
            billed_usage: Default::default(),
            refusal: lash_core_store::turn_failure_evidence::ChargeSafetyRefusalEvidence {
                denial_reason:
                    lash_protocol_types::llm::types::ChargeSafetyDenialReason::GuaranteeRequired,
                protocol_position: lash_protocol_types::llm::types::ProtocolPosition::OutputStarted,
                attempt_number: 1,
                attempt_count: 1,
            },
        },
    ];
    let original = store
        .commit_runtime_state(commit.clone())
        .await
        .expect("commit");
    state.head_revision = original.head_revision;
    let mut later = RuntimeCommit::persisted_state_for_test(&state);
    later.checkpoint.components.insert(
        "receipt/later-head".to_owned(),
        HydratedCheckpointComponent::changed(b"a different retained manifest".to_vec()),
    );
    let later = store.commit_runtime_state(later).await.expect("later head");
    assert_ne!(original.checkpoint_ref, later.checkpoint_ref);
    let limit = std::num::NonZeroU32::new(10).expect("page limit");
    let turns_before = store
        .load_committed_turns(&session, None, limit)
        .await
        .expect("turn page");
    let failures_before = store
        .load_failure_evidence_page(&session, None, limit)
        .await
        .expect("failure page");
    assert_eq!(turns_before.turns.len(), 1);
    assert_eq!(failures_before.settlements.len(), 1);
    let now = store.retirement_instant();
    store
        .conn
        .write(move |tx| {
            crate::revisions::release_unretained_conn(tx, true, None, now)
                .map_err(crate::sqlite_conversion_error)?;
            Ok(())
        })
        .await
        .expect("release old revision");
    store
        .gc_unreachable()
        .await
        .expect("collect released checkpoint");
    let original_root = original.checkpoint_ref.clone();
    let original_blobs = store
        .conn
        .call(move |conn| {
            conn.query_row(
                "SELECT count(*) FROM blobs WHERE hash = ?1",
                [original_root.as_str()],
                |row| row.get::<_, i64>(0),
            )
        })
        .await
        .expect("original manifest presence");
    assert_eq!(
        original_blobs, 0,
        "replay cannot reconstruct the original manifest from a retained root"
    );
    let replay = store
        .commit_runtime_state(commit)
        .await
        .expect("replay after prune");
    assert!(replay.receipt_replayed);
    assert_eq!(
        serde_json::to_vec(&original).expect("answer"),
        serde_json::to_vec(&replay).expect("replayed answer")
    );
    let turns_after = store
        .load_committed_turns(&session, None, limit)
        .await
        .expect("turn page after replay");
    let failures_after = store
        .load_failure_evidence_page(&session, None, limit)
        .await
        .expect("failure page after replay");
    let turn_facts = |page: lash_core_store::store::CommittedTurnNodesPage| {
        serde_json::json!({ "next": page.next, "turns": page.turns.into_iter().map(|turn| serde_json::json!({
            "turn": turn.turn_id, "cursor": turn.cursor, "at": turn.committed_at_ms,
            "outcome": turn.outcome, "nodes": turn.nodes,
        })).collect::<Vec<_>>() })
    };
    assert_eq!(turn_facts(turns_before), turn_facts(turns_after));
    assert_eq!(
        serde_json::json!({ "settlements": failures_before.settlements, "next": failures_before.next }),
        serde_json::json!({ "settlements": failures_after.settlements, "next": failures_after.next })
    );
    let bodies = store
        .conn
        .call(|conn| {
            conn.prepare("SELECT result_json FROM runtime_turn_commits")?
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
        .expect("stored receipts");
    for body in bodies {
        let facts: serde_json::Value = serde_json::from_str(&body).expect("receipt JSON");
        assert!(
            facts.get("committed_at_ms").is_none(),
            "commit time belongs to its scalar column"
        );
        assert!(
            facts.get("head_revision").is_none(),
            "revision belongs to its scalar column"
        );
    }
}

/// FIG-6168: deleting a session releases the store's decoded graph and all
/// projections it owns; an outstanding read keeps only its own snapshot alive.
#[tokio::test]
async fn closed_sessions_release_cached_graph_bytes_after_the_last_reader() {
    cache_release_population(true).await;
}

/// FIG-6168: a retained idle session has no decoded cache owner once its
/// last reader drops; reopening it must read the durable bytes again.
#[tokio::test]
async fn idle_sessions_release_cached_graph_bytes_and_reload_on_reopen() {
    cache_release_population(false).await;
}

async fn cache_release_population(delete: bool) {
    use lash_core_store::session_read_view::SessionReadView;
    use lash_core_store::store::SessionStore;
    use lash_core_store::store::WindowSelector;
    use lash_core_store::store::load_session_window_state;
    use lash_protocol_types::session_model::Message;
    use lash_protocol_types::session_model::MessageRole;
    use lash_protocol_types::session_model::Part;
    let mut retained = Vec::new();
    for depth in [8, 64] {
        for count in [1, 5, 20] {
            let store = crate::test_support::sqlite_memory_store()
                .await
                .expect("store");
            let mut records = Vec::new();
            for ordinal in 0..count {
                let session = SessionId::fixture(format!("closed-{depth}-{ordinal}"));
                let mut state = durable_state(&store, &session).await;
                state.ensure_agent_frame_initialized();
                for node in 0..depth {
                    state.session_graph.append_message(Message {
                        id: format!("message-{node}"),
                        role: MessageRole::User,
                        parts: Arc::new(vec![Part::text(
                            format!("part-{node}"),
                            "x".repeat(4096),
                            None,
                        )]),
                        origin: None,
                        reply_marker: None,
                    });
                }
                store
                    .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
                    .await
                    .expect("grow session");
                drop(state);
                let bound = SessionStore::new(store.clone(), session.clone()).expect("binding");
                let loaded = load_session_window_state(&bound, WindowSelector::Current)
                    .await
                    .expect("read")
                    .expect("window");
                let view = SessionReadView::from_persisted_state(&loaded.state);
                let transcript = view.transcript().expect("transcript");
                let projection = view.shared_chronological_projection();
                let render_cache = Arc::downgrade(&loaded.state.read_model().prompt_render_cache);
                let expected = serde_json::to_vec(view.messages()).expect("recorded messages");
                assert_eq!(view.messages().len(), depth);
                let weak: Vec<_> = loaded
                    .state
                    .session_graph
                    .nodes
                    .iter()
                    .skip(1)
                    .map(Arc::downgrade)
                    .collect();
                assert_eq!(weak.len(), depth);
                drop(loaded);
                if delete {
                    store
                        .delete_session(&session)
                        .await
                        .expect("durable storage close");
                }
                assert!(
                    weak.iter().all(|record| record.strong_count() > 0),
                    "a reader still owns its graph after close"
                );
                assert_eq!(view.messages().len(), depth, "the held read stays usable");
                drop(transcript);
                drop(projection);
                drop(view);
                assert_eq!(
                    render_cache.strong_count(),
                    0,
                    "the graph's render cache has no owner"
                );
                records.extend(weak);
                if !delete {
                    let before = store.decoded_graph_node_bodies.load(Ordering::Relaxed);
                    let reopened = load_session_window_state(&bound, WindowSelector::Current)
                        .await
                        .expect("reopen")
                        .expect("retained window");
                    assert_eq!(
                        store.decoded_graph_node_bodies.load(Ordering::Relaxed) - before,
                        (depth + 1) as u64,
                        "reopen decodes the complete cold window"
                    );
                    let view = SessionReadView::from_persisted_state(&reopened.state);
                    assert_eq!(
                        serde_json::to_vec(view.messages()).expect("reopened messages"),
                        expected,
                        "cache release changed no stored message"
                    );
                    records.extend(
                        reopened
                            .state
                            .session_graph
                            .nodes
                            .iter()
                            .skip(1)
                            .map(Arc::downgrade),
                    );
                    drop(view);
                    drop(reopened);
                }
            }
            let bytes = records
                .iter()
                .filter(|record| record.strong_count() > 0)
                .count()
                * 4096;
            eprintln!("cache payload bytes deleted={delete} K={count} N={depth}: {bytes}");
            retained.push((count, depth, bytes));
        }
    }
    assert!(
        retained.iter().all(|(_, _, bytes)| *bytes == 0),
        "closed sessions retain graph payloads after every reader dropped: {retained:?}"
    );
}

/// FIG-6612: a revision release drains at most `MATERIAL_RELEASE_BATCH`
/// settled tool-intent payloads per call, only those whose owning run
/// published a revision its session no longer retains, and every read
/// already answers an undrained one as expired. Pending, retained-run,
/// unfinished-run and uncorrelated payloads stay.
#[test]
fn settled_payload_release_is_bounded_and_takes_only_released_runs_payloads() {
    let conn = Connection::open_in_memory().expect("SQLite");
    for statements in crate::schema::provisioning_statements() {
        conn.execute_batch(statements).expect("schema");
    }
    let batch = i64::from(lash_store_sql::session::revisions::MATERIAL_RELEASE_BATCH);
    for (session, run, revision) in [
        ("payloads", "released", Some(1_i64)),
        ("payloads", "retained", Some(2)),
        ("payloads", "unfinished", None),
        ("other", "released", Some(1)),
    ] {
        match revision {
            Some(revision) => conn.execute(
                "INSERT INTO session_runs (session_id, run, terminal_kind, terminal_cause_json, terminal_at_ms, terminal_head_revision) VALUES (?1, ?2, 'answered', '{\"cause\":\"commands_applied\"}', 0, ?3)",
                params![session, run, revision],
            ),
            None => conn.execute(
                "INSERT INTO session_runs (session_id, run) VALUES (?1, ?2)",
                params![session, run],
            ),
        }
        .expect("run");
    }
    // Revision 1 was released; revision 2 is retained.
    crate::revisions::record_revision_conn(
        &conn,
        &SessionId::from("payloads"),
        2,
        None,
        None,
        "{}",
    )
    .expect("retained revision");
    let insert = |key: &str, session: &str, run: Option<&str>, settled: bool| {
        conn.execute(
            "INSERT INTO tool_intent_submissions (replay_key, owner, execution_scope_id, tool_call_id, intent_index, payload_hash, submission_json, intent_json, settlement_json, owner_session_id, owner_run, admitted_at_ms) VALUES (?1, 'session:' || ?2, 'scope', 'call', 0, 'hash', '{}', '{\"payload\":true}', ?3, ?4, ?5, 0)",
            params![
                key,
                session,
                settled.then_some("{\"settled\":true}"),
                run.map(|_| session),
                run
            ],
        )
        .expect("submission");
    };
    let released = batch + 3;
    for index in 0..released {
        insert(
            &format!("released-{index:04}"),
            "payloads",
            Some("released"),
            true,
        );
    }
    insert("pending", "payloads", Some("released"), false);
    insert("retained", "payloads", Some("retained"), true);
    insert("unfinished", "payloads", Some("unfinished"), true);
    insert("uncorrelated", "payloads", None, true);
    insert("other-session", "other", Some("released"), true);

    let payloads = |pattern: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM tool_intent_submissions WHERE replay_key LIKE ?1 AND intent_json IS NOT NULL",
            [pattern],
            |row| row.get(0),
        )
        .expect("count payloads")
    };
    let read_payload = |key: &str| -> Option<String> {
        conn.query_row(
            crate::turn_ingress::tool_intent_sql()
                .shared
                .select_by_replay_key
                .sql(),
            [key],
            |row| row.get(1),
        )
        .expect("read the row")
    };
    let release = |session: Option<SessionId>| {
        crate::revisions::release_revision_material_conn(
            &conn,
            session.as_ref(),
            lash_durable::DurableInstant(0),
        )
        .expect("release");
    };
    assert_eq!(
        read_payload("released-0000"),
        None,
        "a settled payload of a released run reads expired before it drains"
    );

    release(Some(SessionId::fixture("payloads")));
    assert_eq!(payloads("released-%"), 3, "one release drains one batch");
    let undrained: String = conn
        .query_row(
            "SELECT replay_key FROM tool_intent_submissions WHERE replay_key LIKE 'released-%' AND intent_json IS NOT NULL ORDER BY replay_key LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("an undrained row");
    assert_eq!(
        read_payload(&undrained),
        None,
        "an undrained row reads expired"
    );
    release(Some(SessionId::fixture("payloads")));
    assert_eq!(
        payloads("released-%"),
        0,
        "the next release drains the rest"
    );
    for key in [
        "pending",
        "retained",
        "unfinished",
        "uncorrelated",
        "other-session",
    ] {
        assert_eq!(payloads(key), 1, "`{key}` keeps its payload");
    }
    assert_eq!(
        read_payload("retained").as_deref(),
        Some("{\"payload\":true}"),
        "a retained run's payload reads back"
    );
    release(None);
    assert_eq!(
        payloads("other-session"),
        0,
        "a collection over every session drains another session's released payload"
    );
    for key in ["pending", "retained", "unfinished", "uncorrelated"] {
        assert_eq!(payloads(key), 1, "`{key}` still keeps its payload");
    }
}

async fn measured_configuration_head(
    store: &SqliteStore,
    session: &SessionId,
) -> (SessionHeadMeta, u64) {
    let window =
        lash_core_execution::facade_support::sql::Window::new("configuration-reuse-law", "sqlite");
    let head = window
        .within(store.load_session_head_meta(session))
        .await
        .expect("metadata read")
        .expect("head");
    let reads = window
        .snapshot()
        .shapes
        .iter()
        .filter(|(shape, _)| {
            shape.starts_with("select stored.hash,") && shape.contains("blobs as stored")
        })
        .map(|(_, shape)| shape.statements)
        .sum();
    (head, reads)
}

/// IMMUTABLE-CONFIG-REUSE: the root, rather than a session or head revision,
/// identifies reusable configuration. Every metadata read still observes the
/// head another handle published, including a head with an unchanged root.
#[tokio::test]
async fn validated_configuration_is_reused_only_for_its_immutable_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config-reuse.db");
    let store = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("reader");
    let writer = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("independent writer");
    let session = SessionId::from("config-reuse");
    let mut state = durable_state(&store, &session).await;
    let (first, cold_reads) = measured_configuration_head(&store, &session).await;
    assert!(cold_reads > 0, "cold root is validated");
    let (reused, reused_reads) = measured_configuration_head(&store, &session).await;
    assert_eq!(reused.config, first.config);
    assert_eq!(
        reused_reads, 0,
        "a validated immutable root needs no body SELECTs"
    );

    state.policy.max_tool_calls = lash_core_execution::MaxToolCalls::new(7);
    state.config_revision = 1;
    let receipt = writer
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
        .await
        .expect("publish changed config");
    let (changed, changed_reads) = measured_configuration_head(&store, &session).await;
    assert_eq!(changed.head_revision, receipt.head_revision);
    assert_eq!(changed.config.config_revision, 1);
    assert_eq!(
        changed.config.max_tool_calls,
        lash_core_execution::MaxToolCalls::new(7)
    );
    assert!(changed_reads > 0, "new root is validated");

    state.head_revision = receipt.head_revision;
    let receipt = writer
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
        .await
        .expect("publish same config on a new head");
    let (advanced, advanced_reads) = measured_configuration_head(&store, &session).await;
    assert_eq!(advanced.head_revision, receipt.head_revision);
    assert_eq!(advanced.config, changed.config);
    assert_eq!(
        advanced_reads, 0,
        "new head with the same root reuses validated config"
    );
}

/// CONFIG-CORRUPTION-REFUSAL: cached payloads still prove their session
/// identity against fresh rows; a cold reader validates the graph material.
#[tokio::test]
async fn configuration_reuse_preserves_identity_and_cold_corruption_refusals() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config-corruption.db");
    let store = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("store");
    let owner = SessionId::from("config-owner");
    let foreign = SessionId::from("config-foreign");
    durable_state(&store, &owner).await;
    durable_state(&store, &foreign).await;
    store
        .load_session_head_meta(&owner)
        .await
        .expect("validate and cache owner")
        .expect("head");
    let raw = Connection::open(&path).expect("corruption connection");
    let root: String = raw
        .query_row(
            "SELECT config_ref FROM session_revisions WHERE session_id = ?1",
            [owner.as_str()],
            |row| row.get(0),
        )
        .expect("owner root");
    raw.execute(
        "UPDATE session_revisions SET config_ref = ?1 WHERE session_id = ?2",
        params![root, foreign.as_str()],
    )
    .expect("mis-key the cached root");
    let error = store
        .load_session_head_meta(&foreign)
        .await
        .expect_err("cached config cannot change the row identity");
    assert!(
        matches!(error, StoreError::StoredDataCorrupt { record_kind: "SessionHeadMeta", message } if message.contains("row is keyed on session `config-foreign`"))
    );
    raw.execute(
        "UPDATE blobs SET content = CAST('{' AS BLOB) WHERE hash = ?1",
        [root],
    )
    .expect("corrupt configuration graph body");
    let cold = SqliteStore::open_file_for_testing(&path)
        .await
        .expect("cold reader");
    let error = cold
        .load_session_head_meta(&owner)
        .await
        .expect_err("cold root must validate its material");
    assert!(matches!(
        error,
        StoreError::StoredDataCorrupt {
            record_kind: "SessionHeadMeta",
            ..
        }
    ));
}

#[path = "tests/configuration.rs"]
mod configuration;

#[path = "tests/prepared.rs"]
mod prepared;
