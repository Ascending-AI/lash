use super::*;
use lash_core::{
    ProcessEventLog as _, ProcessEventLogTestSupport as _, ProcessObserverRegistry as _,
    SessionCommitStore as _,
};
use lash_sansio::ProcessId;
use lash_sansio::SessionId;

fn complete_full_page<Full, Lite>(
    outcome: lash_core::ProcessEventReadOutcome<lash_core::ProcessEventPage<Full, Lite>>,
) -> Vec<Full> {
    match outcome {
        lash_core::ProcessEventReadOutcome::Retained(lash_core::ProcessEventPage {
            events: lash_core::ProcessEventPageEvents::Full(events),
            more: lash_core::ProcessEventPageMore::Complete,
        }) => events,
        _ => panic!("expected one complete full event page"),
    }
}

struct NoopProcessWork;

#[async_trait]
impl lash_core::ProcessWorkSubstrate for NoopProcessWork {
    async fn admit_pending_processes(
        &self,
        _reason: &str,
    ) -> std::result::Result<
        lash_core::facade_support::ProcessAdmissionReport,
        lash_core::PluginError,
    > {
        Ok(lash_core::facade_support::ProcessAdmissionReport::default())
    }

    async fn await_process_terminal(
        &self,
        process_ref: &lash_core::ProcessRef,
    ) -> std::result::Result<lash_core::ProcessTerminalWait, lash_core::PluginError> {
        panic!("unexpected terminal wait for {process_ref}")
    }
}

struct NonblockingObservationQuery;

struct HideAllProcessTools;

impl lash_core::facade_support::ProcessToolVisibilityFilter for HideAllProcessTools {
    fn narrow(
        &self,
        _session: &lash_core::SessionId,
        _candidates: &[lash_core::ProcessId],
    ) -> Vec<lash_core::ProcessId> {
        Vec::new()
    }
}

fn explicit_runtime_host_config(provider: ProviderHandle) -> RuntimeHostConfig {
    let mut config = RuntimeHostConfig::new(
        Arc::new(
            crate::durability::NativeEffectHost::default().allow_process_lifetime_completion_keys(),
        ),
        Arc::new(crate::persistence::InMemoryAttachmentStore::new()),
        Arc::new(crate::persistence::InMemoryProcessExecutionEnvStore::new()),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    config.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(provider),
    );
    config
}

fn provider_session_spec(provider: &ProviderHandle) -> crate::SessionSpec {
    crate::SessionSpec::new()
        .provider_id(provider.kind())
        .turn_budget(crate::TurnBudget::Unbounded)
}

impl lash_core::facade_support::PluginOperation for NonblockingObservationQuery {
    const NAME: &'static str = "test.nonblocking_observation_query";
    const DESCRIPTION: &'static str =
        "Return a static value through observation-backed query dispatch.";
    const SESSION_PARAM: lash_core::facade_support::SessionParam =
        lash_core::facade_support::SessionParam::Optional;

    type Args = serde_json::Value;
    type Output = serde_json::Value;
}

impl lash_core::facade_support::PluginQuery for NonblockingObservationQuery {}

struct FixedCompactor;

#[async_trait]
impl lash_core::facade_support::ContextCompactor for FixedCompactor {
    fn id(&self) -> &'static str {
        "test.fixed_compactor"
    }

    async fn compact(
        &self,
        ctx: &lash_core::facade_support::CompactionContext<'_>,
    ) -> std::result::Result<
        Option<lash_core::facade_support::ContextCompaction>,
        lash_core::facade_support::ContextError,
    > {
        assert_eq!(
            ctx.instructions.as_deref(),
            Some("focus on durable summary")
        );
        assert!(
            ctx.state
                .messages()
                .iter()
                .any(|message| message.parts[0].content().contains("old durable request"))
        );
        Ok(Some(lash_core::facade_support::ContextCompaction::new(
            vec![lash_core::SessionAppendNode::message(
                lash_core::PluginMessage::text(
                    lash_core::MessageRole::Assistant,
                    "Compaction summary:\nold durable request summarized",
                )
                .with_origin(lash_core::MessageOrigin::Plugin {
                    plugin_id: "test_compactor".to_string(),
                    transient: false,
                }),
            )],
        )))
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
    assert!(session.admin().processes().list().await?.is_empty());
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
        .restore_execution(&lash_core::plugin::HydratedExecutionState {
            root: vec![1, 2, 3].into(),
            components: std::collections::BTreeMap::new(),
        })
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .plugin(Arc::new(StaticPluginFactory::new(
            "test-compactor",
            lash_core::facade_support::PluginSpec::new()
                .with_context_compactor(100, Arc::new(FixedCompactor)),
        )))
        .build(crate::testing::runtime_lease_owner())?;
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
                .map(lash_core::NodeId::as_str)
                == previous_frame_node_id.as_deref()
                && node.message().is_some_and(|message| {
                    message.parts[0].content().contains("old durable request")
                })
        }),
        "initial frame should contain the original request"
    );

    // Boxed: the future carries the scoped controller, which now also carries
    // the admitted incarnation (FIG-3394) — past the `large_futures` budget.
    let compacted = Box::pin(session.admin().state().compact_context(
        Some("focus on durable summary".to_string()),
        runtime_operation_scope(&core, "compact-context-test"),
    ))
    .await?;

    assert!(compacted);
    let read_view = session.read_view();
    assert_eq!(read_view.messages().len(), 1);
    assert_eq!(
        read_view.messages()[0].parts[0].content(),
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
    // Compaction is a direct completion now (FIG-3374): the result commits in
    // the same publication as the frame switch, so the resident change rides
    // on `Committed`'s read view rather than a separate `ResidentChanged`.
    assert!(
        events.windows(2).any(|window| matches!(
            (&window[0].payload, &window[1].payload),
            (
                lash_core::SessionObservationEventPayload::AgentFrameSwitched { .. },
                lash_core::SessionObservationEventPayload::Committed { read_view }
            ) if read_view.messages().iter().any(|message| {
                message.parts[0].content().contains("old durable request summarized")
            })
        )),
        "expected AgentFrameSwitched immediately followed by Committed carrying the summary, got {events:?}"
    );

    let after = session.admin().state().persist_current().await?;
    let current = after
        .agent_frames
        .iter()
        .find(|frame| Some(frame.frame_node_id.as_str()) == after.current_frame_node_id.as_deref())
        .expect("current frame");
    assert_eq!(current.reason.as_str(), "compaction");
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
                .map(lash_core::NodeId::as_str)
                == previous_frame_node_id.as_deref()
                && node.message().is_some_and(|message| {
                    message.parts[0].content().contains("old durable request")
                })
        }),
        "previous frame content should remain durable after compaction"
    );
    assert!(
        after.session_graph.nodes.iter().any(|node| {
            after
                .session_graph
                .nearest_frame_node_id(Some(&node.node_id))
                .map(lash_core::NodeId::as_str)
                == after.current_frame_node_id.as_deref()
                && node.message().is_some_and(|message| {
                    message.parts[0]
                        .content()
                        .contains("old durable request summarized")
                })
        }),
        "compaction summary should be scoped to the new frame"
    );
    Ok(())
}

struct PromptAssertingCompactor;

#[async_trait]
impl lash_core::facade_support::ContextCompactor for PromptAssertingCompactor {
    fn id(&self) -> &'static str {
        "test.prompt_asserting_compactor"
    }

    async fn compact(
        &self,
        ctx: &lash_core::facade_support::CompactionContext<'_>,
    ) -> std::result::Result<
        Option<lash_core::facade_support::ContextCompaction>,
        lash_core::facade_support::ContextError,
    > {
        // FIG-3374: the direct completion carries the same prompt stack a
        // turn on this session would resolve — capability (context + plugin
        // hook), core, and session layers — minus the turn layer and every
        // tool-gated contribution, since the request ships no tools.
        let prompt = ctx
            .system_prompt
            .as_deref()
            .expect("compaction request carries the resolved prompt stack");
        for marker in [
            "core-layer-guidance-marker",
            "plugin-hook-guidance-marker",
            "session-layer-guidance-marker",
        ] {
            assert!(
                prompt.contains(marker),
                "system prompt must contain `{marker}`: {prompt}"
            );
        }
        assert!(
            !prompt.contains("tool-gated-guidance-marker"),
            "a tool-gated contribution cannot ship on a no-tools request: {prompt}"
        );
        Ok(Some(lash_core::facade_support::ContextCompaction::new(
            vec![lash_core::SessionAppendNode::message(
                lash_core::PluginMessage::text(
                    lash_core::MessageRole::Assistant,
                    "Compaction summary:\nprompt stack pinned",
                )
                .with_origin(lash_core::MessageOrigin::Plugin {
                    plugin_id: "test_prompt_compactor".to_string(),
                    transient: false,
                }),
            )],
        )))
    }
}

#[tokio::test]
async fn compact_context_system_prompt_carries_the_full_prompt_stack() -> Result<()> {
    let core = explicit_ephemeral_facets(
        LashCore::standard_builder(crate::TurnBudget::Unbounded)
            .instructions("core-layer-guidance-marker"),
    )
    .provider(mock_provider())
    .model(mock_model_spec())
    .plugin(Arc::new(StaticPluginFactory::new(
        "test-prompt-compactor",
        lash_core::facade_support::PluginSpec::new()
            .with_context_compactor(100, Arc::new(PromptAssertingCompactor))
            .with_prompt_contributor(Arc::new(|_ctx| {
                Box::pin(async {
                    Ok(vec![
                        lash_core::PromptContribution::guidance(
                            "hook",
                            "plugin-hook-guidance-marker",
                        ),
                        lash_core::PromptContribution::guidance(
                            "gated",
                            "tool-gated-guidance-marker",
                        )
                        .requires_tool("absent_tool"),
                    ])
                })
            })),
    )))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("compact-prompt-stack")
        .instructions("session-layer-guidance-marker")
        .open()
        .await?;
    session
        .turn(TurnInput::text("content to compact"))
        .run()
        .await?;
    assert!(
        Box::pin(session.admin().state().compact_context(
            None,
            runtime_operation_scope(&core, "compact-prompt-stack-test"),
        ))
        .await?
    );
    Ok(())
}

#[tokio::test]
async fn session_commands_enqueue_idempotently_by_source_key() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("command-idempotency").open().await?;

    let first = session
        .admin()
        .commands()
        .refresh_tool_catalog("test refresh", "same-refresh")
        .await?;
    let second = session
        .admin()
        .commands()
        .refresh_tool_catalog("test refresh", "same-refresh")
        .await?;

    assert_eq!(first.batch_id, second.batch_id);
    assert_eq!(
        first.source_key,
        "command:refresh_tool_catalog:same-refresh"
    );
    let queued = session.durable().queued_work().await?;
    assert_eq!(queued.len(), 1);
    assert!(matches!(
        &queued[0].items[0].payload,
        lash_core::runtime::QueuedWorkPayload::SessionCommand { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn queue_enqueue_and_cancel_emit_typed_observation_events() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("queue-observation-events").open().await?;
    let cursor = session.observe().current_observation().cursor;

    let pending = session
        .durable()
        .enqueue(TurnInput::text("queued observation"))
        .id("queue-observation")
        .send()
        .await?;
    let inputs = session.durable().pending_turn_inputs().await?;
    assert_eq!(
        inputs
            .iter()
            .map(|input| input.input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![pending.input_id.as_str()]
    );
    let cancelled = session
        .durable()
        .cancel_pending_turn_input(&pending.input_id)
        .await?;
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("pending-input-facade-cancel").open().await?;
    let cursor = session.observe().current_observation().cursor;

    let first = session
        .durable()
        .enqueue(TurnInput::text("first"))
        .id("edit:1")
        .send()
        .await?;
    let second = session
        .durable()
        .enqueue(TurnInput::text("second"))
        .id("edit:2")
        .send()
        .await?;
    let third = session
        .durable()
        .enqueue(TurnInput::text("third"))
        .id("edit:3")
        .send()
        .await?;

    let bulk = session
        .durable()
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
        .durable()
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
    assert!(session.durable().pending_turn_inputs().await?.is_empty());

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
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let watched = lash_core::facade_support::watch_process_registry(
        registry.clone() as Arc<dyn lash_core::ProcessRegistry>
    );
    let wiring = lash_core::ProcessWorkWiring::new(watched, Arc::new(NoopProcessWork));
    let provider = mock_provider();
    let session_spec = provider_session_spec(&provider);
    let runtime_host = explicit_runtime_host_config(provider);
    let core = LashCore::standard_builder(crate::TurnBudget::Unbounded)
        .session_spec(session_spec)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_work(wiring)
        .with_native_queued_work()
        .advanced()
        .runtime_host_config(runtime_host)
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("process-observation-events").open().await?;
    let cursor = session.observe().current_observation().cursor;
    let process_id = "observed-process";

    let request = lash_core::ProcessStartRequest::new(
        process_id,
        lash_core::ProcessInput::ToolCall {
            call: lash_core::PreparedToolCall::from_parts(
                "observed-process-call",
                "tool:observed-process",
                "observed_process",
                serde_json::Value::Null,
                None,
                serde_json::Value::Null,
            ),
        },
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessOriginator::host(),
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon,
        ),
    )
    .with_observers(["process-observation-events".to_string()])
    .with_env_spec(lash_core::ProcessExecutionEnvSpec::new(
        lash_core::PluginOptions::empty(),
        lash_core::SessionPolicy {
            model: mock_model_spec(),
            ..lash_core::SessionPolicy::new(crate::TurnBudget::Unbounded)
        },
    ));
    let started = session
        .admin()
        .processes()
        .start(request.clone(), native_process_scope(process_id))
        .await?;
    assert_eq!(
        lash_core::ProcessQuery::get_process(registry.as_ref(), &started.process_id)
            .await?
            .expect("started record")
            .lifecycle,
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon
        )
    );
    session
        .admin()
        .processes()
        .start(
            request,
            native_scope(lash_core::AdmittedScope::runtime_operation(
                "process-observation-events-replay",
            )),
        )
        .await
        .expect("public session start replay bypasses the retired staging owner");
    session
        .admin()
        .processes()
        .cancel(
            &ProcessId::from(process_id),
            native_process_scope(process_id),
        )
        .await?;

    let SessionResume::Replayed { events } = session.observe().resume_from_cursor(&cursor)? else {
        panic!("recent cursor should replay process observation events");
    };
    let durable = registry
        .recent_events(&ProcessId::from(process_id), 128)
        .await?;
    let lifecycle = durable
        .iter()
        .filter_map(|event| {
            SessionProcessEventKind::from_durable_event(&event.event_type, event.sequence)
        })
        .collect::<Vec<_>>();
    assert!(
        !lifecycle.is_empty(),
        "cancel must append a lifecycle event"
    );
    for expected in lifecycle {
        assert!(
            events.iter().any(|event| matches!(
                &event.payload,
                lash_core::SessionObservationEventPayload::ProcessChanged { kind, process_ids }
                    if *kind == expected
                        && process_ids.as_slice() == [ProcessId::from(process_id)]
            )),
            "missing journaled lifecycle {expected:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn trigger_emit_does_not_append_session_node_or_queue_work() -> Result<()> {
    let trigger = lash_core::facade_support::TriggerEvent::new(
        "Button",
        "ui.button",
        "pressed",
        lash_core::LashSchema::any(),
    );
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .plugin(Arc::new(StaticPluginFactory::new(
            "button-triggers",
            lash_core::facade_support::PluginSpec::new().with_trigger_event(trigger),
        )))
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("command-trigger").open().await?;
    let before = session.admin().state().persist_current().await?;

    let source_key = lash_core::facade_support::empty_trigger_source_key("ui.button.pressed")?;
    let scoped_effect_controller = lash_core::ScopedEffectController::shared(
        Arc::new(lash_core::facade_support::NativeRuntimeEffectController::default()),
        lash_core::AdmittedScope::runtime_operation("trigger:button-press-1"),
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

    assert!(session.durable().queued_work().await?.is_empty());
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(checkpoint_gated_provider(entered_tx, release_rx))
        .model(mock_model_spec())
        .tools(Arc::new(AppTools))
        .plugin(Arc::new(StaticPluginFactory::new(
            "nonblocking-observation-query",
            lash_core::facade_support::PluginSpec::new()
                .with_plugin_query_typed::<NonblockingObservationQuery, _, _>(
                    |_ctx, _args| async move {
                        Ok::<_, lash_core::test_support::PluginOperationFailure>(
                            serde_json::json!({ "ok": true }),
                        )
                    },
                ),
        )))
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build(crate::testing::runtime_lease_owner())?;
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
                <NonblockingObservationQuery as lash_core::facade_support::PluginOperation>::NAME,
                serde_json::json!({}),
            )
            .await?;
        assert_eq!(
            query_output.get("ok").and_then(|value| value.as_bool()),
            Some(true)
        );
        let _ = session.admin().processes().list().await?;
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
    let provider = mock_provider();
    let session_spec = provider_session_spec(&provider);
    let core = LashCore::standard_builder(crate::TurnBudget::Unbounded)
        .session_spec(session_spec)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .without_queued_work()
        .advanced()
        .runtime_host_config(explicit_runtime_host_config(provider))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("host-cancel").open().await?;
    session
        .admin()
        .processes()
        .start(
            lash_core::ProcessStartRequest::external(
                "host-process",
                lash_core::ProcessOriginator::host(),
                serde_json::Value::Null,
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_observers(["host-cancel".to_string()]),
            native_process_scope("host-process"),
        )
        .await?;

    let summary = session
        .admin()
        .processes()
        .cancel(
            &ProcessId::from("host-process"),
            native_process_scope("host-process"),
        )
        .await?;

    assert_eq!(summary.process_id, "host-process");
    assert_eq!(
        summary.status,
        lash_core::ProcessStatus::Running,
        "cancel is a durable request; the runner owns terminalization"
    );
    assert!(
        complete_full_page(
            core.processes()
                .events(
                    &ProcessId::from("host-process"),
                    std::num::NonZeroUsize::new(64).expect("non-zero event page size"),
                    lash_core::ProcessEventQueryMode::Full,
                    None,
                )
                .await?
        )
        .iter()
        .any(|event| event.event_type == "process.cancel_requested"),
        "the visible-process cancel appended its durable request"
    );
    Ok(())
}

#[tokio::test]
async fn process_admin_list_signal_and_cancel_bypass_model_tool_filter() -> Result<()> {
    let provider = mock_provider();
    let session_spec = provider_session_spec(&provider);
    let mut runtime_host = explicit_runtime_host_config(provider);
    runtime_host.control.process_tool_visibility_filter = Some(Arc::new(HideAllProcessTools));
    let core = LashCore::standard_builder(crate::TurnBudget::Unbounded)
        .session_spec(session_spec)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .without_queued_work()
        .advanced()
        .runtime_host_config(runtime_host)
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("host-filter-bypass").open().await?;
    for process_id in ["host-filter-signal", "host-filter-cancel"] {
        session
            .admin()
            .processes()
            .start(
                lash_core::ProcessStartRequest::external(
                    process_id,
                    lash_core::ProcessOriginator::host(),
                    serde_json::Value::Null,
                    lash_core::ProcessLifecyclePolicy::new(
                        lash_core::ParentScope::Host,
                        lash_core::OnParentEnd::Abandon,
                    ),
                )
                .with_extra_event_types([lash_core::ProcessEventType {
                    name: "signal.ready".to_string(),
                    payload_schema: lash_core::LashSchema::any(),
                    semantics: lash_core::ProcessEventSemanticsSpec::default(),
                }])
                .with_observers(["host-filter-bypass".to_string()]),
                native_process_scope(process_id),
            )
            .await?;
    }

    assert_eq!(
        session.admin().processes().list_all().await?.len(),
        2,
        "host list_all must retain the complete observer-edge view"
    );
    session
        .admin()
        .processes()
        .signal(
            &ProcessId::from("host-filter-signal"),
            "ready",
            "host-filter-signal-id",
            serde_json::json!({"source": "host"}),
            native_process_scope("host-filter-signal"),
        )
        .await?;
    assert!(
        complete_full_page(
            session
                .admin()
                .processes()
                .events(
                    &ProcessId::from("host-filter-signal"),
                    std::num::NonZeroUsize::new(64).expect("non-zero event page size"),
                    lash_core::ProcessEventQueryMode::Full,
                    None,
                )
                .await?,
        )
        .iter()
        .any(|event| event.event_type == "signal.ready"),
        "host events must expose the signal hidden from model tools"
    );
    session
        .admin()
        .processes()
        .cancel(
            &ProcessId::from("host-filter-cancel"),
            native_process_scope("host-filter-cancel"),
        )
        .await?;
    assert!(
        complete_full_page(
            session
                .admin()
                .processes()
                .events(
                    &ProcessId::from("host-filter-cancel"),
                    std::num::NonZeroUsize::new(64).expect("non-zero event page size"),
                    lash_core::ProcessEventQueryMode::Full,
                    None,
                )
                .await?
        )
        .iter()
        .any(|event| event.event_type == "process.cancel_requested"),
        "host cancel must bypass the model-tool filter"
    );
    lash_core::testing::runbook_evidence::checkpoint(serde_json::json!({
        "checkpoint": "host_admin_rail_bypasses_the_model_tool_filter",
        "model_tool_filter": "HideAllProcessTools",
        "host_list_all_len": 2,
        "signalled_process_id": "host-filter-signal",
        "host_signal_event_type": "signal.ready",
        "cancelled_process_id": "host-filter-cancel",
        "host_cancel_event_type": "process.cancel_requested",
    }));
    Ok(())
}

#[tokio::test]
async fn processes_cancel_all_cancels_visible_processes() -> Result<()> {
    let provider = mock_provider();
    let session_spec = provider_session_spec(&provider);
    let runtime_host = explicit_runtime_host_config(provider);
    let registry =
        Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn lash_core::ProcessRegistry>;
    let watched = lash_core::facade_support::watch_process_registry(registry);
    let wiring = lash_core::ProcessWorkWiring::new(watched, Arc::new(NoopProcessWork));
    let core = LashCore::standard_builder(crate::TurnBudget::Unbounded)
        .session_spec(session_spec)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_work(wiring)
        .with_native_queued_work()
        .advanced()
        .runtime_host_config(runtime_host)
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("host-cancel-all").open().await?;
    for process_id in ["host-process-a", "host-process-b"] {
        session
            .admin()
            .processes()
            .start(
                lash_core::ProcessStartRequest::external(
                    process_id,
                    lash_core::ProcessOriginator::host(),
                    serde_json::Value::Null,
                    lash_core::ProcessLifecyclePolicy::new(
                        lash_core::ParentScope::Host,
                        lash_core::OnParentEnd::Abandon,
                    ),
                )
                .with_observers(["host-cancel-all".to_string()]),
                native_process_scope(process_id),
            )
            .await?;
    }

    let mut summaries = session
        .admin()
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .tools(Arc::new(AppTools))
        .build(crate::testing::runtime_lease_owner())?;
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
async fn config_admin_sets_persisted_tool_access() -> Result<()> {
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .tools(Arc::new(AppTools))
        .store_factory(Arc::clone(&store_factory) as Arc<dyn SessionStoreFactory>)
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("config-admin-tool-access").open().await?;
    let access = lash_core::SessionToolAccess::ambient()
        .with_hidden_tools(["app_lookup"])
        .expect("valid hidden tool");

    Box::pin(session.admin().config().set_tool_access(access.clone())).await?;

    let store = store_factory
        .raw_store_for_testing(&SessionId::from("config-admin-tool-access"))
        .expect("session store");
    assert_eq!(
        store
            .load_session_head_meta()
            .await
            .expect("load session head")
            .expect("session head")
            .config
            .tool_access,
        access
    );
    Ok(())
}

/// FIG-3373 / ADR 0089: a host-run related session is an ordinary session.
/// `core.session(child).parent(parent)` admits it under its own Session
/// Binding, records the Child relation, carries the provider pin, and runs a
/// turn — there is no second session model behind a child-admin facade.
#[tokio::test]

async fn related_session_opens_with_parent_and_runs_a_turn() -> Result<()> {
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::clone(&store_factory) as Arc<dyn SessionStoreFactory>)
        .build(crate::testing::runtime_lease_owner())?;
    let _parent = core.session("parent-control").open().await?;

    let child = core
        .session("child-control")
        .parent("parent-control")
        .open()
        .await?;

    assert_eq!(child.parent_session_id(), Some("parent-control"));
    assert_eq!(child.policy_snapshot().recorded_provider_id(), "embed-test");
    child.turn(TurnInput::text("child turn")).run().await?;
    assert!(
        store_factory
            .open_existing_store_by_id(&SessionId::from("child-control"))
            .await
            .expect("read session catalog")
            .is_some(),
        "the related session was admitted under its own store binding"
    );
    Ok(())
}

/// FIG-3373: process-observer intents are persisted facts settled at open.
/// A host that creates the related session's store carrying
/// `pending_observer_intents` gets the same observer publication the deleted
/// `children().create_session` facade performed — `SessionBuilder::open`
/// reconciles them through `SessionObserverIntentSource::PersistedIfPresent`
/// before returning the handle.
#[tokio::test]
async fn persisted_observer_intents_publish_before_open_returns() -> Result<()> {
    let sqlite_dir = tempfile::tempdir().expect("create managed-create SQLite directory");
    let cases: Vec<(&str, Option<Arc<dyn lash_core::SessionStoreFactory>>)> = vec![
        (
            "in-memory",
            Some(Arc::new(
                lash_core::facade_support::InMemorySessionStoreFactory::new(),
            )),
        ),
        (
            "sqlite",
            Some(Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
                sqlite_dir.path().join("managed-create-sessions"),
            ))),
        ),
    ];

    for (case, store_factory) in cases {
        let store_factory = store_factory.expect("every case selects a store");
        let parent_session_id = SessionId::from(format!("managed-observer-parent-{case}"));
        let child_session_id = SessionId::from(format!("managed-observer-child-{case}"));
        let create_process_id = ProcessId::from(format!("managed-create-process-{case}"));
        let registry = Arc::new(TestLocalProcessRegistry::default());
        let process_registry = registry.clone() as Arc<dyn lash_core::ProcessRegistry>;
        let watched = lash_core::facade_support::watch_process_registry(process_registry);
        let wiring = lash_core::ProcessWorkWiring::new(watched, Arc::new(NoopProcessWork));
        let mut builder =
            explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
                .provider(mock_provider())
                .model(mock_model_spec())
                .process_work(wiring);
        builder = builder
            .store_factory(Arc::clone(&store_factory))
            .with_native_queued_work();
        let core = builder.build(crate::testing::runtime_lease_owner())?;
        let _parent = core.session(&parent_session_id).open().await?;

        registry
            .register_process(lash_core::ProcessRegistration::new(
                &create_process_id,
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            ))
            .await?;

        let child_store = store_factory
            .create_store(&lash_core::SessionStoreCreateRequest {
                pending_observer_intents: vec![
                    lash_core::facade_support::SessionObserverIntent::host_requested(
                        create_process_id.clone(),
                    ),
                ],
                session_id: child_session_id.clone(),
                relation: lash_core::SessionRelation::Child {
                    parent_session_id: parent_session_id.clone(),
                    caused_by: None,
                },
                policy: lash_core::SessionPolicy {
                    provider_id: mock_provider().kind().to_string(),
                    model: mock_model_spec(),
                    ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
                },
            })
            .await?;

        let child = core
            .session(&child_session_id)
            .store(child_store)
            .parent(&parent_session_id)
            .open()
            .await?;

        assert_eq!(child.session_id(), child_session_id);
        assert!(
            registry
                .is_observer(&child_session_id, &create_process_id)
                .await?,
            "the returned live session must have its create observer edge"
        );
        let create_events = registry.full_event_window(&create_process_id, 0).await?;
        assert!(create_events.iter().any(|event| {
            event.event_type == "process.observer_added"
                && event.payload["by"]
                    == serde_json::json!({
                        "kind": "host",
                        "operation_id": format!("session-create:{child_session_id}")
                    })
        }));
        let child_store = store_factory
            .open_existing_store(&lash_core::SessionStoreCreateRequest {
                pending_observer_intents: Vec::new(),
                session_id: child_session_id.clone(),
                relation: lash_core::SessionRelation::Root,
                policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
            })
            .await
            .expect("open child store")
            .expect("child store exists");
        let child_meta = child_store
            .load_session_meta()
            .await?
            .expect("child metadata exists");
        assert!(matches!(
            child_meta.relation,
            lash_core::SessionRelation::Child { .. }
        ));
        assert!(child_meta.pending_observer_intents.is_empty());
    }
    Ok(())
}

/// ADR 0069: a direct turn is admitted through the same durable acceptance the
/// queue uses, and the handle it returns names that admission.
#[tokio::test]
async fn direct_turn_reports_the_acceptance_it_was_admitted_under() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("direct-turn-acceptance").open().await?;

    let first = session.turn(TurnInput::text("same words")).run().await?;
    let acceptance = first
        .result
        .acceptance
        .as_ref()
        .expect("a store-backed direct turn is admitted through a durable acceptance");
    assert_eq!(acceptance.session_id, "direct-turn-acceptance");
    assert!(!acceptance.input_id.is_empty());
    assert_eq!(
        acceptance.ingress,
        lash_core::runtime::TurnInputIngress::next_turn()
    );
    assert_eq!(
        acceptance.source_key, None,
        "direct ingress admits, it does not mint an identity to deduplicate by"
    );
    assert!(
        session.durable().pending_turn_inputs().await?.is_empty(),
        "a committed turn settles the row it was admitted under"
    );

    // The documented double-submit window: resubmitting the same content after
    // an unacknowledged crash is a second admission, not a deduplicated retry,
    // because the caller named no identity Lash could recognise it by.
    let second = session.turn(TurnInput::text("same words")).run().await?;
    assert_ne!(
        acceptance.input_id,
        second
            .result
            .acceptance
            .as_ref()
            .expect("the second direct turn is admitted too")
            .input_id
    );
    Ok(())
}

/// Facade admission refuses a session with no selected store, so there is no
/// storeless exception to durable turn acceptance.
#[tokio::test]
async fn a_session_without_a_store_cannot_bypass_turn_acceptance() -> Result<()> {
    let core = core_without_session_store();
    let error = match core.session("missing-store-acceptance").open().await {
        Ok(_) => panic!("facade session admission requires a store"),
        Err(error) => error,
    };
    assert!(matches!(error, EmbedError::MissingSessionStore));
    Ok(())
}
