use super::*;
use pretty_assertions::assert_eq;

#[expect(
    clippy::expect_used,
    reason = "conformance fixtures establish each result"
)]
pub async fn later_segment_recovery_refuses_without_terminal_mutation(
    registry: Arc<dyn ProcessRegistry>,
) {
    let id = registry
        .register_process(executed_registration("later-segment"))
        .await
        .expect("register engine process")
        .id;
    registry
        .set_external_ref(
            &id,
            crate::ProcessExternalRef {
                backend: "workflow-engine".into(),
                id: "segment-2".into(),
                metadata: None,
                segment_ordinal: Some(2),
            },
        )
        .await
        .expect("hand over to segment 2");
    let before = serde_json::to_value(registry.get_process(&id).await.expect("read before"))
        .expect("serialize before");
    let events = serde_json::to_value(
        registry
            .full_event_window(&id, 0)
            .await
            .expect("events before"),
    )
    .expect("serialize events");
    for ordinal in [0, 1] {
        let result = registry
            .complete_process_with_prelude(
                &id,
                settled_success(serde_json::json!("stale")),
                vec![call_wait_event(
                    &id,
                    "process.effect_summary",
                    "process.effect_summary",
                    serde_json::json!({"stale":true}),
                )],
                ProcessCompletionAuthority::WorkflowKeyRecovery {
                    workflow_key: "segment-0".into(),
                    segment_ordinal: ordinal,
                },
            )
            .await;
        assert!(
            matches!(result, Err(PluginError::ProcessHandedOver { ref process_id, segment_ordinal: 2 }) if process_id == id),
            "{result:?}"
        );
        assert_eq!(
            serde_json::to_value(registry.get_process(&id).await.expect("read after"))
                .expect("serialize after"),
            before
        );
        assert_eq!(
            serde_json::to_value(
                registry
                    .full_event_window(&id, 0)
                    .await
                    .expect("events after")
            )
            .expect("serialize events"),
            events
        );
    }
    registry
        .complete_process(
            &id,
            settled_success(serde_json::json!("current")),
            ProcessCompletionAuthority::WorkflowKeyRecovery {
                workflow_key: "segment-2".into(),
                segment_ordinal: 2,
            },
        )
        .await
        .expect("current segment may complete");
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixtures establish each result"
)]
pub async fn consumer_hold_prevents_destructive_prune_until_settlement(
    registry: Arc<dyn ProcessRegistry>,
) {
    let hold = crate::ConsumerHold {
        key: "retained-consumer".into(),
        owner: crate::ScopeId::turn(SessionId::from("consumer"), crate::TurnId::from("turn")),
        cancels: false,
    };
    let id = registry
        .register_process(registration("held-terminal").with_consumer_hold(Some(hold.clone())))
        .await
        .expect("register held process")
        .id;
    registry
        .complete_process(
            &id,
            settled_success(serde_json::json!("retained")),
            ProcessCompletionAuthority::workflow_key(&id),
        )
        .await
        .expect("complete held child");
    let events = serde_json::to_value(
        registry
            .full_event_window(&id, 0)
            .await
            .expect("terminal events"),
    )
    .expect("serialize events");
    for _ in 0..2 {
        assert!(
            registry
                .prunable_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
                .await
                .expect("survey held child")
                .is_empty()
        );
        assert_eq!(
            registry
                .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
                .await
                .expect("prune held child")
                .pruned_processes,
            0
        );
        assert!(
            registry
                .get_process(&id)
                .await
                .expect("held row survives")
                .is_some()
        );
        assert_eq!(
            serde_json::to_value(
                registry
                    .full_event_window(&id, 0)
                    .await
                    .expect("held events survive")
            )
            .expect("serialize events"),
            events
        );
    }
    registry
        .release_consumer_hold(&id, "unrelated-hold")
        .await
        .expect("unrelated release");
    assert_eq!(
        registry
            .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
            .await
            .expect("wrong hold cannot release")
            .pruned_processes,
        0
    );
    registry
        .release_consumer_hold(&id, &hold.key)
        .await
        .expect("settle consumer");
    assert_eq!(
        registry
            .prunable_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
            .await
            .expect("released child eligible"),
        vec![id.clone()]
    );
    assert_eq!(
        registry
            .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
            .await
            .expect("prune settled child")
            .pruned_processes,
        1
    );
    assert!(matches!(
        registry.get_process(&id).await,
        Err(PluginError::ProcessNoLongerRetained { .. })
    ));
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixtures establish each result"
)]
pub async fn every_execution_write_refuses_a_superseded_invocation_without_mutation(
    registry: Arc<dyn ProcessRegistry>,
) {
    let watched = lash_core::facade_support::watch_process_registry(registry.clone());
    for registry in [registry, watched.registry().clone()] {
        let id = registry
            .register_process(executed_registration("invocation-write-matrix"))
            .await
            .expect("register engine process")
            .id;
        let old = crate::ProcessExecutionWriteAuthority::invocation(id.clone(), "old-invocation")
            .bind_attempt(1);
        let current =
            crate::ProcessExecutionWriteAuthority::invocation(id.clone(), "current-invocation")
                .bind_attempt(2);
        for authority in [&old, &current] {
            registry
                .record_first_started_with_authority(
                    &id,
                    authority.invocation_started().expect("bound attempt"),
                    authority,
                )
                .await
                .expect("admit successive invocation");
        }
        let before = serde_json::to_value(registry.get_process(&id).await.expect("read before"))
            .expect("serialize row");
        let events = serde_json::to_value(
            registry
                .full_event_window(&id, 0)
                .await
                .expect("read events"),
        )
        .expect("serialize events");
        let wait = WaitState {
            since_ms: 1,
            kind: crate::WaitKind::Call {
                call_id: lash_sansio::ToolCallId::fixture("process-wait-law"),
                tool_id: lash_sansio::ToolId::new("process_wait"),
            },
        };
        let terminal =
            crate::terminal_append_request(&id, &settled_success(serde_json::Value::Null), None);
        let results = [
            registry
                .record_first_started_with_authority(
                    &id,
                    old.invocation_started().expect("old attempt"),
                    &old,
                )
                .await
                .map(|_| ()),
            registry
                .append_event_with_authority(
                    &id,
                    call_wait_event(
                        &id,
                        "process.progress",
                        "process.progress",
                        serde_json::json!({"stale":true}),
                    ),
                    &old,
                )
                .await
                .map(|_| ()),
            registry
                .append_events(&id, vec![terminal], &old)
                .await
                .map(|_| ()),
            registry
                .set_process_wait_with_authority(&id, wait, Vec::new(), &old)
                .await
                .map(|_| ()),
            registry
                .clear_process_wait_with_authority(&id, Vec::new(), &old)
                .await
                .map(|_| ()),
        ];
        for (write, result) in results.into_iter().enumerate() {
            if write == 0 {
                assert!(
                    matches!(result, Err(PluginError::Session(ref message)) if message.contains("execution attempt must be 3, got 1")),
                    "write {write}: {result:?}"
                );
            } else {
                assert!(
                    matches!(result, Err(PluginError::ProcessExecutionSuperseded { ref process_id }) if process_id == id),
                    "write {write}: {result:?}"
                );
            }
        }
        assert_eq!(
            serde_json::to_value(registry.get_process(&id).await.expect("read after"))
                .expect("serialize row"),
            before
        );
        assert_eq!(
            serde_json::to_value(
                registry
                    .full_event_window(&id, 0)
                    .await
                    .expect("read events")
            )
            .expect("serialize events"),
            events
        );
        registry
            .clear_process_wait_with_authority(&id, Vec::new(), &current)
            .await
            .expect("current invocation can write");
    }
}

#[expect(clippy::expect_used, reason = "conformance fixture assertions")]
pub async fn retired_process_shapes_refuse_before_registration_or_effects(
    registry: Arc<dyn ProcessRegistry>,
) {
    let before = registry
        .list_processes(&crate::ProcessListFilter {
            status: crate::ProcessStatusFilter::Any,
            ..Default::default()
        })
        .await
        .expect("registry before invalid requests")
        .len();
    for value in [
        serde_json::json!({"type":"lashlang","module":"old-module","process":"old-process"}),
        serde_json::json!({"type":"external","metadata":{"job":"legacy"}}),
        serde_json::json!({"type":"subagent","task":"legacy"}),
        serde_json::json!({"type":"workflow","workflow_id":"old"}),
    ] {
        let decoded = serde_json::from_value::<crate::ProcessInput>(value.clone());
        assert!(decoded.is_err(), "retired input decoded: {value}");
    }
    assert_eq!(
        registry
            .list_processes(&crate::ProcessListFilter {
                status: crate::ProcessStatusFilter::Any,
                ..Default::default()
            })
            .await
            .expect("invalid decoding never reached registry")
            .len(),
        before
    );
    let current = registry
        .register_process(registration("current-external"))
        .await
        .expect("current input remains admitted");
    assert!(!current.is_terminal());
}

#[expect(clippy::expect_used, reason = "conformance fixture assertions")]
pub async fn scope_replay_cancel_and_trace_ignore_environment_rebinding(
    registry: Arc<dyn ProcessRegistry>,
) {
    let scope = crate::ScopeId::turn(
        SessionId::from("execution-owner"),
        crate::TurnId::from("original-turn"),
    );
    let key = crate::DERIVED_START_KEYS.for_tool_intent(&crate::derive_tool_intent_identity(
        &crate::RuntimeOwner::Session(SessionId::from("execution-owner")),
        "original-turn",
        &crate::ToolCallId::fixture("scope-environment"),
        0,
    ));
    let mut engine = registration("scope-environment");
    engine.input = crate::ProcessInput::Engine {
        kind: "scope-environment".into(),
        payload: serde_json::Value::Null,
    }
    .into();
    let request = crate::started_until_starter(engine, scope.clone())
        .with_start_key(Some(key))
        .with_execution_env_ref(Some(lash_core::testing::process_execution_env_fixture_ref()));
    let first = registry
        .register_process_reporting_outcome(request.clone(), &[])
        .await
        .expect("admit captured environment");
    let events = serde_json::to_value(
        registry
            .full_event_window(&first.record.id, 0)
            .await
            .expect("initial trace"),
    )
    .expect("trace snapshot");
    let mut rebound = request;
    rebound.env_ref = Some(super::super::helpers::process_registry_alternate_environment_ref());
    let replay = registry
        .register_process_reporting_outcome(rebound, &[])
        .await
        .expect("replay retained start");
    assert_eq!(replay.outcome, crate::ProcessRegistrationOutcome::Existing);
    assert_eq!(replay.record.id, first.record.id);
    assert_eq!(replay.record.env_ref, first.record.env_ref);
    assert_eq!(replay.record.lifetime, first.record.lifetime);
    assert_eq!(
        serde_json::to_value(
            registry
                .full_event_window(&first.record.id, 0)
                .await
                .expect("replayed trace")
        )
        .expect("trace snapshot"),
        events
    );
    assert!(
        registry
            .list_processes(&crate::ProcessListFilter {
                until: Some(crate::ScopeId::turn(
                    SessionId::from("environment-new-session"),
                    crate::TurnId::from("original-turn")
                )),
                ..Default::default()
            })
            .await
            .expect("environment is not cancellation scope")
            .is_empty()
    );
    let matches = registry
        .list_processes(&crate::ProcessListFilter {
            until: Some(scope),
            ..Default::default()
        })
        .await
        .expect("recorded scope owns cancellation");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].id, first.record.id);
    registry
        .request_process_cancel(
            &first.record.id,
            crate::CancelOrigin::ParentEnded,
            "execution-owner".into(),
            None,
        )
        .await
        .expect("close recorded scope");
    let cancelled = registry
        .get_process(&first.record.id)
        .await
        .expect("cancel trace")
        .expect("retained process");
    assert_eq!(cancelled.env_ref, first.record.env_ref);
    assert_eq!(
        cancelled
            .cancel_request
            .expect("scope cancellation recorded")
            .origin,
        crate::CancelOrigin::ParentEnded
    );
}
