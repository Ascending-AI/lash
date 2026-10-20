use super::*;

use crate::tests::host::turn::*;

/// FIG-5263: every call of all three steps survives an owner loss between
/// steps, and a cold follower reads the same records without live activity.
#[tokio::test]
async fn three_step_tool_calls_survive_owner_loss_and_cold_reattachment() {
    let (_, capture) = lash_core::testing::trace_capture::capturing(|| async {
        let mut uninterrupted = None;
        for crash in [false, true] {
            let stores = sqlite_memory_store_set().await;
            let entered = Arc::new(tokio::sync::Notify::new());
            let calls = Arc::new(AtomicUsize::new(0));
            let provider = crate::testing::TestProvider::builder()
                .kind("three-steps")
                .complete({
                    let entered = Arc::clone(&entered);
                    let calls = Arc::clone(&calls);
                    move |request: LlmRequest| {
                        let entered = Arc::clone(&entered);
                        let calls = Arc::clone(&calls);
                        async move {
                            let results = request
                                .messages
                                .iter()
                                .flat_map(|message| message.blocks.iter())
                                .filter(|block| matches!(block, LlmContentBlock::ToolResult { .. }))
                                .count();
                            if crash && results == 2 && calls.fetch_add(1, Ordering::SeqCst) == 0 {
                                entered.notify_one();
                                std::future::pending::<()>().await;
                            }
                            if results == 6 {
                                return Ok(text_response("all six calls finished"));
                            }
                            Ok(LlmResponse {
                                parts: (results..results + 2)
                                    .map(|index| LlmOutputPart::ToolCall {
                                        call_id: format!("call-{index}"),
                                        tool_name: lash_core::testing::FIXTURE_ECHO_TOOL
                                            .replace('.', "__"),
                                        input_json:
                                            serde_json::json!({"value": format!("value-{index}")})
                                                .to_string(),
                                        replay: None,
                                    })
                                    .collect(),
                                ..LlmResponse::default()
                            })
                        }
                    }
                })
                .build()
                .into_handle();
            let build = |owner: &str| {
                explicit_ephemeral_facets(LashCore::standard_builder(
                    lash_conformance::backend_over(stores.clone()),
                ))
                .serve_test_llm_profile(provider.clone(), mock_llm_profile_spec())
                .tools(
                    crate::tool_provider_key!("tools"),
                    Arc::new(lash_core::testing::FixtureTools),
                )
                .build(lash_core::LeaseOwnerIdentity::opaque(
                    lash_core::LeaseOwnerId::new(owner),
                    lash_core::LeaseIncarnationId::new("boot"),
                ))
                .expect("core")
            };
            let old = build("three-steps-old");
            let id =
                lash_protocol_types::SessionId::try_from("three-steps".to_owned()).expect("id");
            let session = old
                .session(id.clone())
                .create(crate::SessionCreation::root(mock_session_spec()))
                .await
                .expect("created");
            let input = crate::TurnId::parse("three-step-input").expect("input id");
            let handle = session
                .send(crate::TurnInput::text("call six tools"))
                .id(input.clone());
            let output = tokio::spawn(handle.output());
            let new = if crash {
                tokio::time::timeout(std::time::Duration::from_secs(60), entered.notified())
                    .await
                    .expect("second step started");
                // Stop drops the active turn in the second model call. The next
                // node restores the checkpoint after the first round committed.
                old.node.stop().await;
                Some(build("three-steps-new"))
            } else {
                None
            };
            let output = tokio::time::timeout(std::time::Duration::from_secs(60), output)
                .await
                .expect("turn finishes")
                .expect("output task")
                .expect("turn answers");
            assert_eq!(output.assistant_message(), Some("all six calls finished"));
            let output_tool_calls =
                crate::testing::tool_calls_for_activity(&session, &output.activities).await;
            let records = &output_tool_calls;
            assert_eq!(records.len(), 6, "all calls, crash={crash}: {records:?}");
            for (index, record) in records.iter().enumerate() {
                assert_eq!(
                    record.provider_call_id.as_deref(),
                    Some(format!("call-{index}").as_str())
                );
                assert_eq!(
                    record.args,
                    serde_json::json!({"value": format!("value-{index}")})
                );
                assert!(record.output.is_success());
            }
            if let Some(expected) = &uninterrupted {
                assert_eq!(
                    records, expected,
                    "a resumed turn reports the uninterrupted records"
                );
            } else {
                uninterrupted = Some(records.clone());
            }
            let core = new.as_ref().unwrap_or(&old);
            let durable = core.session(id).durable().await.expect("durable session");
            durable
                .attach_id(input.clone())
                .output()
                .await
                .expect("cold answer");
            assert_eq!(
                durable
                    .read_turn_tool_calls(&input)
                    .await
                    .expect("read cold ledger"),
                *records,
                "no live activity is needed"
            );
            if let Some(new) = new {
                new.shutdown().await.expect("shutdown new");
            }
            old.shutdown().await.expect("shutdown old");
        }
    })
    .await;
    let warnings: Vec<_> = capture
        .events
        .lock_recover()
        .iter()
        .filter(|event| {
            event.level == "WARN"
                && event.target == "lash_core::runtime::observation"
                && event.contains_field("message")
                && event
                    .field("message")
                    .contains("failed to capture plugin query services")
        })
        .cloned()
        .collect();
    assert!(
        warnings.is_empty(),
        "normal durable turns warned: {warnings:?}"
    );
}

/// FIG-5264: an operation's atomic batch withdrawal uses the same receipt,
/// without inventing input identity or cancellation evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_queued_operation_answers_withdrawn_and_repeats_are_unknown() {
    let accounts = AccountCore::new().await;
    let session_id = crate::SessionId::from("facade-operation-withdraw");
    let session = accounts
        .core
        .session(session_id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let held = session
        .send(crate::TurnInput::text("hold"))
        .await
        .expect("accepted");
    accounts
        .entered
        .acquire()
        .await
        .expect("model entered")
        .forget();
    let parts = session.send_parts().await.expect("session parts");
    let batch = parts
        .store
        .enqueue_queued_work(
            crate::persistence::QueuedWorkBatchDraft::new(
                session_id.clone(),
                lash_core::DeliveryPolicy::AfterCurrentTurnCommit,
                crate::persistence::QueuedWorkPayload::SessionCommand {
                    command: Box::new(lash_core::facade_support::SessionCommand::RunPluginTask {
                        name: "never invoked".to_owned(),
                        args: serde_json::json!({}),
                    }),
                },
            )
            .with_source_key("command:facade-operation-withdraw"),
        )
        .await
        .expect("queued task");
    let operation = lash_core::tool_run::OperationRun {
        session_id,
        operation_id: batch.batch_id.to_string(),
    };
    let handle = session.run(crate::RunId::parse(operation.run_id().as_str()).unwrap());
    let receipt = handle.cancel().await.expect("withdraw task");
    assert!(
        matches!(receipt, crate::CancelReceipt::Withdrawn { ref run, input: None } if run.as_str() == operation.run_id().as_str()),
        "{receipt:?}"
    );
    assert!(
        !session
            .queued_work()
            .await
            .expect("queue")
            .iter()
            .any(|open| open.batch_id == batch.batch_id)
    );
    assert!(matches!(
        handle.cancel().await.expect("repeat"),
        crate::CancelReceipt::UnknownOrRevoked
    ));
    let missing = lash_core::tool_run::OperationRun {
        session_id: session.session_id().clone(),
        operation_id: "missing-operation".to_owned(),
    };
    assert!(matches!(
        session
            .run(crate::RunId::parse(missing.run_id().as_str()).unwrap())
            .cancel()
            .await
            .expect("missing task"),
        crate::CancelReceipt::UnknownOrRevoked
    ));
    accounts.release.add_permits(1);
    held.output().await.expect("held turn answers");
    accounts.core.shutdown().await.expect("shutdown");
}

/// FIG-5264: a terminal operation cannot be withdrawn or acquire a new cancel.
#[tokio::test]
async fn cancelling_a_settled_operation_answers_unknown_or_revoked() {
    use lash_core::durable_port::domain::{DomainWrite, SessionMailWrite, TurnWrite};
    use lash_core::durable_port::{ActorKey, CommitLabel, NodeId, NodeSpec};
    let backend = sqlite_memory_store_backend().await;
    let core = standard_core_over(backend.clone());
    let session_id = crate::SessionId::from("facade-operation-settled");
    let session = core
        .session(session_id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    // Seed terminal evidence without a live owner competing with the fixture.
    core.shutdown().await.expect("shutdown");
    let parts = session.send_parts().await.expect("session parts");
    let batch = parts
        .store
        .enqueue_queued_work(
            crate::persistence::QueuedWorkBatchDraft::new(
                session_id.clone(),
                lash_core::DeliveryPolicy::AfterCurrentTurnCommit,
                crate::persistence::QueuedWorkPayload::SessionCommand {
                    command: Box::new(lash_core::facade_support::SessionCommand::RunPluginTask {
                        name: "settled task".to_owned(),
                        args: serde_json::json!({}),
                    }),
                },
            )
            .with_source_key("command:facade-operation-settled"),
        )
        .await
        .expect("queued task");
    let operation = lash_core::tool_run::OperationRun {
        session_id: session_id.clone(),
        operation_id: batch.batch_id.to_string(),
    };
    let database = backend.durable();
    let actor = ActorKey::session(session_id.as_str()).expect("actor");
    let snapshot = database
        .actor(&actor)
        .await
        .expect("actor read")
        .expect("woken actor");
    let lease = database
        .register_node(&NodeSpec {
            node: NodeId::new("settled-operation-fixture"),
            decodes: vec![snapshot.formats],
            serves: Vec::new(),
            ttl_millis: 15_000,
        })
        .await
        .expect("fixture lease");
    let claimed = database.claim(&lease, 1).await.expect("claim");
    assert_eq!(claimed.len(), 1);
    let mut tx = database
        .begin(&actor, claimed[0].epoch)
        .await
        .expect("transaction");
    tx.write(DomainWrite::SessionMail(SessionMailWrite::Admit {
        session: session_id.clone(),
        run: operation.run_id(),
        inputs: Vec::new(),
        batches: vec![batch.batch_id.clone()],
    }));
    tx.write(DomainWrite::Turn(TurnWrite::Admit {
        session: session_id.clone(),
        run: operation.run_id(),
        admission: lash_core::store::RunAdmissionRecord::Operation {
            batch: batch.batch_id,
        },
        turn_deadline: None,
    }));
    database
        .commit(tx, CommitLabel::TURN_ADMIT)
        .await
        .expect("admitted");
    let mut tx = database
        .begin(&actor, claimed[0].epoch)
        .await
        .expect("terminal transaction");
    tx.write(DomainWrite::Turn(TurnWrite::Terminal {
        session: session_id,
        run: operation.run_id(),
        cause: Box::new(lash_core::store::RunTerminalCause::Refused {
            refusal: lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::SessionCommandRun,
                "operation refused before execution",
            )
            .into(),
        }),
        head_revision: None,
        transcript: None,
    }));
    database
        .commit(tx, CommitLabel::TURN_COMMIT)
        .await
        .expect("terminal");
    assert!(matches!(
        session
            .run(crate::RunId::parse(operation.run_id().as_str()).unwrap())
            .cancel()
            .await
            .expect("settled task cancel"),
        crate::CancelReceipt::UnknownOrRevoked
    ));
}
