use super::effect_group_committed_recovery::HarnessStoreTier;
use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};
use super::*;

async fn index_rows(
    harness: &LiveConformanceHarness,
    object_key: &str,
) -> BTreeMap<String, serde_json::Value> {
    #[derive(serde::Deserialize)]
    struct Row {
        key: String,
        value_utf8: String,
    }
    let rows: Vec<Row> = harness
        .admin_client()
        .query_json(&format!(
            "SELECT key, value_utf8 FROM state WHERE service_name = 'LashDurableWaitIndex' \
             AND service_key = {} AND key != '_compat' AND key != 'wait-index/v2/metadata'",
            crate::ingress::sql_string_literal(object_key),
        ))
        .await
        .expect("read the wait index's retained rows");
    rows.into_iter()
        .map(|row| {
            (
                row.key,
                serde_json::from_str(&row.value_utf8).expect("a stamped index row"),
            )
        })
        .collect()
}

async fn indexed_wait_keeps_its_key_through_settlement_and_retirement(
    target: HarnessServer,
    tier: HarnessStoreTier,
) {
    let harness = LiveConformanceHarness::start_for_tool_children_over(target, tier).await;
    let ingress = harness.ingress();
    let identity = uuid::Uuid::new_v4().to_string();
    for transition in ["settle", "resolve", "cancel_all"] {
        let session = SessionId::from(format!("{identity}-{transition}"));
        let scopes = [
            ExecutionScope::turn(session.clone(), "root"),
            ExecutionScope::process(ProcessId::fixture(&format!("{session}-process"))),
            ExecutionScope::session_operation(session.clone(), "operation"),
            ExecutionScope::session_delete(session.clone()),
            ExecutionScope::runtime_operation(format!("{session}-operation")),
        ];
        for (ordinal, scope) in scopes.into_iter().enumerate() {
            let key = restate_await_event_key(
                &scope,
                AwaitEventWaitIdentity::Custom {
                    key: format!("wait-{ordinal}"),
                },
            )
            .expect("derive the scope's wait");
            let address = RestateDurableWaitAddress::for_key(&key);
            let object_key = address.index_key();
            let registered: RestateDurableWaitRegistration = ingress
                .call_lash_object(
                    "LashDurableWaitIndex",
                    &object_key,
                    "register",
                    &RestateDurableWaitIndexRequest { key: key.clone() },
                )
                .await
                .expect("register the wait");
            assert_eq!(registered, RestateDurableWaitRegistration::Registered);
            let rows = index_rows(&harness, &object_key).await;
            assert_eq!(rows.len(), 1, "one row owns an open wait");
            let state_key = format!("wait-index/v2/wait/{}", address.workflow_key);
            assert_eq!(
                rows.get(&state_key).map(|row| &row["body"]),
                Some(&serde_json::json!({"key": key, "terminal": null})),
                "the row owns its key and optional terminal for {scope:?}"
            );
            let terminal = if transition == "cancel_all" {
                Resolution::Cancelled
            } else {
                Resolution::Ok(serde_json::json!({"result": ordinal}))
            };
            match transition {
                "settle" => ingress
                    .call_lash_object::<_, ()>(
                        "LashDurableWaitIndex",
                        &object_key,
                        "settle",
                        &RestateDurableWaitSettleRequest {
                            key: key.clone(),
                            resolution: terminal.clone(),
                        },
                    )
                    .await
                    .expect("settle the wait"),
                "resolve" => {
                    let outcome: crate::durable_wait::RestateDurableWaitResolveResponse = ingress
                        .call_lash_object(
                            "LashDurableWaitIndex",
                            &object_key,
                            "resolve",
                            &RestateDurableWaitResolveRequest {
                                key: key.clone(),
                                resolution: terminal.clone(),
                            },
                        )
                        .await
                        .expect("resolve the wait");
                    assert_eq!(
                        outcome,
                        crate::durable_wait::RestateDurableWaitResolveResponse::Outcome(
                            ResolveOutcome::Accepted
                        )
                    );
                }
                "cancel_all" => ingress
                    .call_lash_object::<_, ()>(
                        "LashDurableWaitIndex",
                        &object_key,
                        "cancel_all",
                        &(),
                    )
                    .await
                    .expect("cancel the wait"),
                _ => unreachable!("the transition table is closed"),
            }
            let rows = index_rows(&harness, &object_key).await;
            assert_eq!(rows.len(), 1, "settlement keeps one sweepable row");
            assert_eq!(
                rows[&state_key]["body"],
                serde_json::json!({"key": key, "terminal": terminal}),
                "settlement retains the preimage for {scope:?}"
            );
            let registered: RestateDurableWaitRegistration = ingress
                .call_lash_object(
                    "LashDurableWaitIndex",
                    &object_key,
                    "register",
                    &RestateDurableWaitIndexRequest { key: key.clone() },
                )
                .await
                .expect("reattach to the settled wait");
            assert_eq!(
                registered,
                RestateDurableWaitRegistration::Resolved(terminal)
            );
            let outstanding: Vec<AwaitEventKey> = ingress
                .call_lash_object("LashDurableWaitIndex", &object_key, "outstanding", &())
                .await
                .expect("list the outstanding waits");
            assert!(outstanding.is_empty(), "a terminal row is not outstanding");
            let retired: bool = ingress
                .call_lash_object(
                    "LashDurableWaitIndex",
                    &object_key,
                    "revoke_all_if_quiescent",
                    &(),
                )
                .await
                .expect("retire the settled scope");
            assert!(
                retired,
                "a terminal row does not block quiescent retirement"
            );
            assert!(index_rows(&harness, &object_key).await.is_empty());
            ingress
                .call_lash_object::<_, ()>(
                    "LashDurableWaitIndex",
                    &object_key,
                    "settle",
                    &RestateDurableWaitSettleRequest {
                        key,
                        resolution: Resolution::Cancelled,
                    },
                )
                .await
                .expect("a late settlement observes the retirement fence");
            assert!(
                index_rows(&harness, &object_key).await.is_empty(),
                "late settlement cannot recreate the retired row"
            );
            // Session scopes share an index. Each iteration reinstates its
            // fence, keeping the next scope's registration independent.
            ingress
                .call_lash_object::<_, ()>("LashDurableWaitIndex", &object_key, "reinstate", &())
                .await
                .expect("reinstate the cleared index");
        }
    }
    harness.finish().await;
}

macro_rules! indexed_wait_law {
    ($name:ident, $tier:ident, $target:expr $(, $ignore:literal)?) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        $(#[ignore = $ignore])?
        async fn $name() {
            indexed_wait_keeps_its_key_through_settlement_and_retirement(
                $target,
                HarnessStoreTier::$tier,
            )
            .await;
        }
    };
}

indexed_wait_law!(sqlite_memory, SqliteMemory, HarnessServer::in_process());
indexed_wait_law!(sqlite_file, SqliteFile, HarnessServer::in_process());
indexed_wait_law!(
    sqlite_replay,
    SqliteMemory,
    HarnessServer::InProcess {
        seed: 0x4674,
        always_replay: true
    }
);
indexed_wait_law!(
    postgres,
    Postgres,
    HarnessServer::in_process(),
    "requires PostgreSQL"
);
indexed_wait_law!(
    postgres_replay,
    Postgres,
    HarnessServer::InProcess {
        seed: 0x4674,
        always_replay: true
    },
    "requires PostgreSQL"
);
indexed_wait_law!(
    live_sqlite,
    SqliteFile,
    HarnessServer::Live,
    "requires live Restate"
);
indexed_wait_law!(
    live_postgres,
    Postgres,
    HarnessServer::Live,
    "requires live Restate and PostgreSQL"
);
