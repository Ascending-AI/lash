use super::*;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use sqlx::Row as _;

#[derive(Clone, Debug, PartialEq, Eq)]
struct RawSessionMetaRow {
    session_id: SessionId,
    relation_kind: String,
    parent_session_id: Option<SessionId>,
    caused_by_kind: Option<String>,
    caused_by_session_id: Option<SessionId>,
    caused_by_turn_id: Option<TurnId>,
    caused_by_effect_id: Option<String>,
    caused_by_call_id: Option<String>,
    caused_by_process_id: Option<ProcessId>,
    caused_by_process_event_sequence: Option<String>,
    caused_by_occurrence_id: Option<String>,
    caused_by_subscription_id: Option<String>,
    caused_by_subscription_incarnation: Option<String>,
    caused_by_subscription_revision: Option<String>,
    caused_by_node_id: Option<String>,
    source_session_id: Option<SessionId>,
    source_node_id: Option<String>,
}

impl RawSessionMetaRow {
    fn literal(session_id: &SessionId, relation_kind: &str) -> Self {
        Self {
            session_id: SessionId::from(session_id.to_string()),
            relation_kind: relation_kind.to_string(),
            parent_session_id: None,
            caused_by_kind: None,
            caused_by_session_id: None,
            caused_by_turn_id: None,
            caused_by_effect_id: None,
            caused_by_call_id: None,
            caused_by_process_id: None,
            caused_by_process_event_sequence: None,
            caused_by_occurrence_id: None,
            caused_by_subscription_id: None,
            caused_by_subscription_incarnation: None,
            caused_by_subscription_revision: None,
            caused_by_node_id: None,
            source_session_id: None,
            source_node_id: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RawObserverIntentProcessRow {
    session_id: SessionId,
    process_index: i64,
    process_id: ProcessId,
}

#[derive(Clone, Debug)]
struct SessionMetaLayoutCase {
    meta: SessionMeta,
    row: RawSessionMetaRow,
    pending_observer_intents: Vec<RawObserverIntentProcessRow>,
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn session_meta_layout_cases() -> Vec<SessionMetaLayoutCase> {
    use lash_core::CausalRef;

    let child = |session_id: &SessionId, caused_by| SessionMeta {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from(session_id.to_string()),
        relation: SessionRelation::Child {
            parent_session_id: SessionId::from("layout-parent-literal"),
            caused_by,
        },
    };
    vec![
        SessionMetaLayoutCase {
            meta: SessionMeta {
                pending_observer_intents: Vec::new(),
                session_id: SessionId::from("layout-root-literal"),
                relation: SessionRelation::Root,
            },
            row: RawSessionMetaRow::literal(&SessionId::from("layout-root-literal"), "root"),
            pending_observer_intents: vec![],
        },
        SessionMetaLayoutCase {
            meta: child(&SessionId::from("layout-child-none-literal"), None),
            row: RawSessionMetaRow {
                parent_session_id: Some(SessionId::from("layout-parent-literal")),
                ..RawSessionMetaRow::literal(&SessionId::from("layout-child-none-literal"), "child")
            },
            pending_observer_intents: vec![],
        },
        SessionMetaLayoutCase {
            meta: child(
                &SessionId::from("layout-child-turn-literal"),
                Some(CausalRef::Turn {
                    session_id: SessionId::from("layout-cause-session-literal"),
                    turn_id: TurnId::from("layout-cause-turn-literal"),
                }),
            ),
            row: RawSessionMetaRow {
                parent_session_id: Some(SessionId::from("layout-parent-literal")),
                caused_by_kind: Some("turn".to_string()),
                caused_by_session_id: Some(SessionId::from("layout-cause-session-literal")),
                caused_by_turn_id: Some(TurnId::from("layout-cause-turn-literal")),
                ..RawSessionMetaRow::literal(&SessionId::from("layout-child-turn-literal"), "child")
            },
            pending_observer_intents: vec![],
        },
        SessionMetaLayoutCase {
            meta: child(
                &SessionId::from("layout-child-effect-no-turn-literal"),
                Some(CausalRef::Effect {
                    address: EffectAddress::new(
                        ExecutionScope::runtime_operation("layout-effect-operation-literal"),
                        "layout-effect-id-literal",
                    )
                    .expect("valid operation effect address"),
                }),
            ),
            row: RawSessionMetaRow {
                parent_session_id: Some(SessionId::from("layout-parent-literal")),
                caused_by_kind: Some("effect_address".to_string()),
                caused_by_effect_id: Some(
                    serde_json::to_string(
                        &EffectAddress::new(
                            ExecutionScope::runtime_operation("layout-effect-operation-literal"),
                            "layout-effect-id-literal",
                        )
                        .expect("valid operation effect address"),
                    )
                    .expect("serialize operation effect address"),
                ),
                ..RawSessionMetaRow::literal(
                    &SessionId::from("layout-child-effect-no-turn-literal"),
                    "child",
                )
            },
            pending_observer_intents: vec![],
        },
        SessionMetaLayoutCase {
            meta: child(
                &SessionId::from("layout-child-effect-with-turn-literal"),
                Some(CausalRef::Effect {
                    address: EffectAddress::new(
                        ExecutionScope::turn(
                            "layout-effect-session-literal",
                            "layout-effect-turn-literal",
                        ),
                        "layout-effect-id-literal",
                    )
                    .expect("valid turn effect address"),
                }),
            ),
            row: RawSessionMetaRow {
                parent_session_id: Some(SessionId::from("layout-parent-literal")),
                caused_by_kind: Some("effect_address".to_string()),
                caused_by_effect_id: Some(
                    serde_json::to_string(
                        &EffectAddress::new(
                            ExecutionScope::turn(
                                "layout-effect-session-literal",
                                "layout-effect-turn-literal",
                            ),
                            "layout-effect-id-literal",
                        )
                        .expect("valid turn effect address"),
                    )
                    .expect("serialize turn effect address"),
                ),
                ..RawSessionMetaRow::literal(
                    &SessionId::from("layout-child-effect-with-turn-literal"),
                    "child",
                )
            },
            pending_observer_intents: vec![],
        },
        SessionMetaLayoutCase {
            meta: child(
                &SessionId::from("layout-child-tool-call-literal"),
                Some(CausalRef::ToolCall {
                    session_id: SessionId::from("layout-tool-session-literal"),
                    call_id: "layout-tool-call-literal".to_string(),
                }),
            ),
            row: RawSessionMetaRow {
                parent_session_id: Some(SessionId::from("layout-parent-literal")),
                caused_by_kind: Some("tool_call".to_string()),
                caused_by_session_id: Some(SessionId::from("layout-tool-session-literal")),
                caused_by_call_id: Some("layout-tool-call-literal".to_string()),
                ..RawSessionMetaRow::literal(
                    &SessionId::from("layout-child-tool-call-literal"),
                    "child",
                )
            },
            pending_observer_intents: vec![],
        },
        SessionMetaLayoutCase {
            meta: child(
                &SessionId::from("layout-child-process-literal"),
                Some(CausalRef::Process {
                    process_id: ProcessId::fixture("layout-cause-process-literal"),
                }),
            ),
            row: RawSessionMetaRow {
                parent_session_id: Some(SessionId::from("layout-parent-literal")),
                caused_by_kind: Some("process".to_string()),
                caused_by_process_id: Some(ProcessId::fixture("layout-cause-process-literal")),
                ..RawSessionMetaRow::literal(
                    &SessionId::from("layout-child-process-literal"),
                    "child",
                )
            },
            pending_observer_intents: vec![],
        },
        SessionMetaLayoutCase {
            meta: child(
                &SessionId::from("layout-child-process-event-literal"),
                Some(CausalRef::ProcessEvent {
                    process_id: ProcessId::fixture("layout-event-process-literal"),
                    sequence: u64::MAX,
                }),
            ),
            row: RawSessionMetaRow {
                parent_session_id: Some(SessionId::from("layout-parent-literal")),
                caused_by_kind: Some("process_event".to_string()),
                caused_by_process_id: Some(ProcessId::fixture("layout-event-process-literal")),
                caused_by_process_event_sequence: Some("18446744073709551615".to_string()),
                ..RawSessionMetaRow::literal(
                    &SessionId::from("layout-child-process-event-literal"),
                    "child",
                )
            },
            pending_observer_intents: vec![],
        },
        SessionMetaLayoutCase {
            meta: child(
                &SessionId::from("layout-child-trigger-minimal-literal"),
                Some(CausalRef::TriggerOccurrence {
                    occurrence_id: "layout-occurrence-minimal-literal".to_string(),
                    subscription_id: None,
                    subscription_incarnation: None,
                    subscription_revision: None,
                }),
            ),
            row: RawSessionMetaRow {
                parent_session_id: Some(SessionId::from("layout-parent-literal")),
                caused_by_kind: Some("trigger_occurrence".to_string()),
                caused_by_occurrence_id: Some("layout-occurrence-minimal-literal".to_string()),
                ..RawSessionMetaRow::literal(
                    &SessionId::from("layout-child-trigger-minimal-literal"),
                    "child",
                )
            },
            pending_observer_intents: vec![],
        },
        SessionMetaLayoutCase {
            meta: child(
                &SessionId::from("layout-child-trigger-complete-literal"),
                Some(CausalRef::TriggerOccurrence {
                    occurrence_id: "layout-occurrence-complete-literal".to_string(),
                    subscription_id: Some("layout-subscription-literal".to_string()),
                    subscription_incarnation: Some("layout-incarnation-literal".to_string()),
                    subscription_revision: Some(u64::MAX),
                }),
            ),
            row: RawSessionMetaRow {
                parent_session_id: Some(SessionId::from("layout-parent-literal")),
                caused_by_kind: Some("trigger_occurrence".to_string()),
                caused_by_occurrence_id: Some("layout-occurrence-complete-literal".to_string()),
                caused_by_subscription_id: Some("layout-subscription-literal".to_string()),
                caused_by_subscription_incarnation: Some("layout-incarnation-literal".to_string()),
                caused_by_subscription_revision: Some("18446744073709551615".to_string()),
                ..RawSessionMetaRow::literal(
                    &SessionId::from("layout-child-trigger-complete-literal"),
                    "child",
                )
            },
            pending_observer_intents: vec![],
        },
        SessionMetaLayoutCase {
            meta: child(
                &SessionId::from("layout-child-session-node-literal"),
                Some(CausalRef::SessionNode {
                    session_id: SessionId::from("layout-node-session-literal"),
                    node_id: "layout-cause-node-literal".to_string(),
                }),
            ),
            row: RawSessionMetaRow {
                parent_session_id: Some(SessionId::from("layout-parent-literal")),
                caused_by_kind: Some("session_node".to_string()),
                caused_by_session_id: Some(SessionId::from("layout-node-session-literal")),
                caused_by_node_id: Some("layout-cause-node-literal".to_string()),
                ..RawSessionMetaRow::literal(
                    &SessionId::from("layout-child-session-node-literal"),
                    "child",
                )
            },
            pending_observer_intents: vec![],
        },
        SessionMetaLayoutCase {
            meta: SessionMeta {
                pending_observer_intents: Vec::new(),
                session_id: SessionId::from("layout-fork-history-literal"),
                relation: SessionRelation::Fork {
                    source_session_id: SessionId::from("layout-source-history-literal"),
                    source_node_id: "layout-source-node-history-literal".to_string().into(),
                },
            },
            row: RawSessionMetaRow {
                source_session_id: Some(SessionId::from("layout-source-history-literal")),
                source_node_id: Some("layout-source-node-history-literal".to_string()),
                ..RawSessionMetaRow::literal(
                    &SessionId::from("layout-fork-history-literal"),
                    "fork",
                )
            },
            pending_observer_intents: vec![],
        },
        SessionMetaLayoutCase {
            meta: SessionMeta {
                pending_observer_intents: vec![
                    lash_core::facade_support::SessionObserverIntent::host_requested(
                        ProcessId::fixture("layout-pending-selected-literal"),
                    ),
                ],
                session_id: SessionId::from("layout-fork-selected-literal"),
                relation: SessionRelation::Fork {
                    source_session_id: SessionId::from("layout-source-selected-literal"),
                    source_node_id: "layout-source-node-selected-literal".to_string().into(),
                },
            },
            row: RawSessionMetaRow {
                source_session_id: Some(SessionId::from("layout-source-selected-literal")),
                source_node_id: Some("layout-source-node-selected-literal".to_string()),
                ..RawSessionMetaRow::literal(
                    &SessionId::from("layout-fork-selected-literal"),
                    "fork",
                )
            },
            pending_observer_intents: vec![RawObserverIntentProcessRow {
                session_id: SessionId::from("layout-fork-selected-literal"),
                process_index: 0,
                process_id: ProcessId::fixture("layout-pending-selected-literal"),
            }],
        },
        SessionMetaLayoutCase {
            meta: SessionMeta {
                pending_observer_intents: vec![
                    lash_core::facade_support::SessionObserverIntent::host_requested(
                        ProcessId::fixture("layout-observer-root-a-literal"),
                    ),
                    lash_core::facade_support::SessionObserverIntent::host_requested(
                        ProcessId::fixture("layout-observer-root-b-literal"),
                    ),
                ],
                session_id: SessionId::from("layout-observer-root-literal"),
                relation: SessionRelation::Root,
            },
            row: RawSessionMetaRow::literal(
                &SessionId::from("layout-observer-root-literal"),
                "root",
            ),
            pending_observer_intents: vec![
                RawObserverIntentProcessRow {
                    session_id: SessionId::from("layout-observer-root-literal"),
                    process_index: 0,
                    process_id: ProcessId::fixture("layout-observer-root-a-literal"),
                },
                RawObserverIntentProcessRow {
                    session_id: SessionId::from("layout-observer-root-literal"),
                    process_index: 1,
                    process_id: ProcessId::fixture("layout-observer-root-b-literal"),
                },
            ],
        },
    ]
}

const RAW_SESSION_META_SELECT: &str = "session_id, relation_kind, parent_session_id,
    caused_by_kind, caused_by_session_id, caused_by_turn_id,
    caused_by_effect_id, caused_by_call_id, caused_by_process_id,
    caused_by_process_event_sequence, caused_by_occurrence_id,
    caused_by_subscription_id, caused_by_subscription_incarnation,
    caused_by_subscription_revision, caused_by_node_id, source_session_id,
    source_node_id";

fn sqlite_raw_session_meta_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawSessionMetaRow> {
    Ok(RawSessionMetaRow {
        session_id: SessionId::from(row.get::<_, String>(0)?),
        relation_kind: row.get(1)?,
        parent_session_id: row.get::<_, Option<String>>(2)?.map(SessionId::from),
        caused_by_kind: row.get(3)?,
        caused_by_session_id: row.get::<_, Option<String>>(4)?.map(SessionId::from),
        caused_by_turn_id: row.get::<_, Option<String>>(5)?.map(TurnId::from),
        caused_by_effect_id: row.get(6)?,
        caused_by_call_id: row.get(7)?,
        caused_by_process_id: row.get::<_, Option<String>>(8)?.map(stored_process_id),
        caused_by_process_event_sequence: row.get(9)?,
        caused_by_occurrence_id: row.get(10)?,
        caused_by_subscription_id: row.get(11)?,
        caused_by_subscription_incarnation: row.get(12)?,
        caused_by_subscription_revision: row.get(13)?,
        caused_by_node_id: row.get(14)?,
        source_session_id: row.get::<_, Option<String>>(15)?.map(SessionId::from),
        source_node_id: row.get(16)?,
    })
}

fn postgres_raw_session_meta_row(row: sqlx::postgres::PgRow) -> RawSessionMetaRow {
    RawSessionMetaRow {
        session_id: SessionId::from(row.get::<String, _>(0)),
        relation_kind: row.get(1),
        parent_session_id: row.get::<Option<String>, _>(2).map(SessionId::from),
        caused_by_kind: row.get(3),
        caused_by_session_id: row.get::<Option<String>, _>(4).map(SessionId::from),
        caused_by_turn_id: row.get::<Option<String>, _>(5).map(TurnId::from),
        caused_by_effect_id: row.get(6),
        caused_by_call_id: row.get(7),
        caused_by_process_id: row.get::<Option<String>, _>(8).map(stored_process_id),
        caused_by_process_event_sequence: row.get(9),
        caused_by_occurrence_id: row.get(10),
        caused_by_subscription_id: row.get(11),
        caused_by_subscription_incarnation: row.get(12),
        caused_by_subscription_revision: row.get(13),
        caused_by_node_id: row.get(14),
        source_session_id: row.get::<Option<String>, _>(15).map(SessionId::from),
        source_node_id: row.get(16),
    }
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn assert_sqlite_raw_session_meta_layout(path: &Path, cases: &[SessionMetaLayoutCase]) {
    let connection = rusqlite::Connection::open(path).expect("open SQLite metadata layout reader");
    for case in cases {
        let row = connection
            .query_row(
                &format!(
                    "SELECT {RAW_SESSION_META_SELECT} FROM session_meta WHERE session_id = ?1"
                ),
                [case.row.session_id.as_str()],
                sqlite_raw_session_meta_row,
            )
            .expect("read literal SQLite session metadata columns");
        assert_eq!(
            row, case.row,
            "SQLite production write must use the literal relational layout for {}",
            case.row.session_id
        );

        let pending_observer_intents = {
            let mut statement = connection
                .prepare(
                    "SELECT session_id, process_index, process_id
                     FROM session_meta_pending_observer_intents
                     WHERE session_id = ?1 ORDER BY process_index",
                )
                .expect("prepare SQLite observer-intent layout read");
            statement
                .query_map([case.row.session_id.as_str()], |row| {
                    Ok(RawObserverIntentProcessRow {
                        session_id: SessionId::from(row.get::<_, String>(0)?),
                        process_index: row.get(1)?,
                        process_id: stored_process_id(row.get::<_, String>(2)?),
                    })
                })
                .expect("read SQLite observer-intent layout")
                .collect::<Result<Vec<_>, _>>()
                .expect("decode SQLite observer-intent layout")
        };
        assert_eq!(
            pending_observer_intents, case.pending_observer_intents,
            "SQLite production write must preserve literal observer-intent rows for {}",
            case.row.session_id
        );
    }
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
async fn assert_postgres_raw_session_meta_layout(pool: &PgPool, cases: &[SessionMetaLayoutCase]) {
    for case in cases {
        let row = sqlx::query(&format!(
            "SELECT {RAW_SESSION_META_SELECT} FROM lash_session_meta WHERE session_id = $1"
        ))
        .bind(case.row.session_id.as_str())
        .fetch_one(pool)
        .await
        .map(postgres_raw_session_meta_row)
        .expect("read literal PostgreSQL session metadata columns");
        assert_eq!(
            row, case.row,
            "PostgreSQL production write must use the literal relational layout for {}",
            case.row.session_id
        );

        let pending_observer_intents = sqlx::query_as::<_, (String, i64, String)>(
            "SELECT session_id, process_index, process_id
             FROM lash_session_meta_pending_observer_intents
             WHERE session_id = $1 ORDER BY process_index",
        )
        .bind(case.row.session_id.as_str())
        .fetch_all(pool)
        .await
        .expect("read PostgreSQL observer-intent layout")
        .into_iter()
        .map(
            |(session_id, process_index, process_id)| RawObserverIntentProcessRow {
                session_id: SessionId::from(session_id),
                process_index,
                process_id: stored_process_id(process_id),
            },
        )
        .collect::<Vec<_>>();
        assert_eq!(
            pending_observer_intents, case.pending_observer_intents,
            "PostgreSQL production write must preserve literal observer-intent rows for {}",
            case.row.session_id
        );
    }
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn replace_sqlite_session_meta_with_raw_rows(path: &Path, cases: &[SessionMetaLayoutCase]) {
    let mut connection = rusqlite::Connection::open(path).expect("open SQLite metadata raw writer");
    connection
        .execute_batch("PRAGMA foreign_keys=ON;")
        .expect("enable SQLite metadata raw-writer foreign keys");
    let transaction = connection
        .transaction()
        .expect("begin SQLite metadata raw-write transaction");
    for case in cases {
        transaction
            .execute(
                "DELETE FROM session_meta WHERE session_id = ?1",
                [case.row.session_id.as_str()],
            )
            .expect("delete SQLite production metadata row");
        transaction
            .execute(
                "INSERT INTO session_meta
                 (session_id, relation_kind, parent_session_id,
                  caused_by_kind, caused_by_session_id, caused_by_turn_id,
                  caused_by_effect_id, caused_by_call_id, caused_by_process_id,
                  caused_by_process_event_sequence, caused_by_occurrence_id,
                  caused_by_subscription_id, caused_by_subscription_incarnation,
                  caused_by_subscription_revision, caused_by_node_id, source_session_id,
                  source_node_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                         ?14, ?15, ?16, ?17)",
                rusqlite::params![
                    case.row.session_id.as_str(),
                    case.row.relation_kind,
                    case.row.parent_session_id.as_ref().map(SessionId::as_str),
                    case.row.caused_by_kind,
                    case.row
                        .caused_by_session_id
                        .as_ref()
                        .map(SessionId::as_str),
                    case.row.caused_by_turn_id.as_ref().map(TurnId::as_str),
                    case.row.caused_by_effect_id,
                    case.row.caused_by_call_id,
                    case.row
                        .caused_by_process_id
                        .as_ref()
                        .map(ProcessId::as_str),
                    case.row.caused_by_process_event_sequence,
                    case.row.caused_by_occurrence_id,
                    case.row.caused_by_subscription_id,
                    case.row.caused_by_subscription_incarnation,
                    case.row.caused_by_subscription_revision,
                    case.row.caused_by_node_id,
                    case.row.source_session_id.as_ref().map(SessionId::as_str),
                    case.row.source_node_id,
                ],
            )
            .expect("insert literal SQLite metadata row");
        for row in &case.pending_observer_intents {
            transaction
                .execute(
                    "INSERT INTO session_meta_pending_observer_intents
                     (session_id, process_index, process_id)
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![
                        row.session_id.as_str(),
                        row.process_index,
                        row.process_id.as_str(),
                    ],
                )
                .expect("insert literal SQLite observer-intent row");
        }
    }
    transaction
        .commit()
        .expect("commit literal SQLite metadata rows");
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
async fn delete_postgres_session_meta_rows(pool: &PgPool, cases: &[SessionMetaLayoutCase]) {
    for case in cases {
        sqlx::query("DELETE FROM lash_session_meta WHERE session_id = $1")
            .bind(case.row.session_id.as_str())
            .execute(pool)
            .await
            .expect("delete prior PostgreSQL metadata layout row");
    }
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
async fn replace_postgres_session_meta_with_raw_rows(
    pool: &PgPool,
    cases: &[SessionMetaLayoutCase],
) {
    let mut transaction = pool
        .begin()
        .await
        .expect("begin PostgreSQL metadata raw-write transaction");
    for case in cases {
        sqlx::query("DELETE FROM lash_session_meta WHERE session_id = $1")
            .bind(case.row.session_id.as_str())
            .execute(&mut *transaction)
            .await
            .expect("delete PostgreSQL production metadata row");
        sqlx::query(
            "INSERT INTO lash_session_meta
             (session_id, relation_kind, parent_session_id,
              caused_by_kind, caused_by_session_id, caused_by_turn_id,
              caused_by_effect_id, caused_by_call_id, caused_by_process_id,
              caused_by_process_event_sequence, caused_by_occurrence_id,
              caused_by_subscription_id, caused_by_subscription_incarnation,
              caused_by_subscription_revision, caused_by_node_id, source_session_id,
              source_node_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                     $14, $15, $16, $17)",
        )
        .bind(case.row.session_id.as_str())
        .bind(&case.row.relation_kind)
        .bind(case.row.parent_session_id.as_ref().map(SessionId::as_str))
        .bind(&case.row.caused_by_kind)
        .bind(
            case.row
                .caused_by_session_id
                .as_ref()
                .map(SessionId::as_str),
        )
        .bind(case.row.caused_by_turn_id.as_ref().map(TurnId::as_str))
        .bind(&case.row.caused_by_effect_id)
        .bind(&case.row.caused_by_call_id)
        .bind(
            case.row
                .caused_by_process_id
                .as_ref()
                .map(ProcessId::as_str),
        )
        .bind(&case.row.caused_by_process_event_sequence)
        .bind(&case.row.caused_by_occurrence_id)
        .bind(&case.row.caused_by_subscription_id)
        .bind(&case.row.caused_by_subscription_incarnation)
        .bind(&case.row.caused_by_subscription_revision)
        .bind(&case.row.caused_by_node_id)
        .bind(case.row.source_session_id.as_ref().map(SessionId::as_str))
        .bind(&case.row.source_node_id)
        .execute(&mut *transaction)
        .await
        .expect("insert literal PostgreSQL metadata row");
        for row in &case.pending_observer_intents {
            sqlx::query(
                "INSERT INTO lash_session_meta_pending_observer_intents
                 (session_id, process_index, process_id)
                 VALUES ($1, $2, $3)",
            )
            .bind(row.session_id.as_str())
            .bind(row.process_index)
            .bind(row.process_id.as_str())
            .execute(&mut *transaction)
            .await
            .expect("insert literal PostgreSQL observer-intent row");
        }
    }
    transaction
        .commit()
        .await
        .expect("commit literal PostgreSQL metadata rows");
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) async fn verify_independent_session_meta_layout(
    sqlite_root: &Path,
    postgres: &PostgresStorage,
) {
    let cases = session_meta_layout_cases();
    let sqlite_case_root = sqlite_root.join("session-meta-relational-contract");
    let sqlite_factory =
        lash_sqlite_store::SqliteSessionStoreFactory::new(sqlite_case_root.clone());
    let sqlite_path =
        sqlite_case_root.join(lash_sqlite_store::SqliteDatabase::DurableCore.file_name());
    let postgres_factory = postgres.session_store_factory();
    delete_postgres_session_meta_rows(postgres.pool(), &cases).await;

    let mut sqlite_stores = Vec::with_capacity(cases.len());
    let mut postgres_stores = Vec::with_capacity(cases.len());
    for case in &cases {
        let request = SessionStoreCreateRequest {
            pending_observer_intents: case.meta.pending_observer_intents.clone(),
            session_id: case.meta.session_id.clone(),
            relation: case.meta.relation.clone(),
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        };
        sqlite_stores.push(
            sqlite_factory
                .create_store(&request)
                .await
                .expect("write SQLite metadata through production API"),
        );
        postgres_stores.push(
            postgres_factory
                .create_store(&request)
                .await
                .expect("write PostgreSQL metadata through production API"),
        );
    }

    assert_sqlite_raw_session_meta_layout(&sqlite_path, &cases);
    assert_postgres_raw_session_meta_layout(postgres.pool(), &cases).await;

    replace_sqlite_session_meta_with_raw_rows(&sqlite_path, &cases);
    replace_postgres_session_meta_with_raw_rows(postgres.pool(), &cases).await;
    for ((case, sqlite_store), postgres_store) in
        cases.iter().zip(&sqlite_stores).zip(&postgres_stores)
    {
        let sqlite_meta = sqlite_store
            .load_session_meta()
            .await
            .expect("decode SQLite metadata inserted with raw SQL");
        assert_eq!(
            sqlite_meta,
            Some(case.meta.clone()),
            "SQLite production decoder must reconstruct literal metadata for {}",
            case.meta.session_id
        );
        let postgres_meta = postgres_store
            .load_session_meta()
            .await
            .expect("decode PostgreSQL metadata inserted with raw SQL");
        assert_eq!(
            postgres_meta,
            Some(case.meta.clone()),
            "PostgreSQL production decoder must reconstruct literal metadata for {}",
            case.meta.session_id
        );
    }

    delete_postgres_session_meta_rows(postgres.pool(), &cases).await;
}

#[expect(
    clippy::expect_used,
    reason = "test support: a stored process id that does not decode is a layout defect the law must surface"
)]
fn stored_process_id(id: String) -> ProcessId {
    ProcessId::parse(&id).expect("a stored process id is minted")
}
