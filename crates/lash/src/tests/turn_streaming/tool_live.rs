//! TOOL-LIVE-HOST: host-executed tools publish without durable observations.
use super::*;
use futures_util::StreamExt;
use lash_core::{ToolLiveFact, ToolLiveFilter, ToolLiveItem, ToolLivePayload};

struct ProgressTools {
    address: Arc<StdMutex<Option<lash_core::CallAddress>>>,
}
#[async_trait]
impl ToolProvider for ProgressTools {
    fn tool_manifests(
        &self,
        _: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, crate::plugins::PluginError> {
        Ok(vec![app_tool_definition().manifest()])
    }
    fn resolve_contract(
        &self,
        name: &str,
        _: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(app_tool_definition().contract()))
    }
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let live = call.context.live();
        *self.address.lock_recover() = Some(live.address().clone());
        live.publish(ToolLiveFact::Progress {
            progress: 100.0,
            total: Some(100.0),
            message: Some("tool-live-only-marker".into()),
        });
        lash_core::ToolOutcome::ok(serde_json::json!({"ok":true})).into()
    }
}
#[tokio::test]
async fn a_host_executed_tool_publishes_progress_without_any_durable_tool_live_fact() -> Result<()>
{
    let address = Arc::new(StdMutex::new(None));
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .requires_streaming(true)
        .complete(move |request| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                let part = if call == 0 {
                    LlmOutputPart::ToolCall {
                        call_id: "host-progress".into(),
                        tool_name: "app_lookup".into(),
                        input_json: "{}".into(),
                        replay: None,
                    }
                } else {
                    LlmOutputPart::Text {
                        text: "done".into(),
                        response_meta: None,
                    }
                };
                if let Some(stream) = request.stream_events {
                    stream.send(LlmStreamEvent::Part(part.clone()));
                }
                Ok(LlmResponse {
                    parts: vec![part],
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .tools(
        crate::tool_provider_key!("tools"),
        Arc::new(ProgressTools {
            address: address.clone(),
        }),
    )
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(SessionId::from("host-tool-live"))
        .created()
        .await
        .open()
        .await?;
    let output = session
        .send(TurnInput::text("run the host tool"))
        .output()
        .await?;
    let address = address.lock_recover().clone().expect("host tool executed");
    assert_eq!(address.owner.session_id(), Some(&session.session_id()));
    let mut feed = core
        .tool_live()
        .subscribe(ToolLiveFilter::Call(address.clone()))
        .await
        .expect("subscribe");
    loop {
        if let ToolLiveItem::Emission { emission, .. } =
            feed.next().await.expect("fact").expect("read")
            && let ToolLivePayload::Fact {
                fact:
                    ToolLiveFact::Progress {
                        progress, message, ..
                    },
            } = emission.payload
        {
            assert_eq!(emission.address, address);
            assert_eq!(progress, 100.0);
            assert_eq!(message.as_deref(), Some("tool-live-only-marker"));
            break;
        }
    }
    assert!(
        !serde_json::to_string(&output.activities)
            .unwrap()
            .contains("tool-live-only-marker"),
        "attempt capture and durable activities contain no live facts"
    );
    let snapshot = session.observe().snapshot().await?;
    assert!(
        !format!("{:?}", snapshot.read_view).contains("tool-live-only-marker"),
        "committed history has no live facts"
    );
    core.shutdown().await?;
    Ok(())
}

/// TOOL-LIVE-WINDOWS: tool traffic cannot evict either owner's only live text
/// or the node history retained by a process replay window.
#[tokio::test(start_paused = true)]
async fn tool_live_flood_never_trims_uncommitted_text_or_process_node_history() {
    use lash_core::{LiveReplayStore, ProcessReplayStore};
    let session = SessionId::from("window-isolation");
    let process = lash_core::ProcessId::fixture("window-process");
    let mut session_retention =
        lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard();
    session_retention.max_events_per_session = 1;
    let sessions = lash_core::facade_support::InMemoryLiveReplayStore::new(session_retention);
    let mut process_retention = lash_core::InMemoryProcessReplayStoreConfig::standard();
    process_retention.max_events_per_process = 1;
    let processes = lash_core::InMemoryProcessReplayStore::new(process_retention);
    let start = sessions.current_cursor(&session, lash_core::SessionRevision::new(0));
    let activity = TurnActivity {
        id: lash_core::TurnActivityId::new("stopped-tail"),
        correlation_id: lash_core::TurnActivityId::new("stopped-tail"),
        event: TurnEvent::StreamBlock(StreamBlockEvent::Delta {
            kind: StreamBlockKind::AssistantText,
            block: lash_sansio::llm::types::StreamBlockIdentity::new("tail", 0),
            text: "uncommitted tail".into(),
        }),
    };
    sessions
        .publish(
            &session,
            lash_core::SessionRevision::new(0),
            vec![lash_core::LiveReplayEventDraft::new(
                None::<TurnId>,
                lash_core::SessionObservationEventPayload::TurnActivity(activity),
            )],
        )
        .await
        .unwrap();
    let process_start = processes
        .earliest_cursor(&process, lash_core::ProcessSequence::new(0))
        .await
        .unwrap();
    processes
        .publish(
            &process,
            vec![lash_core::ProcessReplayEventDraft::language_execution(
                lash_core::ProcessSequence::new(0),
                lash_core::testing::process_language_observation(
                    &process,
                    "node-history",
                    "kept-node",
                ),
            )],
        )
        .await
        .unwrap();
    let lane = lash_core::ToolLive::new(
        sessions.tool_live_window(),
        processes.tool_live_window(),
        Default::default(),
    );
    let owners = [
        lash_core::ExecutionOwner::SessionFrame {
            session_id: session,
            agent_frame_id: lash_core::FrameNodeId::new("frame").unwrap(),
        },
        lash_core::ExecutionOwner::Process {
            process_id: process,
        },
    ];
    let mut traffic = Vec::new();
    for owner in owners {
        traffic.push(
            lane.subscribe(ToolLiveFilter::Owner(owner.clone()))
                .await
                .unwrap(),
        );
        let call = lane.publisher_for_test(lash_core::CallAddress {
            owner,
            call_id: lash_core::ToolCallId::fixture("flood"),
            attempt: 1,
        });
        for n in 0..10000 {
            call.publish(ToolLiveFact::Progress {
                progress: f64::from(n),
                total: None,
                message: None,
            });
        }
        call.finish();
    }
    tokio::time::advance(std::time::Duration::from_millis(100)).await;
    for mut feed in traffic {
        assert!(
            matches!(
                feed.next().await.unwrap().unwrap(),
                ToolLiveItem::Emission { .. }
            ),
            "tool traffic reached its own window before history is checked"
        );
    }
    let lash_core::LiveReplayOutcome::Replayed(events) =
        sessions.replay_after_cursor(&start).await.unwrap()
    else {
        panic!("turn text retained");
    };
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0].payload, lash_core::SessionObservationEventPayload::TurnActivity(TurnActivity { event: TurnEvent::StreamBlock(StreamBlockEvent::Delta { text, .. }), .. }) if text == "uncommitted tail")
    );
    let lash_core::ProcessReplayOutcome::Replayed(events) =
        processes.replay_after_cursor(&process_start).await.unwrap()
    else {
        panic!("node history retained");
    };
    assert_eq!(events.len(), 1);
    assert_eq!(
        lash_core::testing::process_observation_label(&events[0]),
        "kept-node"
    );
}
