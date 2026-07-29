use super::*;

struct NoopProcessRunHandle;

#[async_trait]
impl lash_core::ProcessRunHandle for NoopProcessRunHandle {
    async fn claim_and_run_pending(&self) -> std::result::Result<(), lash_core::PluginError> {
        Ok(())
    }
}

struct NonblockingObservationQuery;

impl lash_core::PluginOperation for NonblockingObservationQuery {
    const NAME: &'static str = "test.nonblocking_observation_query";
    const DESCRIPTION: &'static str =
        "Return a static value through observation-backed query dispatch.";
    const SESSION_PARAM: lash_core::SessionParam = lash_core::SessionParam::Optional;

    type Args = serde_json::Value;
    type Output = serde_json::Value;
}

impl lash_core::PluginQuery for NonblockingObservationQuery {}

struct FixedCompactor;

#[async_trait]
impl lash_core::ContextCompactor for FixedCompactor {
    fn id(&self) -> &'static str {
        "test.fixed_compactor"
    }

    async fn compact(
        &self,
        ctx: &lash_core::CompactionContext<'_>,
    ) -> std::result::Result<Option<lash_core::ContextCompaction>, lash_core::ContextError> {
        assert_eq!(
            ctx.instructions.as_deref(),
            Some("focus on durable summary")
        );
        assert!(
            ctx.state
                .messages()
                .iter()
                .any(|message| message.parts[0].content.contains("old durable request"))
        );
        Ok(Some(lash_core::ContextCompaction::new(vec![
            lash_core::SessionAppendNode::message(
                lash_core::PluginMessage::text(
                    lash_core::MessageRole::Assistant,
                    "Compaction summary:\nold durable request summarized",
                )
                .with_origin(lash_core::MessageOrigin::Plugin {
                    plugin_id: "test_compactor".to_string(),
                    transient: false,
                }),
            ),
        ])))
    }
}

#[tokio::test]
async fn session_operations_delegate_to_runtime() -> Result<()> {
    let core = standard_core();
    let session = core.session("session-ops").open().await?;

    session.turn(TurnInput::text("usage")).run().await?;
    let usage = session.usage_report();
    assert_eq!(usage.usage.usage.output_tokens, 2);
    session
        .admin()
        .commands()
        .refresh_tool_catalog("control admin test", "control-admin-refresh")
        .await?;
    session.refresh_background_graph().await?;
    assert!(session.processes().list().await?.is_empty());
    let err = session
        .admin()
        .state()
        .snapshot_execution()
        .await
        .expect_err("standard protocol has no code executor to snapshot");
    assert!(matches!(
        err,
        EmbedError::Session(SessionError::CodeExecutionUnavailable)
    ));
    let err = session
        .admin()
        .state()
        .restore_execution(&[1, 2, 3])
        .await
        .expect_err("standard protocol has no code executor to restore");
    assert!(matches!(
        err,
        EmbedError::Session(SessionError::CodeExecutionUnavailable)
    ));
    Ok(())
}

#[tokio::test]
async fn compact_context_opens_compaction_frame_and_preserves_prior_frame() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .plugin(Arc::new(StaticPluginFactory::new(
            "test-compactor",
            lash_core::PluginSpec::new().with_context_compactor(100, Arc::new(FixedCompactor)),
        )))
        .build()?;
    let session = core.session("compact-context").open().await?;
    session
        .turn(TurnInput::text("old durable request"))
        .run()
        .await?;
    let before = session.admin().state().persist_current().await?;
    let previous_frame_node_id = before.current_frame_node_id.clone();
    let observation_cursor = session.observe().current_observation().cursor;
    assert!(
        before.session_graph.nodes.iter().any(|node| {
            before
                .session_graph
                .nearest_frame_node_id(Some(&node.node_id))
                == previous_frame_node_id.as_deref()
                && node
                    .message()
                    .is_some_and(|message| message.parts[0].content.contains("old durable request"))
        }),
        "initial frame should contain the original request"
    );

    let compacted = session
        .admin()
        .state()
        .compact_context(
            Some("focus on durable summary".to_string()),
            runtime_operation_scope(&core, "compact-context-test"),
        )
        .await?;

    assert!(compacted);
    let read_view = session.read_view();
    assert_eq!(read_view.messages().len(), 1);
    assert_eq!(
        read_view.messages()[0].parts[0].content,
        "Compaction summary:\nold durable request summarized"
    );
    assert!(matches!(
        read_view.messages()[0].origin.as_ref(),
        Some(lash_core::MessageOrigin::Plugin { plugin_id, .. }) if plugin_id == "test_compactor"
    ));
    let SessionResume::Replayed { events } =
        session.observe().resume_from_cursor(&observation_cursor)?
    else {
        panic!("recent cursor should replay compaction observation events");
    };
    assert!(
        events.windows(2).any(|window| matches!(
            (&window[0].payload, &window[1].payload),
            (
                lash_core::SessionObservationEventPayload::AgentFrameSwitched { .. },
                lash_core::SessionObservationEventPayload::Committed { .. }
            )
        )),
        "expected AgentFrameSwitched immediately followed by Committed, got {events:?}"
    );

    let after = session.admin().state().persist_current().await?;
    let current = after
        .agent_frames
        .iter()
        .find(|frame| Some(&frame.frame_node_id) == after.current_frame_node_id.as_ref())
        .expect("current frame");
    assert_eq!(
        current.reason.as_str(),
        lash_core::AgentFrameReason::COMPACTION
    );
    assert_eq!(
        current.previous_frame_node_id.as_deref(),
        previous_frame_node_id.as_deref()
    );
    assert_eq!(
        current.assignment.policy.provider_id,
        before.agent_frames[0].assignment.policy.provider_id
    );
    assert_eq!(
        current.protocol_turn_options.payload,
        before.agent_frames[0].protocol_turn_options.payload
    );
    assert!(
        after.session_graph.nodes.iter().any(|node| {
            after
                .session_graph
                .nearest_frame_node_id(Some(&node.node_id))
                == previous_frame_node_id.as_deref()
                && node
                    .message()
                    .is_some_and(|message| message.parts[0].content.contains("old durable request"))
        }),
        "previous frame content should remain durable after compaction"
    );
    assert!(
        after.session_graph.nodes.iter().any(|node| {
            after
                .session_graph
                .nearest_frame_node_id(Some(&node.node_id))
                == after.current_frame_node_id.as_deref()
                && node.message().is_some_and(|message| {
                    message.parts[0]
                        .content
                        .contains("old durable request summarized")
                })
        }),
        "compaction summary should be scoped to the new frame"
    );
    Ok(())
}

#[tokio::test]
async fn session_commands_enqueue_idempotently_by_source_key() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .disable_queued_work_driver()
        .build()?;
    let session = core.session("command-idempotency").open().await?;

    let first = session
        .commands()
        .refresh_tool_catalog("test refresh", "same-refresh")
        .await?;
    let second = session
        .commands()
        .refresh_tool_catalog("test refresh", "same-refresh")
        .await?;

    assert_eq!(first.batch_id, second.batch_id);
    assert_eq!(
        first.source_key,
        "command:refresh_tool_catalog:same-refresh"
    );
    let queued = session.queued_work().await?;
    assert_eq!(queued.len(), 1);
    assert!(matches!(
        &queued[0].items[0].payload,
        lash_core::runtime::QueuedWorkPayload::SessionCommand { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn queue_enqueue_and_cancel_emit_typed_observation_events() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .disable_queued_work_driver()
        .build()?;
    let session = core.session("queue-observation-events").open().await?;
    let cursor = session.observe().current_observation().cursor;

    let pending = session
        .enqueue(TurnInput::text("queued observation"))
        .id("queue-observation")
        .send()
        .await?;
    let inputs = session.pending_turn_inputs().await?;
    assert_eq!(
        inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![pending.input_id.as_str()]
    );
    let cancelled = session.cancel_pending_turn_input(&pending.input_id).await?;
    assert!(matches!(
        cancelled,
        crate::PendingTurnInputCancelOutcome::Cancelled(_)
    ));

    let SessionResume::Replayed { events } = session.observe().resume_from_cursor(&cursor)? else {
        panic!("recent cursor should replay queue observation events");
    };
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        lash_core::SessionObservationEventPayload::QueueChanged { kind, batch_ids }
            if *kind == lash_core::SessionQueueEventKind::Enqueued
                && batch_ids.as_slice() == std::slice::from_ref(&pending.input_id)
    )));
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        lash_core::SessionObservationEventPayload::QueueChanged { kind, batch_ids }
            if *kind == lash_core::SessionQueueEventKind::Cancelled
                && batch_ids.as_slice() == std::slice::from_ref(&pending.input_id)
    )));
    Ok(())
}

#[tokio::test]
async fn pending_turn_input_facade_cancels_bulk_and_suffix_by_source_key() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .disable_queued_work_driver()
        .build()?;
    let session = core.session("pending-input-facade-cancel").open().await?;
    let cursor = session.observe().current_observation().cursor;

    let first = session
        .enqueue(TurnInput::text("first"))
        .id("edit:1")
        .send()
        .await?;
    let second = session
        .enqueue(TurnInput::text("second"))
        .id("edit:2")
        .send()
        .await?;
    let third = session
        .enqueue(TurnInput::text("third"))
        .id("edit:3")
        .send()
        .await?;

    let bulk = session
        .cancel_pending_turn_inputs([
            lash_core::PendingTurnInputCancelTarget::source_key("host:edit:1"),
            lash_core::PendingTurnInputCancelTarget::source_key("host:missing"),
        ])
        .await?;
    assert_eq!(bulk.len(), 2);
    assert!(matches!(
        &bulk[0].outcome,
        crate::PendingTurnInputCancelOutcome::Cancelled(input) if input.input_id == first.input_id
    ));
    assert!(matches!(
        bulk[1].outcome,
        crate::PendingTurnInputCancelOutcome::NotFound
    ));

    let suffix = session
        .cancel_pending_turn_input_suffix(lash_core::PendingTurnInputCancelTarget::source_key(
            "host:edit:2",
        ))
        .await?;
    let lash_core::PendingTurnInputSuffixCancelOutcome::Outcomes { outcomes, .. } = suffix else {
        panic!("source-key suffix anchor should exist");
    };
    assert_eq!(outcomes.len(), 2);
    assert!(matches!(
        &outcomes[0],
        crate::PendingTurnInputCancelOutcome::Cancelled(input) if input.input_id == second.input_id
    ));
    assert!(matches!(
        &outcomes[1],
        crate::PendingTurnInputCancelOutcome::Cancelled(input) if input.input_id == third.input_id
    ));
    assert!(session.pending_turn_inputs().await?.is_empty());

    let SessionResume::Replayed { events } = session.observe().resume_from_cursor(&cursor)? else {
        panic!("recent cursor should replay queue observation events");
    };
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        lash_core::SessionObservationEventPayload::QueueChanged { kind, batch_ids }
            if *kind == lash_core::SessionQueueEventKind::Cancelled
                && batch_ids.as_slice() == std::slice::from_ref(&first.input_id)
    )));
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        lash_core::SessionObservationEventPayload::QueueChanged { kind, batch_ids }
            if *kind == lash_core::SessionQueueEventKind::Cancelled
                && batch_ids == &vec![second.input_id.clone(), third.input_id.clone()]
    )));
    Ok(())
}

#[tokio::test]
async fn process_start_and_cancel_emit_typed_observation_events() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build()?;
    let session = core.session("process-observation-events").open().await?;
    let cursor = session.observe().current_observation().cursor;
    let process_id = "observed-process";

    session
        .processes()
        .start(
            lash_core::ProcessStartRequest::external(
                process_id,
                lash_core::ProcessOriginator::host(),
                serde_json::Value::Null,
            )
            .with_observers(["process-observation-events".to_string()]),
            inline_scope(lash_core::ExecutionScope::process(process_id)),
        )
        .await?;
    session
        .processes()
        .cancel(
            process_id,
            inline_scope(lash_core::ExecutionScope::process(process_id)),
        )
        .await?;

    let SessionResume::Replayed { events } = session.observe().resume_from_cursor(&cursor)? else {
        panic!("recent cursor should replay process observation events");
    };
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        lash_core::SessionObservationEventPayload::ProcessChanged { kind, process_ids }
            if *kind == SessionProcessEventKind::Started
                && process_ids.len() == 1
                && process_ids[0] == process_id
    )));
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        lash_core::SessionObservationEventPayload::ProcessChanged { kind, process_ids }
            if *kind == SessionProcessEventKind::Cancelled
                && process_ids.len() == 1
                && process_ids[0] == process_id
    )));
    Ok(())
}

#[tokio::test]
async fn trigger_emit_does_not_append_session_node_or_queue_work() -> Result<()> {
    let trigger = lash_core::TriggerEvent::new(
        "Button",
        "ui.button",
        "pressed",
        lash_core::LashSchema::any(),
    );
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .plugin(Arc::new(StaticPluginFactory::new(
            "button-triggers",
            lash_core::PluginSpec::new().with_trigger_event(trigger),
        )))
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .build()?;
    let session = core.session("command-trigger").open().await?;
    let before = session.admin().state().persist_current().await?;

    let source_key = lash_core::empty_trigger_source_key("ui.button.pressed")?;
    let scoped_effect_controller = lash_core::ScopedEffectController::shared(
        Arc::new(lash_core::InlineRuntimeEffectController::default()),
        lash_core::ExecutionScope::runtime_operation("trigger:button-press-1"),
    )?;
    let report = core
        .triggers()
        .emit(
            lash_core::TriggerOccurrenceRequest::new(
                "ui.button.pressed",
                source_key,
                serde_json::json!({ "pressed": true }),
                "button-press-1",
            )
            .with_source(serde_json::json!({})),
            scoped_effect_controller,
        )
        .await?;
    assert!(!report.occurrence_id.is_empty());
    assert!(report.deliveries.is_empty());

    assert!(session.queued_work().await?.is_empty());
    let persisted = session.admin().state().persist_current().await?;
    assert_eq!(
        persisted.session_graph.leaf_node_id,
        before.session_graph.leaf_node_id
    );
    let trigger_nodes = persisted
        .session_graph
        .nodes
        .iter()
        .filter_map(|node| node.plugin())
        .filter(|(plugin_type, body)| {
            *plugin_type == "lash.trigger" && body["source_type"] == "ui.button.pressed"
        })
        .collect::<Vec<_>>();
    assert!(trigger_nodes.is_empty());
    Ok(())
}

#[tokio::test]
async fn observation_reads_do_not_wait_for_active_turn() -> Result<()> {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(checkpoint_gated_provider(entered_tx, release_rx))
        .model(mock_model_spec())
        .tools(Arc::new(AppTools))
        .plugin(Arc::new(StaticPluginFactory::new(
            "nonblocking-observation-query",
            lash_core::PluginSpec::new()
                .with_plugin_query_typed::<NonblockingObservationQuery, _, _>(
                    |_ctx, _args| async move {
                        Ok::<_, lash_core::PluginOperationFailure>(
                            serde_json::json!({ "ok": true }),
                        )
                    },
                ),
        )))
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build()?;
    let session = core.session("nonblocking-observation").open().await?;
    let turn_session = session.clone();
    let scoped_effect_controller = turn_scope(&turn_session.session_id());
    let turn = tokio::spawn(async move {
        turn_session
            .turn(TurnInput::text("blocked"))
            .advanced()
            .run_with_scope(scoped_effect_controller)
            .await
    });

    entered_rx.await.expect("provider entered");

    let observed = tokio::time::timeout(std::time::Duration::from_millis(50), async {
        let _ = session.session_id();
        let _ = session.policy_snapshot();
        let _ = session.read_view();
        let _ = session.usage_report();
        let _ = session.admin().tools().state().await?;
        let _ = session.admin().tools().active_manifests().await?;
        let (_plugin_id, query_output) = session
            .plugin_operations()
            .query_raw(
                <NonblockingObservationQuery as lash_core::PluginOperation>::NAME,
                serde_json::json!({}),
            )
            .await?;
        assert_eq!(
            query_output.get("ok").and_then(|value| value.as_bool()),
            Some(true)
        );
        let _ = session.processes().list().await?;
        Result::<()>::Ok(())
    })
    .await
    .expect("observation reads should not wait for the turn");
    observed?;

    release_tx.send(()).expect("release provider");
    turn.await.expect("turn task")?;
    Ok(())
}

#[tokio::test]
async fn processes_cancel_cancels_visible_process() -> Result<()> {
    let runtime_host = RuntimeHostConfig::in_memory();
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .advanced()
        .runtime_host_config(runtime_host)
        .build()?;
    let session = core.session("host-cancel").open().await?;
    session
        .processes()
        .start(
            lash_core::ProcessStartRequest::external(
                "host-process",
                lash_core::ProcessOriginator::host(),
                serde_json::Value::Null,
            )
            .with_observers(["host-cancel".to_string()]),
            inline_scope(lash_core::ExecutionScope::process("host-process")),
        )
        .await?;

    let summary = session
        .processes()
        .cancel(
            "host-process",
            inline_scope(lash_core::ExecutionScope::process("host-process")),
        )
        .await?;

    assert_eq!(summary.process_id, "host-process");
    assert_eq!(
        summary.status,
        lash_core::ProcessStatus::Running,
        "cancel is a durable request; the runner owns terminalization"
    );
    assert!(
        core.processes()
            .events("host-process", 0)
            .await?
            .iter()
            .any(|event| event.event_type == "process.cancel_requested"),
        "the visible-process cancel appended its durable request"
    );
    Ok(())
}

#[tokio::test]
async fn processes_cancel_all_cancels_visible_processes() -> Result<()> {
    let runtime_host = RuntimeHostConfig::in_memory();
    let registry =
        Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn lash_core::ProcessRegistry>;
    let driver =
        lash_core::ProcessWorkDriver::new(Arc::clone(&registry), Arc::new(NoopProcessRunHandle));
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .process_work_driver(driver)
        .advanced()
        .runtime_host_config(runtime_host)
        .build()?;
    let session = core.session("host-cancel-all").open().await?;
    for process_id in ["host-process-a", "host-process-b"] {
        session
            .processes()
            .start(
                lash_core::ProcessStartRequest::external(
                    process_id,
                    lash_core::ProcessOriginator::host(),
                    serde_json::Value::Null,
                )
                .with_observers(["host-cancel-all".to_string()]),
                inline_scope(lash_core::ExecutionScope::process(process_id)),
            )
            .await?;
    }

    let mut summaries = session
        .processes()
        .cancel_all(runtime_operation_scope(&core, "host-cancel-all"))
        .await?;
    summaries.sort_by(|left, right| left.process_id.cmp(&right.process_id));

    assert_eq!(
        summaries
            .iter()
            .map(|summary| summary.process_id.as_str())
            .collect::<Vec<_>>(),
        vec!["host-process-a", "host-process-b"]
    );
    Ok(())
}

#[tokio::test]
async fn observation_updates_after_completed_turn() -> Result<()> {
    let core = standard_core();
    let session = core.session("observation-after-turn").open().await?;

    assert!(session.read_view().messages().is_empty());
    session
        .turn(TurnInput::text("hello observation"))
        .run()
        .await?;

    let observed = session.observe();
    assert_eq!(observed.read_view().messages().len(), 2);
    assert_eq!(observed.usage_report().usage.usage.output_tokens, 2);
    assert_eq!(observed.policy_snapshot().model.id, "mock-model");
    Ok(())
}

#[tokio::test]
async fn config_and_tool_mutations_publish_observation_immediately() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .tools(Arc::new(AppTools))
        .build()?;
    let session = core.session("observation-mutations").open().await?;

    session
        .admin()
        .config()
        .set_prompt_template(PromptTemplate::new(vec![
            lash_core::PromptTemplateSection::untitled(vec![lash_core::PromptTemplateEntry::text(
                "updated",
            )]),
        ]))
        .await?;
    assert!(session.policy_snapshot().prompt.template.is_some());

    session
        .admin()
        .tools()
        .set_membership("tool:app_lookup", false)
        .await?;
    let tool_state = session
        .observe()
        .tool_state()
        .expect("tool state should be observable");
    assert!(
        !tool_state
            .get(&lash_core::ToolId::from("tool:app_lookup"))
            .expect("app tool")
            .is_member(),
        "the host-removed tool is a non-member"
    );
    Ok(())
}

#[tokio::test]
async fn session_control_manages_child_session_lifecycle() -> Result<()> {
    let core = standard_core();
    let session = core.session("parent-control").open().await?;
    let children = session.admin().children();
    let child = children
        .create_session(SessionCreateRequest {
            session_id: Some("child-control".to_string()),
            relation: lash_core::SessionRelation::Child {
                parent_session_id: "parent-control".to_string(),
                caused_by: None,
            },
            start: lash_core::SessionStartPoint::Empty,
            policy: None,
            plugin_source: lash_core::SessionPluginSource::CurrentSessionFork,
            initial_nodes: Vec::new(),
            tool_access: lash_core::SessionToolAccess::default(),
            subagent: None,
            context_overlay: lash_core::SessionContextOverlay::default(),
            plugin_options: lash_core::PluginOptions::default(),
            usage_source: None,
        })
        .await?;

    assert_eq!(child.session_id, "child-control");
    children.close_session(&child.session_id).await?;
    Ok(())
}
