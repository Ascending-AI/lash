use super::*;
use sqlx::Row as _;

#[derive(Clone, Debug, PartialEq, Eq)]
struct RawSessionMetaRow {
    session_id: String,
    relation_kind: String,
    observer_intent_depth: i64,
    parent_session_id: Option<String>,
    caused_by_kind: Option<String>,
    caused_by_session_id: Option<String>,
    caused_by_turn_id: Option<String>,
    caused_by_effect_id: Option<String>,
    caused_by_call_id: Option<String>,
    caused_by_process_id: Option<String>,
    caused_by_process_event_sequence: Option<String>,
    caused_by_occurrence_id: Option<String>,
    caused_by_subscription_id: Option<String>,
    caused_by_subscription_incarnation: Option<String>,
    caused_by_subscription_revision: Option<String>,
    caused_by_node_id: Option<String>,
    source_session_id: Option<String>,
    source_node_id: Option<String>,
    observer_inheritance_kind: Option<String>,
}

impl RawSessionMetaRow {
    fn literal(session_id: &str, relation_kind: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            relation_kind: relation_kind.to_string(),
            observer_intent_depth: 0,
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
            observer_inheritance_kind: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RawObserverIntentProcessRow {
    session_id: String,
    layer_index: i64,
    process_index: i64,
    process_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RawProcessRow {
    session_id: String,
    process_index: i64,
    process_id: String,
}

#[derive(Clone, Debug)]
struct SessionMetaLayoutCase {
    meta: SessionMeta,
    row: RawSessionMetaRow,
    observer_intent_processes: Vec<RawObserverIntentProcessRow>,
    fork_pending_processes: Vec<RawProcessRow>,
    fork_inheritance_processes: Vec<RawProcessRow>,
}

fn session_meta_layout_cases() -> Vec<SessionMetaLayoutCase> {
    use lash_core::{CausalRef, ObserverInheritance};

    let child = |session_id: &str, caused_by| SessionMeta {
        session_id: session_id.to_string(),
        relation: SessionRelation::Child {
            parent_session_id: "layout-parent-literal".to_string(),
            caused_by,
        },
    };
    vec![
        SessionMetaLayoutCase {
            meta: SessionMeta {
                session_id: "layout-root-literal".to_string(),
                relation: SessionRelation::Root,
            },
            row: RawSessionMetaRow::literal("layout-root-literal", "root"),
            observer_intent_processes: vec![],
            fork_pending_processes: vec![],
            fork_inheritance_processes: vec![],
        },
        SessionMetaLayoutCase {
            meta: child("layout-child-none-literal", None),
            row: RawSessionMetaRow {
                parent_session_id: Some("layout-parent-literal".to_string()),
                ..RawSessionMetaRow::literal("layout-child-none-literal", "child")
            },
            observer_intent_processes: vec![],
            fork_pending_processes: vec![],
            fork_inheritance_processes: vec![],
        },
        SessionMetaLayoutCase {
            meta: child(
                "layout-child-turn-literal",
                Some(CausalRef::Turn {
                    session_id: "layout-cause-session-literal".to_string(),
                    turn_id: "layout-cause-turn-literal".to_string(),
                }),
            ),
            row: RawSessionMetaRow {
                parent_session_id: Some("layout-parent-literal".to_string()),
                caused_by_kind: Some("turn".to_string()),
                caused_by_session_id: Some("layout-cause-session-literal".to_string()),
                caused_by_turn_id: Some("layout-cause-turn-literal".to_string()),
                ..RawSessionMetaRow::literal("layout-child-turn-literal", "child")
            },
            observer_intent_processes: vec![],
            fork_pending_processes: vec![],
            fork_inheritance_processes: vec![],
        },
        SessionMetaLayoutCase {
            meta: child(
                "layout-child-effect-no-turn-literal",
                Some(CausalRef::Effect {
                    session_id: "layout-effect-session-literal".to_string(),
                    turn_id: None,
                    effect_id: "layout-effect-id-literal".to_string(),
                }),
            ),
            row: RawSessionMetaRow {
                parent_session_id: Some("layout-parent-literal".to_string()),
                caused_by_kind: Some("effect".to_string()),
                caused_by_session_id: Some("layout-effect-session-literal".to_string()),
                caused_by_effect_id: Some("layout-effect-id-literal".to_string()),
                ..RawSessionMetaRow::literal("layout-child-effect-no-turn-literal", "child")
            },
            observer_intent_processes: vec![],
            fork_pending_processes: vec![],
            fork_inheritance_processes: vec![],
        },
        SessionMetaLayoutCase {
            meta: child(
                "layout-child-effect-with-turn-literal",
                Some(CausalRef::Effect {
                    session_id: "layout-effect-session-literal".to_string(),
                    turn_id: Some("layout-effect-turn-literal".to_string()),
                    effect_id: "layout-effect-id-literal".to_string(),
                }),
            ),
            row: RawSessionMetaRow {
                parent_session_id: Some("layout-parent-literal".to_string()),
                caused_by_kind: Some("effect".to_string()),
                caused_by_session_id: Some("layout-effect-session-literal".to_string()),
                caused_by_turn_id: Some("layout-effect-turn-literal".to_string()),
                caused_by_effect_id: Some("layout-effect-id-literal".to_string()),
                ..RawSessionMetaRow::literal("layout-child-effect-with-turn-literal", "child")
            },
            observer_intent_processes: vec![],
            fork_pending_processes: vec![],
            fork_inheritance_processes: vec![],
        },
        SessionMetaLayoutCase {
            meta: child(
                "layout-child-tool-call-literal",
                Some(CausalRef::ToolCall {
                    session_id: "layout-tool-session-literal".to_string(),
                    call_id: "layout-tool-call-literal".to_string(),
                }),
            ),
            row: RawSessionMetaRow {
                parent_session_id: Some("layout-parent-literal".to_string()),
                caused_by_kind: Some("tool_call".to_string()),
                caused_by_session_id: Some("layout-tool-session-literal".to_string()),
                caused_by_call_id: Some("layout-tool-call-literal".to_string()),
                ..RawSessionMetaRow::literal("layout-child-tool-call-literal", "child")
            },
            observer_intent_processes: vec![],
            fork_pending_processes: vec![],
            fork_inheritance_processes: vec![],
        },
        SessionMetaLayoutCase {
            meta: child(
                "layout-child-process-literal",
                Some(CausalRef::Process {
                    process_id: "layout-cause-process-literal".to_string(),
                }),
            ),
            row: RawSessionMetaRow {
                parent_session_id: Some("layout-parent-literal".to_string()),
                caused_by_kind: Some("process".to_string()),
                caused_by_process_id: Some("layout-cause-process-literal".to_string()),
                ..RawSessionMetaRow::literal("layout-child-process-literal", "child")
            },
            observer_intent_processes: vec![],
            fork_pending_processes: vec![],
            fork_inheritance_processes: vec![],
        },
        SessionMetaLayoutCase {
            meta: child(
                "layout-child-process-event-literal",
                Some(CausalRef::ProcessEvent {
                    process_id: "layout-event-process-literal".to_string(),
                    sequence: u64::MAX,
                }),
            ),
            row: RawSessionMetaRow {
                parent_session_id: Some("layout-parent-literal".to_string()),
                caused_by_kind: Some("process_event".to_string()),
                caused_by_process_id: Some("layout-event-process-literal".to_string()),
                caused_by_process_event_sequence: Some("18446744073709551615".to_string()),
                ..RawSessionMetaRow::literal("layout-child-process-event-literal", "child")
            },
            observer_intent_processes: vec![],
            fork_pending_processes: vec![],
            fork_inheritance_processes: vec![],
        },
        SessionMetaLayoutCase {
            meta: child(
                "layout-child-trigger-minimal-literal",
                Some(CausalRef::TriggerOccurrence {
                    occurrence_id: "layout-occurrence-minimal-literal".to_string(),
                    subscription_id: None,
                    subscription_incarnation: None,
                    subscription_revision: None,
                }),
            ),
            row: RawSessionMetaRow {
                parent_session_id: Some("layout-parent-literal".to_string()),
                caused_by_kind: Some("trigger_occurrence".to_string()),
                caused_by_occurrence_id: Some("layout-occurrence-minimal-literal".to_string()),
                ..RawSessionMetaRow::literal("layout-child-trigger-minimal-literal", "child")
            },
            observer_intent_processes: vec![],
            fork_pending_processes: vec![],
            fork_inheritance_processes: vec![],
        },
        SessionMetaLayoutCase {
            meta: child(
                "layout-child-trigger-complete-literal",
                Some(CausalRef::TriggerOccurrence {
                    occurrence_id: "layout-occurrence-complete-literal".to_string(),
                    subscription_id: Some("layout-subscription-literal".to_string()),
                    subscription_incarnation: Some("layout-incarnation-literal".to_string()),
                    subscription_revision: Some(u64::MAX),
                }),
            ),
            row: RawSessionMetaRow {
                parent_session_id: Some("layout-parent-literal".to_string()),
                caused_by_kind: Some("trigger_occurrence".to_string()),
                caused_by_occurrence_id: Some("layout-occurrence-complete-literal".to_string()),
                caused_by_subscription_id: Some("layout-subscription-literal".to_string()),
                caused_by_subscription_incarnation: Some("layout-incarnation-literal".to_string()),
                caused_by_subscription_revision: Some("18446744073709551615".to_string()),
                ..RawSessionMetaRow::literal("layout-child-trigger-complete-literal", "child")
            },
            observer_intent_processes: vec![],
            fork_pending_processes: vec![],
            fork_inheritance_processes: vec![],
        },
        SessionMetaLayoutCase {
            meta: child(
                "layout-child-session-node-literal",
                Some(CausalRef::SessionNode {
                    session_id: "layout-node-session-literal".to_string(),
                    node_id: "layout-cause-node-literal".to_string(),
                }),
            ),
            row: RawSessionMetaRow {
                parent_session_id: Some("layout-parent-literal".to_string()),
                caused_by_kind: Some("session_node".to_string()),
                caused_by_session_id: Some("layout-node-session-literal".to_string()),
                caused_by_node_id: Some("layout-cause-node-literal".to_string()),
                ..RawSessionMetaRow::literal("layout-child-session-node-literal", "child")
            },
            observer_intent_processes: vec![],
            fork_pending_processes: vec![],
            fork_inheritance_processes: vec![],
        },
        SessionMetaLayoutCase {
            meta: SessionMeta {
                session_id: "layout-fork-all-literal".to_string(),
                relation: SessionRelation::Fork {
                    source_session_id: "layout-source-all-literal".to_string(),
                    source_node_id: "layout-source-node-all-literal".to_string(),
                    observer_inheritance: ObserverInheritance::All,
                    pending_observer_process_ids: vec![],
                },
            },
            row: RawSessionMetaRow {
                source_session_id: Some("layout-source-all-literal".to_string()),
                source_node_id: Some("layout-source-node-all-literal".to_string()),
                observer_inheritance_kind: Some("all".to_string()),
                ..RawSessionMetaRow::literal("layout-fork-all-literal", "fork")
            },
            observer_intent_processes: vec![],
            fork_pending_processes: vec![],
            fork_inheritance_processes: vec![],
        },
        SessionMetaLayoutCase {
            meta: SessionMeta {
                session_id: "layout-fork-none-literal".to_string(),
                relation: SessionRelation::Fork {
                    source_session_id: "layout-source-none-literal".to_string(),
                    source_node_id: "layout-source-node-none-literal".to_string(),
                    observer_inheritance: ObserverInheritance::None,
                    pending_observer_process_ids: vec![
                        "layout-pending-none-a-literal".to_string(),
                        "layout-pending-none-b-literal".to_string(),
                    ],
                },
            },
            row: RawSessionMetaRow {
                source_session_id: Some("layout-source-none-literal".to_string()),
                source_node_id: Some("layout-source-node-none-literal".to_string()),
                observer_inheritance_kind: Some("none".to_string()),
                ..RawSessionMetaRow::literal("layout-fork-none-literal", "fork")
            },
            observer_intent_processes: vec![],
            fork_pending_processes: vec![
                RawProcessRow {
                    session_id: "layout-fork-none-literal".to_string(),
                    process_index: 0,
                    process_id: "layout-pending-none-a-literal".to_string(),
                },
                RawProcessRow {
                    session_id: "layout-fork-none-literal".to_string(),
                    process_index: 1,
                    process_id: "layout-pending-none-b-literal".to_string(),
                },
            ],
            fork_inheritance_processes: vec![],
        },
        SessionMetaLayoutCase {
            meta: SessionMeta {
                session_id: "layout-fork-only-empty-literal".to_string(),
                relation: SessionRelation::Fork {
                    source_session_id: "layout-source-only-empty-literal".to_string(),
                    source_node_id: "layout-source-node-only-empty-literal".to_string(),
                    observer_inheritance: ObserverInheritance::Only(vec![]),
                    pending_observer_process_ids: vec![],
                },
            },
            row: RawSessionMetaRow {
                source_session_id: Some("layout-source-only-empty-literal".to_string()),
                source_node_id: Some("layout-source-node-only-empty-literal".to_string()),
                observer_inheritance_kind: Some("only".to_string()),
                ..RawSessionMetaRow::literal("layout-fork-only-empty-literal", "fork")
            },
            observer_intent_processes: vec![],
            fork_pending_processes: vec![],
            fork_inheritance_processes: vec![],
        },
        SessionMetaLayoutCase {
            meta: SessionMeta {
                session_id: "layout-fork-only-processes-literal".to_string(),
                relation: SessionRelation::Fork {
                    source_session_id: "layout-source-only-literal".to_string(),
                    source_node_id: "layout-source-node-only-literal".to_string(),
                    observer_inheritance: ObserverInheritance::Only(vec![
                        "layout-inherit-a-literal".to_string(),
                        "layout-inherit-b-literal".to_string(),
                    ]),
                    pending_observer_process_ids: vec!["layout-pending-only-literal".to_string()],
                },
            },
            row: RawSessionMetaRow {
                source_session_id: Some("layout-source-only-literal".to_string()),
                source_node_id: Some("layout-source-node-only-literal".to_string()),
                observer_inheritance_kind: Some("only".to_string()),
                ..RawSessionMetaRow::literal("layout-fork-only-processes-literal", "fork")
            },
            observer_intent_processes: vec![],
            fork_pending_processes: vec![RawProcessRow {
                session_id: "layout-fork-only-processes-literal".to_string(),
                process_index: 0,
                process_id: "layout-pending-only-literal".to_string(),
            }],
            fork_inheritance_processes: vec![
                RawProcessRow {
                    session_id: "layout-fork-only-processes-literal".to_string(),
                    process_index: 0,
                    process_id: "layout-inherit-a-literal".to_string(),
                },
                RawProcessRow {
                    session_id: "layout-fork-only-processes-literal".to_string(),
                    process_index: 1,
                    process_id: "layout-inherit-b-literal".to_string(),
                },
            ],
        },
        SessionMetaLayoutCase {
            meta: SessionMeta {
                session_id: "layout-observer-root-literal".to_string(),
                relation: SessionRelation::ObserverIntent {
                    relation: Box::new(SessionRelation::Root),
                    pending_observer_process_ids: vec![
                        "layout-observer-root-a-literal".to_string(),
                    ],
                },
            },
            row: RawSessionMetaRow {
                observer_intent_depth: 1,
                ..RawSessionMetaRow::literal("layout-observer-root-literal", "root")
            },
            observer_intent_processes: vec![RawObserverIntentProcessRow {
                session_id: "layout-observer-root-literal".to_string(),
                layer_index: 0,
                process_index: 0,
                process_id: "layout-observer-root-a-literal".to_string(),
            }],
            fork_pending_processes: vec![],
            fork_inheritance_processes: vec![],
        },
        SessionMetaLayoutCase {
            meta: SessionMeta {
                session_id: "layout-observer-empty-child-literal".to_string(),
                relation: SessionRelation::ObserverIntent {
                    relation: Box::new(SessionRelation::Child {
                        parent_session_id: "layout-observer-parent-literal".to_string(),
                        caused_by: Some(CausalRef::Process {
                            process_id: "layout-observer-cause-literal".to_string(),
                        }),
                    }),
                    pending_observer_process_ids: vec![],
                },
            },
            row: RawSessionMetaRow {
                observer_intent_depth: 1,
                parent_session_id: Some("layout-observer-parent-literal".to_string()),
                caused_by_kind: Some("process".to_string()),
                caused_by_process_id: Some("layout-observer-cause-literal".to_string()),
                ..RawSessionMetaRow::literal("layout-observer-empty-child-literal", "child")
            },
            observer_intent_processes: vec![],
            fork_pending_processes: vec![],
            fork_inheritance_processes: vec![],
        },
        SessionMetaLayoutCase {
            meta: SessionMeta {
                session_id: "layout-observer-nested-fork-literal".to_string(),
                relation: SessionRelation::ObserverIntent {
                    relation: Box::new(SessionRelation::ObserverIntent {
                        relation: Box::new(SessionRelation::Fork {
                            source_session_id: "layout-nested-source-literal".to_string(),
                            source_node_id: "layout-nested-node-literal".to_string(),
                            observer_inheritance: ObserverInheritance::Only(vec![
                                "layout-nested-inherit-literal".to_string(),
                            ]),
                            pending_observer_process_ids: vec![
                                "layout-nested-fork-pending-literal".to_string(),
                            ],
                        }),
                        pending_observer_process_ids: vec![
                            "layout-nested-inner-a-literal".to_string(),
                            "layout-nested-inner-b-literal".to_string(),
                        ],
                    }),
                    pending_observer_process_ids: vec![],
                },
            },
            row: RawSessionMetaRow {
                observer_intent_depth: 2,
                source_session_id: Some("layout-nested-source-literal".to_string()),
                source_node_id: Some("layout-nested-node-literal".to_string()),
                observer_inheritance_kind: Some("only".to_string()),
                ..RawSessionMetaRow::literal("layout-observer-nested-fork-literal", "fork")
            },
            observer_intent_processes: vec![
                RawObserverIntentProcessRow {
                    session_id: "layout-observer-nested-fork-literal".to_string(),
                    layer_index: 1,
                    process_index: 0,
                    process_id: "layout-nested-inner-a-literal".to_string(),
                },
                RawObserverIntentProcessRow {
                    session_id: "layout-observer-nested-fork-literal".to_string(),
                    layer_index: 1,
                    process_index: 1,
                    process_id: "layout-nested-inner-b-literal".to_string(),
                },
            ],
            fork_pending_processes: vec![RawProcessRow {
                session_id: "layout-observer-nested-fork-literal".to_string(),
                process_index: 0,
                process_id: "layout-nested-fork-pending-literal".to_string(),
            }],
            fork_inheritance_processes: vec![RawProcessRow {
                session_id: "layout-observer-nested-fork-literal".to_string(),
                process_index: 0,
                process_id: "layout-nested-inherit-literal".to_string(),
            }],
        },
    ]
}

const RAW_SESSION_META_SELECT: &str = "session_id, relation_kind, observer_intent_depth,
    parent_session_id, caused_by_kind, caused_by_session_id, caused_by_turn_id,
    caused_by_effect_id, caused_by_call_id, caused_by_process_id,
    caused_by_process_event_sequence, caused_by_occurrence_id,
    caused_by_subscription_id, caused_by_subscription_incarnation,
    caused_by_subscription_revision, caused_by_node_id, source_session_id,
    source_node_id, observer_inheritance_kind";

fn sqlite_raw_session_meta_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawSessionMetaRow> {
    Ok(RawSessionMetaRow {
        session_id: row.get(0)?,
        relation_kind: row.get(1)?,
        observer_intent_depth: row.get(2)?,
        parent_session_id: row.get(3)?,
        caused_by_kind: row.get(4)?,
        caused_by_session_id: row.get(5)?,
        caused_by_turn_id: row.get(6)?,
        caused_by_effect_id: row.get(7)?,
        caused_by_call_id: row.get(8)?,
        caused_by_process_id: row.get(9)?,
        caused_by_process_event_sequence: row.get(10)?,
        caused_by_occurrence_id: row.get(11)?,
        caused_by_subscription_id: row.get(12)?,
        caused_by_subscription_incarnation: row.get(13)?,
        caused_by_subscription_revision: row.get(14)?,
        caused_by_node_id: row.get(15)?,
        source_session_id: row.get(16)?,
        source_node_id: row.get(17)?,
        observer_inheritance_kind: row.get(18)?,
    })
}

fn postgres_raw_session_meta_row(row: sqlx::postgres::PgRow) -> RawSessionMetaRow {
    RawSessionMetaRow {
        session_id: row.get(0),
        relation_kind: row.get(1),
        observer_intent_depth: row.get(2),
        parent_session_id: row.get(3),
        caused_by_kind: row.get(4),
        caused_by_session_id: row.get(5),
        caused_by_turn_id: row.get(6),
        caused_by_effect_id: row.get(7),
        caused_by_call_id: row.get(8),
        caused_by_process_id: row.get(9),
        caused_by_process_event_sequence: row.get(10),
        caused_by_occurrence_id: row.get(11),
        caused_by_subscription_id: row.get(12),
        caused_by_subscription_incarnation: row.get(13),
        caused_by_subscription_revision: row.get(14),
        caused_by_node_id: row.get(15),
        source_session_id: row.get(16),
        source_node_id: row.get(17),
        observer_inheritance_kind: row.get(18),
    }
}

fn assert_sqlite_raw_session_meta_layout(path: &Path, cases: &[SessionMetaLayoutCase]) {
    let connection = rusqlite::Connection::open(path).expect("open SQLite metadata layout reader");
    for case in cases {
        let row = connection
            .query_row(
                &format!(
                    "SELECT {RAW_SESSION_META_SELECT} FROM session_meta WHERE session_id = ?1"
                ),
                [&case.row.session_id],
                sqlite_raw_session_meta_row,
            )
            .expect("read literal SQLite session metadata columns");
        assert_eq!(
            row, case.row,
            "SQLite production write must use the literal relational layout for {}",
            case.row.session_id
        );

        let observer_intent_processes = {
            let mut statement = connection
                .prepare(
                    "SELECT session_id, layer_index, process_index, process_id
                     FROM session_meta_observer_intent_processes
                     WHERE session_id = ?1 ORDER BY layer_index, process_index",
                )
                .expect("prepare SQLite observer-intent layout read");
            statement
                .query_map([&case.row.session_id], |row| {
                    Ok(RawObserverIntentProcessRow {
                        session_id: row.get(0)?,
                        layer_index: row.get(1)?,
                        process_index: row.get(2)?,
                        process_id: row.get(3)?,
                    })
                })
                .expect("read SQLite observer-intent layout")
                .collect::<Result<Vec<_>, _>>()
                .expect("decode SQLite observer-intent layout")
        };
        assert_eq!(
            observer_intent_processes, case.observer_intent_processes,
            "SQLite production write must preserve literal observer-intent rows for {}",
            case.row.session_id
        );

        let fork_pending_processes = {
            let mut statement = connection
                .prepare(
                    "SELECT session_id, process_index, process_id
                     FROM session_meta_fork_pending_observer_processes
                     WHERE session_id = ?1 ORDER BY process_index",
                )
                .expect("prepare SQLite fork-pending layout read");
            statement
                .query_map([&case.row.session_id], |row| {
                    Ok(RawProcessRow {
                        session_id: row.get(0)?,
                        process_index: row.get(1)?,
                        process_id: row.get(2)?,
                    })
                })
                .expect("read SQLite fork-pending layout")
                .collect::<Result<Vec<_>, _>>()
                .expect("decode SQLite fork-pending layout")
        };
        assert_eq!(
            fork_pending_processes, case.fork_pending_processes,
            "SQLite production write must preserve literal fork-pending rows for {}",
            case.row.session_id
        );

        let fork_inheritance_processes = {
            let mut statement = connection
                .prepare(
                    "SELECT session_id, process_index, process_id
                     FROM session_meta_fork_inheritance_processes
                     WHERE session_id = ?1 ORDER BY process_index",
                )
                .expect("prepare SQLite fork-inheritance layout read");
            statement
                .query_map([&case.row.session_id], |row| {
                    Ok(RawProcessRow {
                        session_id: row.get(0)?,
                        process_index: row.get(1)?,
                        process_id: row.get(2)?,
                    })
                })
                .expect("read SQLite fork-inheritance layout")
                .collect::<Result<Vec<_>, _>>()
                .expect("decode SQLite fork-inheritance layout")
        };
        assert_eq!(
            fork_inheritance_processes, case.fork_inheritance_processes,
            "SQLite production write must preserve literal fork-inheritance rows for {}",
            case.row.session_id
        );
    }
}

async fn assert_postgres_raw_session_meta_layout(pool: &PgPool, cases: &[SessionMetaLayoutCase]) {
    for case in cases {
        let row = sqlx::query(&format!(
            "SELECT {RAW_SESSION_META_SELECT} FROM lash_session_meta WHERE session_id = $1"
        ))
        .bind(&case.row.session_id)
        .fetch_one(pool)
        .await
        .map(postgres_raw_session_meta_row)
        .expect("read literal PostgreSQL session metadata columns");
        assert_eq!(
            row, case.row,
            "PostgreSQL production write must use the literal relational layout for {}",
            case.row.session_id
        );

        let observer_intent_processes = sqlx::query_as::<_, (String, i64, i64, String)>(
            "SELECT session_id, layer_index, process_index, process_id
             FROM lash_session_meta_observer_intent_processes
             WHERE session_id = $1 ORDER BY layer_index, process_index",
        )
        .bind(&case.row.session_id)
        .fetch_all(pool)
        .await
        .expect("read PostgreSQL observer-intent layout")
        .into_iter()
        .map(
            |(session_id, layer_index, process_index, process_id)| RawObserverIntentProcessRow {
                session_id,
                layer_index,
                process_index,
                process_id,
            },
        )
        .collect::<Vec<_>>();
        assert_eq!(
            observer_intent_processes, case.observer_intent_processes,
            "PostgreSQL production write must preserve literal observer-intent rows for {}",
            case.row.session_id
        );

        let fork_pending_processes = sqlx::query_as::<_, (String, i64, String)>(
            "SELECT session_id, process_index, process_id
             FROM lash_session_meta_fork_pending_observer_processes
             WHERE session_id = $1 ORDER BY process_index",
        )
        .bind(&case.row.session_id)
        .fetch_all(pool)
        .await
        .expect("read PostgreSQL fork-pending layout")
        .into_iter()
        .map(|(session_id, process_index, process_id)| RawProcessRow {
            session_id,
            process_index,
            process_id,
        })
        .collect::<Vec<_>>();
        assert_eq!(
            fork_pending_processes, case.fork_pending_processes,
            "PostgreSQL production write must preserve literal fork-pending rows for {}",
            case.row.session_id
        );

        let fork_inheritance_processes = sqlx::query_as::<_, (String, i64, String)>(
            "SELECT session_id, process_index, process_id
             FROM lash_session_meta_fork_inheritance_processes
             WHERE session_id = $1 ORDER BY process_index",
        )
        .bind(&case.row.session_id)
        .fetch_all(pool)
        .await
        .expect("read PostgreSQL fork-inheritance layout")
        .into_iter()
        .map(|(session_id, process_index, process_id)| RawProcessRow {
            session_id,
            process_index,
            process_id,
        })
        .collect::<Vec<_>>();
        assert_eq!(
            fork_inheritance_processes, case.fork_inheritance_processes,
            "PostgreSQL production write must preserve literal fork-inheritance rows for {}",
            case.row.session_id
        );
    }
}

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
                [&case.row.session_id],
            )
            .expect("delete SQLite production metadata row");
        transaction
            .execute(
                "INSERT INTO session_meta
                 (session_id, relation_kind, observer_intent_depth, parent_session_id,
                  caused_by_kind, caused_by_session_id, caused_by_turn_id,
                  caused_by_effect_id, caused_by_call_id, caused_by_process_id,
                  caused_by_process_event_sequence, caused_by_occurrence_id,
                  caused_by_subscription_id, caused_by_subscription_incarnation,
                  caused_by_subscription_revision, caused_by_node_id, source_session_id,
                  source_node_id, observer_inheritance_kind)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                         ?14, ?15, ?16, ?17, ?18, ?19)",
                rusqlite::params![
                    case.row.session_id,
                    case.row.relation_kind,
                    case.row.observer_intent_depth,
                    case.row.parent_session_id,
                    case.row.caused_by_kind,
                    case.row.caused_by_session_id,
                    case.row.caused_by_turn_id,
                    case.row.caused_by_effect_id,
                    case.row.caused_by_call_id,
                    case.row.caused_by_process_id,
                    case.row.caused_by_process_event_sequence,
                    case.row.caused_by_occurrence_id,
                    case.row.caused_by_subscription_id,
                    case.row.caused_by_subscription_incarnation,
                    case.row.caused_by_subscription_revision,
                    case.row.caused_by_node_id,
                    case.row.source_session_id,
                    case.row.source_node_id,
                    case.row.observer_inheritance_kind,
                ],
            )
            .expect("insert literal SQLite metadata row");
        for row in &case.observer_intent_processes {
            transaction
                .execute(
                    "INSERT INTO session_meta_observer_intent_processes
                     (session_id, layer_index, process_index, process_id)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![
                        row.session_id,
                        row.layer_index,
                        row.process_index,
                        row.process_id
                    ],
                )
                .expect("insert literal SQLite observer-intent row");
        }
        for row in &case.fork_pending_processes {
            transaction
                .execute(
                    "INSERT INTO session_meta_fork_pending_observer_processes
                     (session_id, process_index, process_id) VALUES (?1, ?2, ?3)",
                    rusqlite::params![row.session_id, row.process_index, row.process_id],
                )
                .expect("insert literal SQLite fork-pending row");
        }
        for row in &case.fork_inheritance_processes {
            transaction
                .execute(
                    "INSERT INTO session_meta_fork_inheritance_processes
                     (session_id, process_index, process_id) VALUES (?1, ?2, ?3)",
                    rusqlite::params![row.session_id, row.process_index, row.process_id],
                )
                .expect("insert literal SQLite fork-inheritance row");
        }
    }
    transaction
        .commit()
        .expect("commit literal SQLite metadata rows");
}

async fn delete_postgres_session_meta_rows(pool: &PgPool, cases: &[SessionMetaLayoutCase]) {
    for case in cases {
        sqlx::query("DELETE FROM lash_session_meta WHERE session_id = $1")
            .bind(&case.row.session_id)
            .execute(pool)
            .await
            .expect("delete prior PostgreSQL metadata layout row");
    }
}

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
            .bind(&case.row.session_id)
            .execute(&mut *transaction)
            .await
            .expect("delete PostgreSQL production metadata row");
        sqlx::query(
            "INSERT INTO lash_session_meta
             (session_id, relation_kind, observer_intent_depth, parent_session_id,
              caused_by_kind, caused_by_session_id, caused_by_turn_id,
              caused_by_effect_id, caused_by_call_id, caused_by_process_id,
              caused_by_process_event_sequence, caused_by_occurrence_id,
              caused_by_subscription_id, caused_by_subscription_incarnation,
              caused_by_subscription_revision, caused_by_node_id, source_session_id,
              source_node_id, observer_inheritance_kind)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                     $14, $15, $16, $17, $18, $19)",
        )
        .bind(&case.row.session_id)
        .bind(&case.row.relation_kind)
        .bind(case.row.observer_intent_depth)
        .bind(&case.row.parent_session_id)
        .bind(&case.row.caused_by_kind)
        .bind(&case.row.caused_by_session_id)
        .bind(&case.row.caused_by_turn_id)
        .bind(&case.row.caused_by_effect_id)
        .bind(&case.row.caused_by_call_id)
        .bind(&case.row.caused_by_process_id)
        .bind(&case.row.caused_by_process_event_sequence)
        .bind(&case.row.caused_by_occurrence_id)
        .bind(&case.row.caused_by_subscription_id)
        .bind(&case.row.caused_by_subscription_incarnation)
        .bind(&case.row.caused_by_subscription_revision)
        .bind(&case.row.caused_by_node_id)
        .bind(&case.row.source_session_id)
        .bind(&case.row.source_node_id)
        .bind(&case.row.observer_inheritance_kind)
        .execute(&mut *transaction)
        .await
        .expect("insert literal PostgreSQL metadata row");
        for row in &case.observer_intent_processes {
            sqlx::query(
                "INSERT INTO lash_session_meta_observer_intent_processes
                 (session_id, layer_index, process_index, process_id) VALUES ($1, $2, $3, $4)",
            )
            .bind(&row.session_id)
            .bind(row.layer_index)
            .bind(row.process_index)
            .bind(&row.process_id)
            .execute(&mut *transaction)
            .await
            .expect("insert literal PostgreSQL observer-intent row");
        }
        for row in &case.fork_pending_processes {
            sqlx::query(
                "INSERT INTO lash_session_meta_fork_pending_observer_processes
                 (session_id, process_index, process_id) VALUES ($1, $2, $3)",
            )
            .bind(&row.session_id)
            .bind(row.process_index)
            .bind(&row.process_id)
            .execute(&mut *transaction)
            .await
            .expect("insert literal PostgreSQL fork-pending row");
        }
        for row in &case.fork_inheritance_processes {
            sqlx::query(
                "INSERT INTO lash_session_meta_fork_inheritance_processes
                 (session_id, process_index, process_id) VALUES ($1, $2, $3)",
            )
            .bind(&row.session_id)
            .bind(row.process_index)
            .bind(&row.process_id)
            .execute(&mut *transaction)
            .await
            .expect("insert literal PostgreSQL fork-inheritance row");
        }
    }
    transaction
        .commit()
        .await
        .expect("commit literal PostgreSQL metadata rows");
}

pub(super) async fn verify_independent_session_meta_layout(
    sqlite_root: &Path,
    postgres: &PostgresStorage,
) {
    let cases = session_meta_layout_cases();
    let sqlite_factory = lash_sqlite_store::SqliteSessionStoreFactory::new(
        sqlite_root.join("session-meta-relational-contract"),
    );
    let sqlite_path = sqlite_factory.catalog_path();
    let postgres_factory = postgres.session_store_factory();
    delete_postgres_session_meta_rows(postgres.pool(), &cases).await;

    let mut sqlite_stores = Vec::with_capacity(cases.len());
    let mut postgres_stores = Vec::with_capacity(cases.len());
    for case in &cases {
        let request = SessionStoreCreateRequest {
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
