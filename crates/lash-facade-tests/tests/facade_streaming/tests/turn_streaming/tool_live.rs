//! TOOL-LIVE-HOST: host-executed tools publish without durable observations.
use super::*;
use futures_util::StreamExt;
use lash_core::{ToolLiveFact, ToolLiveFilter, ToolLiveItem, ToolLiveOmission, ToolLivePayload};

struct ProgressTools {
    address: Arc<StdMutex<Option<lash_core::CallAddress>>>,
}
#[async_trait]
impl ToolProvider for ProgressTools {
    fn tool_manifests(
        &self,
        _: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash::plugins::PluginError> {
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
        // An invalid gauge is counted, never stored, and changes nothing.
        live.publish(ToolLiveFact::Progress {
            progress: f64::NAN,
            total: Some(100.0),
            message: None,
        });
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
    let results = Arc::new(StdMutex::new(Vec::<String>::new()));
    let observed = results.clone();
    let provider = lash::testing::TestProvider::builder()
        .kind("embed-test")
        .requires_streaming(true)
        .complete(move |request| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            for block in request
                .messages
                .iter()
                .flat_map(|message| message.blocks.iter())
            {
                if let lash_core::llm::types::LlmContentBlock::ToolResult { content, .. } = block {
                    observed
                        .lock_recover()
                        .push(lash_llm_transport::tool_result_text(content).into_owned());
                }
            }
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
        lash::tool_provider_key!("tools"),
        Arc::new(ProgressTools {
            address: address.clone(),
        }),
    )
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(&core, SessionId::from("host-tool-live"))
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
    let (mut gauge, mut non_finite) = (false, false);
    while !(gauge && non_finite) {
        let ToolLiveItem::Emission { emission, .. } =
            feed.next().await.expect("fact").expect("read")
        else {
            continue;
        };
        assert_eq!(emission.address, address);
        match emission.payload {
            ToolLivePayload::Fact {
                fact:
                    ToolLiveFact::Progress {
                        progress, message, ..
                    },
            } => {
                assert_eq!(progress, 100.0);
                assert_eq!(message.as_deref(), Some("tool-live-only-marker"));
                gauge = true;
            }
            ToolLivePayload::Omitted {
                cause: ToolLiveOmission::NonFinite,
                count,
            } => {
                assert_eq!(count, 1);
                non_finite = true;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    let results = results
        .lock_recover()
        .iter()
        .map(|text| serde_json::from_str::<serde_json::Value>(text).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        results,
        [serde_json::json!({"ok": true})],
        "the tool outcome is unchanged by the invalid gauge"
    );
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
            block: lash_protocol_types::llm::types::StreamBlockIdentity::new("tail", 0),
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

struct LongTool {
    first: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
#[async_trait]
impl ToolProvider for LongTool {
    fn tool_manifests(
        &self,
        _: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash::plugins::PluginError> {
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
        live.publish(ToolLiveFact::Progress {
            progress: 1.0,
            total: Some(2.0),
            message: Some("first long tool".into()),
        });
        self.first.notify_one();
        self.release.notified().await;
        live.publish(ToolLiveFact::Progress {
            progress: 2.0,
            total: Some(2.0),
            message: None,
        });
        lash_core::ToolOutcome::ok(serde_json::json!({"ok":true})).into()
    }
}

/// TOOL-LIVE-OWNER: a host learns a new session's tool-live owner from the
/// facade before any turn commits, subscribes by it, and receives the first
/// turn's first tool's progress while that tool still runs (figments
/// feedback-14 b1).
#[tokio::test]
async fn a_host_subscribes_by_owner_before_the_first_committed_turn() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = lash::testing::TestProvider::builder()
        .kind("embed-test")
        .requires_streaming(true)
        .complete(move |request| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                let part = if call == 0 {
                    LlmOutputPart::ToolCall {
                        call_id: "first-long-tool".into(),
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
    let tool = LongTool {
        first: Arc::default(),
        release: Arc::default(),
    };
    let (first, release) = (tool.first.clone(), tool.release.clone());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .tools(lash::tool_provider_key!("tools"), Arc::new(tool))
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(&core, SessionId::from("first-turn-owner"))
        .await
        .open()
        .await?;
    let owner = session
        .observe()
        .tool_live_owner()
        .await?
        .expect("a new session has an owner before its first commit");
    let mut owners = session.observe().tool_live_owners();
    assert_eq!(owners.next().await.expect("current owner")?, owner);
    let mut feed = core
        .tool_live()
        .subscribe(ToolLiveFilter::Owner(owner.clone()))
        .await
        .expect("subscribe");
    let turn = session.send(TurnInput::text("run the long tool")).output();
    let observe = async {
        first.notified().await;
        let mut released = false;
        loop {
            let ToolLiveItem::Emission { emission, .. } =
                feed.next().await.expect("item").expect("read")
            else {
                continue;
            };
            assert_eq!(emission.address.owner, owner);
            if let ToolLivePayload::Fact {
                fact: ToolLiveFact::Progress { progress, .. },
            } = emission.payload
            {
                if progress == 1.0 && !released {
                    // Received while the tool still runs.
                    released = true;
                    release.notify_one();
                } else if progress == 2.0 {
                    assert!(released);
                    break;
                }
            }
        }
    };
    let (output, ()) = tokio::join!(turn, observe);
    output?;
    core.shutdown().await?;
    Ok(())
}

/// A replay transport whose first tool-live write takes six seconds. Session
/// observations use the ordinary replay window and do not share that delay.
struct SlowToolReplay {
    sessions: lash_core::facade_support::InMemoryLiveReplayStore,
    tools: lash_core::ToolLiveMemoryWindow,
    first: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait]
impl lash_core::ToolLiveReplay for SlowToolReplay {
    async fn publish(
        &self,
        owner: &lash_core::ExecutionOwner,
        records: Vec<lash_core::ToolLiveRecord>,
    ) -> std::result::Result<(), lash_core::ToolLiveError> {
        if self.first.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
            tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        }
        self.tools.publish(owner, records).await
    }
    async fn subscribe(
        &self,
        owner: &lash_core::ExecutionOwner,
        after: Option<lash_core::ToolLiveCursor>,
    ) -> std::result::Result<lash_core::ToolLiveSubscription, lash_core::ToolLiveError> {
        self.tools.subscribe(owner, after).await
    }
}
struct SlowSessionReplay(Arc<SlowToolReplay>);
#[async_trait]
impl lash_core::LiveReplayStore for SlowSessionReplay {
    fn tool_live_window(&self) -> Option<Arc<dyn lash_core::ToolLiveReplay>> {
        Some(self.0.clone())
    }
    async fn publish(
        &self,
        session: &SessionId,
        revision: lash_core::SessionRevision,
        events: Vec<lash_core::LiveReplayEventDraft>,
    ) -> std::result::Result<
        Vec<Arc<lash_core::SessionObservationEvent>>,
        lash_core::LiveReplayStoreError,
    > {
        self.0.sessions.publish(session, revision, events).await
    }
    async fn replay_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplayOutcome, lash_core::LiveReplayStoreError> {
        self.0.sessions.replay_after_cursor(cursor).await
    }
    async fn subscribe_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplaySubscribeOutcome, lash_core::LiveReplayStoreError>
    {
        self.0.sessions.subscribe_after_cursor(cursor).await
    }
    fn current_cursor(
        &self,
        session: &SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.0.sessions.current_cursor(session, revision)
    }
    fn earliest_cursor(
        &self,
        session: &SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.0.sessions.earliest_cursor(session, revision)
    }
    async fn invalidate_session(
        &self,
        session: &SessionId,
    ) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.0.sessions.invalidate_session(session).await
    }
    async fn invalidate_all(&self) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.0.sessions.invalidate_all().await
    }
    async fn trim_session(
        &self,
        session: &SessionId,
    ) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.0.sessions.trim_session(session).await
    }
}

struct TimedLiveTool(Arc<StdMutex<Option<lash_core::ToolLivePublisher>>>);
#[async_trait]
impl ToolProvider for TimedLiveTool {
    fn tool_manifests(
        &self,
        _: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash::plugins::PluginError> {
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
        let live = call.context.live().clone();
        live.publish(ToolLiveFact::Log {
            level: lash_core::ToolLiveLogLevel::Info,
            logger: None,
            data: serde_json::json!("timed log"),
        });
        live.publish(ToolLiveFact::Progress {
            progress: 1.0,
            total: None,
            message: None,
        });
        *self.0.lock_recover() = Some(live);
        lash_core::ToolOutcome::ok(serde_json::json!({"ok":true})).into()
    }
}

/// TOOL-LIVE-TIMING: selected transport timing controls delivery and sealed
/// slot retirement, while timeout accounting cannot change a SQLite tool's
/// outcome. Retirement does not end retained-handle omission attribution.
#[test]
fn selected_write_timeout_and_completed_linger_govern_delivery_without_changing_outcomes()
-> Result<()> {
    use lash_core::ToolLivePolicy;
    use std::time::Duration;
    for (write_timeout, failures) in [(10, 0), (1, 1)] {
        // Each case owns its clock. Resuming a paused runtime retains its
        // virtual offset, which must not reach another SQLite node's duties.
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let case = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("timing-law runtime");
            let result: Result<()> = runtime.block_on(async {
                let policy = ToolLivePolicy::new(
                    64,
                    16,
                    524288,
                    65536,
                    1024,
                    128,
                    Duration::from_millis(100),
                    Duration::from_secs(write_timeout),
                    Duration::from_secs(3),
                )
                .unwrap();
                let replay = Arc::new(SlowToolReplay {
                    sessions: lash_core::facade_support::InMemoryLiveReplayStore::new(
                        lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
                    ),
                    tools: lash_core::ToolLiveMemoryWindow::default(),
                    first: std::sync::atomic::AtomicBool::new(true),
                    entered: tokio::sync::Notify::new(),
                    release: tokio::sync::Notify::new(),
                });
                let live = Arc::new(StdMutex::new(None));
                let calls = Arc::new(AtomicUsize::new(0));
                let results = Arc::new(StdMutex::new(Vec::new()));
                let observed = results.clone();
                let provider = lash::testing::TestProvider::builder()
                    .kind("embed-test")
                    .requires_streaming(true)
                    .complete(move |request| {
                        let call = calls.fetch_add(1, Ordering::SeqCst);
                        for block in request
                            .messages
                            .iter()
                            .flat_map(|message| message.blocks.iter())
                        {
                            if let lash_core::llm::types::LlmContentBlock::ToolResult {
                                content,
                                ..
                            } = block
                            {
                                observed.lock_recover().push(
                                    lash_llm_transport::tool_result_text(content).into_owned(),
                                );
                            }
                        }
                        async move {
                            let part = if call == 0 {
                                LlmOutputPart::ToolCall {
                                    call_id: "timed-tool".into(),
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
                .live_replay_store(Arc::new(SlowSessionReplay(replay.clone())))
                .tool_live_policy(policy)
                .serve_test_llm_profile(provider, mock_llm_profile_spec())
                .tools(
                    lash::tool_provider_key!("tools"),
                    Arc::new(TimedLiveTool(live.clone())),
                )
                .build(lash_facade_tests::runtime_lease_owner())?;
                let session = created_session(&core, SessionId::from("timed-tool-live"))
                    .await
                    .open()
                    .await?;
                session
                    .send(TurnInput::text("run the timed tool"))
                    .output()
                    .await?;
                let live = live.lock_recover().clone().expect("tool executed");
                // Stop wall-clock node duties before testing the retained publisher
                // on a virtual clock. Tool-live flushes have their own lifetime.
                core.shutdown().await?;
                tokio::time::pause();
                replay.entered.notified().await;
                let mut feed = core
                    .tool_live()
                    .subscribe(ToolLiveFilter::Call(live.address().clone()))
                    .await
                    .unwrap();
                replay.release.notify_one();
                let (mut gauge, mut log, mut failed_log) = (false, false, 0);
                while !gauge || (failures == 0 && !log) || (failures != 0 && failed_log == 0) {
                    let ToolLiveItem::Emission { emission, .. } =
                        tokio::time::timeout(Duration::from_secs(30), feed.next())
                            .await
                            .expect("tool-live emission within 30 virtual seconds")
                            .unwrap()
                            .unwrap()
                    else {
                        continue;
                    };
                    assert_eq!(
                        live.diagnostics().failed_writes,
                        failures,
                        "the selected write bound must control the flusher"
                    );
                    match emission.payload {
                        ToolLivePayload::Fact {
                            fact: ToolLiveFact::Progress { progress, .. },
                        } => {
                            assert_eq!(progress, 1.0);
                            gauge = true;
                        }
                        ToolLivePayload::Fact {
                            fact: ToolLiveFact::Log { .. },
                        } => log = true,
                        ToolLivePayload::Omitted {
                            cause: ToolLiveOmission::StoreFailed,
                            count,
                        } => failed_log += count,
                        other => panic!("unexpected {other:?}"),
                    }
                }
                assert_eq!(
                    live.diagnostics().failed_writes,
                    failures,
                    "the selected write bound must control the flusher"
                );
                assert_eq!(log, failures == 0);
                assert_eq!(failed_log, failures);
                assert_eq!(
                    live.diagnostics().policy.write_timeout,
                    policy.write_timeout()
                );
                assert_eq!(
                    live.diagnostics().policy.completion_linger,
                    policy.completed_linger()
                );
                assert_eq!(
                    results
                        .lock_recover()
                        .iter()
                        .map(|text| serde_json::from_str::<serde_json::Value>(text).unwrap())
                        .collect::<Vec<_>>(),
                    [serde_json::json!({"ok": true})]
                );
                // A fresh publisher starts on the paused clock's interval grid,
                // so the retirement comparison can exercise the exact boundary.
                let lane = lash_core::ToolLive::new(
                    Some(Arc::new(lash_core::ToolLiveMemoryWindow::default())),
                    None,
                    policy,
                );
                let live = lane.publisher_for_test(live.address().clone());
                let mut feed = lane
                    .subscribe(ToolLiveFilter::Call(live.address().clone()))
                    .await
                    .unwrap();
                live.publish(ToolLiveFact::Progress {
                    progress: 1.0,
                    total: None,
                    message: None,
                });
                live.finish();
                assert!(matches!(
                    tokio::time::timeout(Duration::from_secs(30), feed.next())
                        .await
                        .expect("tool-live emission within 30 virtual seconds")
                        .unwrap()
                        .unwrap(),
                    ToolLiveItem::Emission { .. }
                ));
                tokio::time::advance(Duration::from_secs(3) - Duration::from_millis(10)).await;
                tokio::task::yield_now().await;
                assert_eq!(
                    live.diagnostics().sealed_calls,
                    1,
                    "slot lingers strictly before the selected boundary"
                );
                tokio::time::advance(Duration::from_millis(10)).await;
                tokio::task::yield_now().await;
                assert_eq!(
                    live.diagnostics().sealed_calls,
                    0,
                    "slot retires at the selected boundary"
                );
                live.publish(ToolLiveFact::Progress {
                    progress: 2.0,
                    total: None,
                    message: None,
                });
                let ToolLiveItem::Emission { emission, .. } =
                    tokio::time::timeout(Duration::from_secs(30), feed.next())
                        .await
                        .expect("tool-live emission within 30 virtual seconds")
                        .unwrap()
                        .unwrap()
                else {
                    panic!("late omission");
                };
                assert_eq!(emission.address, *live.address());
                assert_eq!(
                    emission.payload,
                    ToolLivePayload::Omitted {
                        cause: ToolLiveOmission::AfterCompletion,
                        count: 1
                    }
                );
                Ok(())
            });
            send.send(result).expect("timing-law receiver");
        });
        let result = receive
            .recv_timeout(Duration::from_secs(30))
            .expect("timing law completes within 30 wall-clock seconds");
        case.join().expect("timing-law thread");
        result?;
    }
    Ok(())
}
