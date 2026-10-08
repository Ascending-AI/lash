//! Session observation laws on the durable substrate (FIG-5307): SQLite
//! memory stores, the core's node serving the session.

use super::*;
use futures_util::StreamExt as _;
use lash_sansio::sync::LockResultExt as _;

#[path = "observation_recovery.rs"]
mod recovery;

use recovery::observation_assistant_delta;
fn bid() -> lash_core::llm::types::StreamBlockIdentity {
    lash_core::llm::types::StreamBlockIdentity::new("text:0", 0)
}

/// A live replay store that counts the commits published into it: a node
/// publishes a turn's `Committed` after the turn's commit is acknowledged,
/// so a law that reads the replay after a send waits for its publication.
struct PublishedCommits {
    inner: Arc<dyn lash_core::LiveReplayStore>,
    commits: tokio::sync::watch::Sender<usize>,
}

impl std::fmt::Debug for PublishedCommits {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PublishedCommits")
            .field("commits", &*self.commits.borrow())
            .finish_non_exhaustive()
    }
}

impl PublishedCommits {
    fn over(inner: Arc<dyn lash_core::LiveReplayStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            commits: tokio::sync::watch::channel(0).0,
        })
    }

    fn unbounded() -> Arc<Self> {
        Self::over(Arc::new(
            lash_core::facade_support::InMemoryLiveReplayStore::default(),
        ))
    }

    /// Wait until `count` commits carrying rows were published.
    async fn published(&self, count: usize) {
        let mut commits = self.commits.subscribe();
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            commits.wait_for(|published| *published >= count),
        )
        .await
        .expect("the commit reaches the live replay")
        .expect("the store outlives the wait");
    }
}

#[async_trait::async_trait]
impl lash_core::LiveReplayStore for PublishedCommits {
    async fn publish(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
        events: Vec<lash_core::LiveReplayEventDraft>,
    ) -> std::result::Result<
        Vec<Arc<lash_core::SessionObservationEvent>>,
        lash_core::LiveReplayStoreError,
    > {
        let commits = events
            .iter()
            .filter(|event| {
                matches!(
                    &event.payload,
                    lash_core::SessionObservationEventPayload::Committed { entries: rows, .. }
                        if !rows.is_empty()
                )
            })
            .count();
        let published = self.inner.publish(session_id, revision, events).await?;
        self.commits.send_modify(|count| *count += commits);
        Ok(published)
    }

    async fn replay_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplayOutcome, lash_core::LiveReplayStoreError> {
        self.inner.replay_after_cursor(cursor).await
    }

    async fn subscribe_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplaySubscribeOutcome, lash_core::LiveReplayStoreError>
    {
        self.inner.subscribe_after_cursor(cursor).await
    }

    fn current_cursor(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.inner.current_cursor(session_id, revision)
    }

    async fn invalidate_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.invalidate_session(session_id).await
    }

    async fn trim_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.trim_session(session_id).await
    }
}

/// A standard core over SQLite memory stores publishing to `live`.
async fn core_publishing_to(live: Arc<PublishedCommits>) -> Result<LashCore> {
    explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .live_replay_store(live)
    .build(crate::testing::runtime_lease_owner())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_reasoning_part_does_not_republish_streamed_summary() -> Result<()> {
    let streamed_reasoning = LlmOutputPart::Reasoning {
        text: "**Planning single file search step**".to_string(),
        replay: Some(lash_core::llm::types::ProviderReasoningReplay {
            item_id: Some("reasoning-streamed".to_string()),
            encrypted_content: Some("opaque-streamed".to_string()),
            summary: vec!["**Planning single file search step**".to_string()],
            ..Default::default()
        }),
    };
    let completed_only_reasoning = LlmOutputPart::Reasoning {
        text: "**Completed-only summary**".to_string(),
        replay: Some(lash_core::llm::types::ProviderReasoningReplay {
            item_id: Some("reasoning-completed-only".to_string()),
            encrypted_content: Some("opaque-completed-only".to_string()),
            summary: vec!["**Completed-only summary**".to_string()],
            ..Default::default()
        }),
    };
    let provider = crate::testing::TestProvider::builder()
        .kind("reasoning-delta-then-completed-part")
        .requires_streaming(true)
        .complete(move |request| {
            let streamed_reasoning = streamed_reasoning.clone();
            let completed_only_reasoning = completed_only_reasoning.clone();
            async move {
                let stream = request.stream_events.expect("stream events");
                let block = lash_core::llm::types::StreamBlockIdentity::new(
                    "reasoning-streamed:summary:0",
                    0,
                )
                .with_item_id(Some("reasoning-streamed".to_string()));
                stream.send(LlmStreamEvent::ReasoningDelta {
                    block: block.clone(),
                    text: "**Planning single ".to_string(),
                });
                stream.send(LlmStreamEvent::ReasoningDelta {
                    block,
                    text: "file search step**".to_string(),
                });
                stream.send(LlmStreamEvent::Part(streamed_reasoning.clone()));
                stream.send(LlmStreamEvent::Part(completed_only_reasoning.clone()));
                stream.send(LlmStreamEvent::Delta {
                    block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                    text: "done".to_string(),
                });
                Ok(LlmResponse {
                    parts: vec![
                        streamed_reasoning,
                        completed_only_reasoning,
                        LlmOutputPart::Text {
                            text: "done".to_string(),
                            response_meta: None,
                        },
                    ],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(
            crate::SessionId::parse("reasoning-single-publication")
                .expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;

    let output = session
        .send(TurnInput::text("run one command"))
        .output()
        .await?;

    let reasoning = output
        .activities
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::ReasoningDelta { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        reasoning,
        vec![
            "**Planning single ",
            "file search step**",
            "**Completed-only summary**",
        ],
        "incremental chunks stay distinct, their completed snapshot is not republished, and a completed-only summary remains visible",
    );

    // The durable head the turn committed.
    let read_view = session.observe().snapshot().await?.read_view;
    let durable_reasoning = read_view
        .messages()
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter(|part| matches!(part.kind(), lash_core::PartKind::Reasoning))
        .map(|part| part.content())
        .collect::<Vec<_>>();
    assert_eq!(
        durable_reasoning,
        vec![
            "**Planning single file search step**",
            "**Completed-only summary**",
        ],
        "completed reasoning parts remain authoritative durable response state",
    );
    Ok(())
}

fn reasoning_output_part(text: &str, item_id: &str) -> LlmOutputPart {
    LlmOutputPart::Reasoning {
        text: text.to_string(),
        replay: Some(lash_core::llm::types::ProviderReasoningReplay {
            item_id: Some(item_id.to_string()),
            encrypted_content: Some(format!("opaque-{item_id}")),
            summary: vec![text.to_string()],
            ..Default::default()
        }),
    }
}

fn reasoning_activities(output: &crate::turn::TurnOutput) -> Vec<&str> {
    output
        .activities
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::ReasoningDelta { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn semantic_publication_reasoning_then_tool_does_not_repeat_reasoning() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("reasoning-tool-publication")
        .requires_streaming(true)
        .complete({
            let calls = Arc::clone(&calls);
            move |request| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    let stream = request.stream_events.expect("stream events");
                    match call {
                        0 => {
                            let reasoning = reasoning_output_part("inspect once", "reasoning-tool");
                            let tool = LlmOutputPart::ToolCall {
                                call_id: "lookup-once".to_string(),
                                tool_name: "app_lookup".to_string(),
                                input_json: "{}".to_string(),
                                replay: None,
                            };
                            stream.send(LlmStreamEvent::ReasoningDelta {
                                block: lash_core::llm::types::StreamBlockIdentity::new(
                                    "reasoning-tool:summary:0",
                                    0,
                                )
                                .with_item_id(Some("reasoning-tool".to_string())),
                                text: "inspect once".to_string(),
                            });
                            stream.send(LlmStreamEvent::Part(reasoning.clone()));
                            stream.send(LlmStreamEvent::Part(tool.clone()));
                            Ok(LlmResponse {
                                parts: vec![reasoning, tool],
                                response_metadata: Default::default(),
                                ..LlmResponse::default()
                            })
                        }
                        1 => {
                            stream.send(LlmStreamEvent::Delta {
                                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                                text: "done".to_string(),
                            });
                            Ok(text_response("done"))
                        }
                        _ => panic!("unexpected provider call {call}"),
                    }
                }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .tools(Arc::new(AppTools))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(
            crate::SessionId::parse("reasoning-tool-publication").expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;

    let output = session
        .send(TurnInput::text("inspect with a tool"))
        .output()
        .await?;

    assert_eq!(reasoning_activities(&output), vec!["inspect once"]);
    assert_eq!(output.assistant_message(), Some("done"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn semantic_publication_streamed_reasoning_keeps_nonstreamed_text() -> Result<()> {
    let reasoning = reasoning_output_part("reasoning once", "reasoning-before-text");
    let provider = crate::testing::TestProvider::builder()
        .kind("reasoning-then-buffered-text")
        .requires_streaming(true)
        .complete(move |request| {
            let reasoning = reasoning.clone();
            async move {
                let stream = request.stream_events.expect("stream events");
                stream.send(LlmStreamEvent::ReasoningDelta {
                    block: lash_core::llm::types::StreamBlockIdentity::new(
                        "reasoning-before-text:summary:0",
                        0,
                    )
                    .with_item_id(Some("reasoning-before-text".to_string())),
                    text: "reasoning once".to_string(),
                });
                stream.send(LlmStreamEvent::Part(reasoning.clone()));
                Ok(LlmResponse {
                    parts: vec![
                        reasoning,
                        LlmOutputPart::Text {
                            text: "buffered answer".to_string(),
                            response_meta: None,
                        },
                    ],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(
            crate::SessionId::parse("reasoning-buffered-text").expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;

    let output = session
        .send(TurnInput::text("answer after reasoning"))
        .output()
        .await?;

    assert_eq!(reasoning_activities(&output), vec!["reasoning once"]);
    assert_eq!(assistant_prose(&output.activities), "buffered answer");
    assert_eq!(output.assistant_message(), Some("buffered answer"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn semantic_publication_preserves_identical_completed_reasoning_parts_and_turns() -> Result<()>
{
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("identical-reasoning-publication")
        .requires_streaming(true)
        .complete({
            let calls = Arc::clone(&calls);
            move |_request| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok(LlmResponse {
                        parts: vec![
                            reasoning_output_part("repeat legitimately", &format!("{call}-a")),
                            reasoning_output_part("repeat legitimately", &format!("{call}-b")),
                        ],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(
            crate::SessionId::parse("identical-reasoning-publication")
                .expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;

    for prompt in ["first turn", "second turn"] {
        let output = session.send(TurnInput::text(prompt)).output().await?;
        assert_eq!(
            reasoning_activities(&output),
            vec!["repeat legitimately", "repeat legitimately"]
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[cfg(feature = "rlm")]
fn output_then_failing_rlm_prose_provider(
    transport_calls: Arc<AtomicUsize>,
    requests: Arc<StdMutex<Vec<lash_core::LlmRequest>>>,
) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("retrying-rlm-prose")
        .requires_streaming(true)
        .options(lash_core::facade_support::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..lash_core::facade_support::ProviderOptions::default()
        })
        .complete(move |request| {
            let transport_calls = Arc::clone(&transport_calls);
            let requests = Arc::clone(&requests);
            async move {
                requests
                    .lock_recover()
                    .push(request.clone());
                let call = transport_calls.fetch_add(1, Ordering::SeqCst);
                let stream = request.stream_events.expect("stream events");
                if call == 0 {
                    stream.send(LlmStreamEvent::Delta {
                        block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                        text: "retry observer single-copy marker\n<typescript>\n".to_string(),
                    });
                    return Err(
                        LlmTransportError::new("deterministic rate limit")
                            .with_http_status(429)
                            .with_output_started(true),
                    );
                }
                let text = match call {
                    1 => {
                        "retry observer single-copy marker\n<typescript>\nretry_missing_name;\n</typescript>"
                    }
                    2 => "<typescript>\nfinish(\"provider retry succeeded\");\n</typescript>",
                    _ => "<typescript>\nfinish(\"subsequent turn succeeded\");\n</typescript>",
                };
                stream.send(LlmStreamEvent::Delta { block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0), text: text.to_string() });
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: text.to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

#[cfg(feature = "rlm")]
fn natural_prose_reasoning_provider(
    requests: Arc<StdMutex<Vec<lash_core::LlmRequest>>>,
) -> ProviderHandle {
    let calls = Arc::new(AtomicUsize::new(0));
    crate::testing::TestProvider::builder()
        .kind("natural-rlm-prose-reasoning")
        .complete(move |request| {
            let requests = Arc::clone(&requests);
            let calls = Arc::clone(&calls);
            async move {
                requests.lock_recover().push(request);
                let call = calls.fetch_add(1, Ordering::SeqCst);
                let text = match call {
                    0 => "natural completion single-copy marker",
                    1 => "subsequent natural answer",
                    other => panic!("unexpected natural RLM provider request {other}"),
                };
                let mut parts = Vec::new();
                if call == 0 {
                    parts.push(LlmOutputPart::Reasoning {
                        text: "reasoning retained for replay".to_string(),
                        replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                            item_id: Some("natural-reasoning".to_string()),
                            encrypted_content: Some("opaque-natural-replay".to_string()),
                            signature: None,
                            redacted: false,
                            summary: Vec::new(),
                            ..Default::default()
                        }),
                    });
                }
                parts.push(LlmOutputPart::Text {
                    text: text.to_string(),
                    response_meta: None,
                });
                Ok(LlmResponse {
                    parts,
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

#[cfg(feature = "rlm")]
fn provider_request_text(request: &lash_core::LlmRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rlm_provider_failure_after_prose_is_not_retried_or_committed() -> Result<()> {
    const MARKER: &str = "retry observer single-copy marker";
    let transport_calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core =
        explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
            .serve_test_llm_profile(
                output_then_failing_rlm_prose_provider(
                    Arc::clone(&transport_calls),
                    Arc::clone(&requests),
                ),
                mock_llm_profile_spec(),
            )
            .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(
            crate::SessionId::parse("rlm-provider-retry-prose").expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;

    let first = session
        .send(TurnInput::text("trigger deterministic rate limit retry"))
        .output()
        .await?;

    assert_eq!(
        transport_calls.load(Ordering::SeqCst),
        1,
        "provider output must not be re-bought after the failed attempt"
    );
    assert!(matches!(
        first.result.outcome,
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::ProviderError)
    ));
    assert!(first.activities.iter().any(|activity| matches!(
        &activity.event,
        TurnEvent::AssistantProseDelta { text, .. } if text.contains(MARKER)
    )));
    assert!(
        first
            .activities
            .iter()
            .all(|activity| !matches!(activity.event, TurnEvent::ModelAttemptReset { .. }))
    );
    let rlm_marker_records = first
        .result
        .state
        .read_view()
        .active_events()
        .iter()
        .filter(|record| match record {
            lash_core::SessionHistoryRecord::Conversation(message) => {
                matches!(
                    message.origin.as_ref(),
                    Some(lash_core::MessageOrigin::Plugin {
                        plugin_id,
                        transient: false,
                    }) if plugin_id == lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID
                ) && message
                    .parts
                    .iter()
                    .any(|part| part.content().contains(MARKER))
            }
            lash_core::SessionHistoryRecord::Protocol(event) => matches!(
                lash_protocol_rlm::decode_rlm_protocol_event(event).expect("recorded RLM protocol event decodes"),
                Some(lash_rlm_types::RlmProtocolEvent::RlmAssistantContent(content))
                    if content.prose.contains(MARKER)
            ),
        })
        .count();
    assert_eq!(
        rlm_marker_records, 0,
        "failed-attempt prose is preview output, not committed RLM history"
    );
    {
        let requests = requests.lock_recover();
        assert_eq!(requests.len(), 1, "RLM scheduled a spurious iteration");
    }
    assert_eq!(first.result.llm_calls.len(), 1);
    assert_eq!(first.result.llm_calls[0].attempts.len(), 1);
    let attempt = &first.result.llm_calls[0].attempts[0];
    assert_eq!(
        attempt.protocol_position,
        lash_core::ProtocolPosition::OutputStarted
    );
    assert_eq!(
        attempt
            .retry_decision
            .as_ref()
            .map(|decision| decision.is_scheduled()),
        Some(false)
    );
    assert_eq!(
        attempt
            .retry_decision
            .as_ref()
            .and_then(|decision| decision.denial_reason()),
        Some(lash_sansio::llm::types::ChargeSafetyDenialReason::GuaranteeRequired)
    );
    // The durable report is rebuilt from the store: the typed refusal is
    // the turn's committed failure evidence.
    let evidence = session
        .durable()
        .failure_evidence(None, std::num::NonZeroU32::MIN.saturating_add(7))
        .await?;
    let [settlement] = evidence.settlements.as_slice() else {
        panic!("one failed turn settles its evidence: {evidence:?}");
    };
    let [failure] = settlement.evidence.as_slice() else {
        panic!("one refused generation: {settlement:?}");
    };
    assert_eq!(
        failure.refusal.code(),
        crate::turn::TurnFailureCode::UnsafeRetryAfterOutputStarted.into()
    );
    assert_eq!(
        failure.refusal.denial_reason,
        lash_sansio::llm::types::ChargeSafetyDenialReason::GuaranteeRequired
    );

    let reopened = core
        .session(
            crate::SessionId::parse("rlm-provider-retry-prose").expect("nonblank host identity"),
        )
        .open()
        .await?;
    assert_eq!(
        reopened
            .read_view()
            .active_events()
            .iter()
            .filter(|record| match record {
                lash_core::SessionHistoryRecord::Conversation(message) => message
                    .parts
                    .iter()
                    .any(|part| part.content().contains(MARKER)),
                lash_core::SessionHistoryRecord::Protocol(event) => matches!(
                    lash_protocol_rlm::decode_rlm_protocol_event(event).expect("recorded RLM protocol event decodes"),
                    Some(lash_rlm_types::RlmProtocolEvent::RlmAssistantContent(content))
                        if content.prose.contains(MARKER)
                ),
            })
            .count(),
        0,
        "reloaded history retained failed-attempt prose"
    );
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rlm_natural_prose_completion_is_single_copy_in_next_request() -> Result<()> {
    const MARKER: &str = "natural completion single-copy marker";
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core =
        explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
            .serve_test_llm_profile(
                natural_prose_reasoning_provider(Arc::clone(&requests)),
                mock_llm_profile_spec(),
            )
            .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(
            crate::SessionId::parse("rlm-natural-prose-single-copy")
                .expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;

    let first = session
        .send(TurnInput::text("answer naturally"))
        .output()
        .await?;
    assert_eq!(first.assistant_message(), Some(MARKER));
    Box::pin(session.admin().state().append_messages(vec![
            lash_core::PluginMessage::text(lash_core::MessageRole::Assistant, MARKER)
                .with_id("workbench-assistant:natural-turn"),
        ]))
    .await?;

    session
        .send(TurnInput::text("check natural completion history"))
        .output()
        .await?;

    let requests = requests.lock_recover();
    assert_eq!(requests.len(), 2);
    let next_request = provider_request_text(&requests[1]);
    assert_eq!(
        next_request.matches(MARKER).count(),
        1,
        "provider-visible history duplicated natural prose: {next_request}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_observation_envelopes_scope_activity_and_commit_to_the_turn() -> Result<()> {
    let live = PublishedCommits::unbounded();
    let core = core_publishing_to(Arc::clone(&live)).await?;
    let session = core
        .session(
            crate::SessionId::parse("session-observation-turn-identity")
                .expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;
    let cursor = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;

    session
        .send(TurnInput::text("identify this turn"))
        .id(crate::TurnId::parse("observation-turn").expect("nonblank host identity"))
        .output()
        .await?;
    live.published(1).await;

    let lash_core::facade_support::SessionResume::Replayed { events } =
        session.observe().resume_from_cursor(&cursor).await?
    else {
        panic!("fresh turn observation cursor should remain replayable");
    };
    let turn_activity = events
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                lash_core::SessionObservationEventPayload::TurnActivity(_)
            )
        })
        .collect::<Vec<_>>();
    assert!(!turn_activity.is_empty(), "turn emitted no activity");
    let lash_core::SessionObservationEventPayload::TurnActivity(first_activity) =
        &turn_activity[0].payload
    else {
        unreachable!("turn_activity contains only activity payloads");
    };
    assert!(
        matches!(
            &first_activity.event,
            TurnEvent::TurnStarted { turn_id } if turn_id == "observation-turn"
        ),
        "replay must begin with the identity event, got {:?}",
        first_activity.event
    );
    assert!(
        turn_activity
            .iter()
            .all(|event| event.turn_id.as_deref() == Some("observation-turn")),
        "every turn activity must carry its producing turn identity"
    );
    let committed = events
        .iter()
        .find(|event| {
            matches!(
                &event.payload,
                lash_core::SessionObservationEventPayload::Committed { entries: rows, .. } if !rows.is_empty()
            )
        })
        .expect("turn commit observation");
    assert_eq!(committed.turn_id.as_deref(), Some("observation-turn"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "FIG-5322: under load the gap's replacement cursor answers another gap instead of the unseen auxiliary event"]
async fn trimmed_gap_replacement_cursor_preserves_unseen_auxiliary_event() -> Result<()> {
    let live = PublishedCommits::over(Arc::new(
        lash_core::facade_support::InMemoryLiveReplayStore::new(
            lash_core::facade_support::InMemoryLiveReplayStoreConfig {
                max_events_per_session: 1,
                ..lash_core::facade_support::InMemoryLiveReplayStoreConfig::default()
            },
        ),
    ));
    let core = core_publishing_to(Arc::clone(&live)).await?;
    let session = core
        .session(
            crate::SessionId::parse("trimmed-gap-unseen-auxiliary-event")
                .expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;
    let stale_cursor = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;

    session
        .send(TurnInput::text("install replacement projection"))
        .output()
        .await?;
    live.published(1).await;
    let installed_projection = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot");
    session
        .observe()
        .runtime
        .record_queue_changed(
            lash_core::SessionQueueEventKind::Enqueued,
            vec!["unseen-batch".to_string()],
        )
        .await;

    let SessionResume::Gap { gap, .. } =
        session.observe().resume_from_cursor(&stale_cursor).await?
    else {
        panic!("the trimmed cursor must yield a replacement gap");
    };
    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Trimmed);
    assert_eq!(
        gap.latest_cursor
            .parse_for_session(&session.session_id())
            .expect("the gap's cursor names its session")
            .revision,
        installed_projection
            .cursor
            .parse_for_session(&session.session_id())
            .expect("the projection's cursor names its session")
            .revision,
        "the replacement is the projection's durable head"
    );

    let SessionResume::Replayed { events } = session
        .observe()
        .resume_from_cursor(&gap.latest_cursor)
        .await?
    else {
        panic!("the replacement cursor must retain a replayable auxiliary suffix");
    };
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0].payload,
        lash_core::SessionObservationEventPayload::QueueChanged { kind, batch_ids }
            if *kind == lash_core::SessionQueueEventKind::Enqueued
                && batch_ids == &["unseen-batch"]
    ));
    Ok(())
}

#[derive(Debug)]
struct PausedCommitReplayStore {
    inner: lash_core::facade_support::InMemoryLiveReplayStore,
    boundary: PublicationBoundary,
    pause: Arc<PublicationPause>,
}

#[derive(Debug)]
struct PublicationPause {
    boundary_reached: std::sync::atomic::AtomicBool,
    release_boundary: std::sync::atomic::AtomicBool,
    pause_lock: StdMutex<()>,
    pause_changed: std::sync::Condvar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublicationBoundary {
    /// Before the store assigns the commit's batch its positions.
    BeforePublish,
    /// After the batch is replay-visible, before its subscribers hear it.
    BeforeNotification,
}

#[derive(Debug)]
struct FailingAppendReplayStore {
    inner: lash_core::facade_support::InMemoryLiveReplayStore,
}

impl FailingAppendReplayStore {
    fn new() -> Self {
        Self {
            inner: lash_core::facade_support::InMemoryLiveReplayStore::default(),
        }
    }
}

#[async_trait::async_trait]
impl lash_core::LiveReplayStore for FailingAppendReplayStore {
    async fn publish(
        &self,
        _session_id: &SessionId,
        _revision: lash_core::SessionRevision,
        _events: Vec<lash_core::LiveReplayEventDraft>,
    ) -> std::result::Result<
        Vec<Arc<lash_core::SessionObservationEvent>>,
        lash_core::LiveReplayStoreError,
    > {
        Err(lash_core::LiveReplayStoreError::Store(
            "injected live-replay append failure".to_string(),
        ))
    }

    async fn replay_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplayOutcome, lash_core::LiveReplayStoreError> {
        self.inner.replay_after_cursor(cursor).await
    }

    async fn subscribe_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplaySubscribeOutcome, lash_core::LiveReplayStoreError>
    {
        self.inner.subscribe_after_cursor(cursor).await
    }

    fn current_cursor(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.inner.current_cursor(session_id, revision)
    }

    async fn invalidate_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.invalidate_session(session_id).await
    }

    async fn trim_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.trim_session(session_id).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_revision_requires_replacement_evidence() -> Result<()> {
    let replay_store = Arc::new(FailingAppendReplayStore::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .live_replay_store(replay_store)
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(
            crate::SessionId::parse("failed-commit-observation-reconciliation")
                .expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;
    let before = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot");

    let output = session
        .send(TurnInput::text("commit despite replay failure"))
        .output()
        .await?;
    assert_eq!(
        output.assistant_message(),
        Some("echo: commit despite replay failure"),
        "the durable turn must still commit"
    );

    // The turn's commit is the durable head's revision.
    let head = session
        .observe()
        .snapshot()
        .await?
        .cursor
        .parse_for_session(&session.session_id())
        .expect("the head's cursor names its session")
        .revision;
    let before_revision = before
        .cursor
        .parse_for_session(&session.session_id())
        .expect("the snapshot's cursor names its session")
        .revision;
    assert!(head > before_revision, "the turn committed a new revision");
    let SessionResume::Gap { observation, gap } =
        session.observe().resume_from_cursor(&before.cursor).await?
    else {
        panic!("a pre-commit cursor without replacement evidence must not replay cleanly");
    };
    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Unavailable);
    assert_eq!(gap.latest_revision, head);
    assert_eq!(gap.requested_cursor, before.cursor);
    assert_eq!(gap.latest_cursor, observation.cursor);
    assert_ne!(
        gap.latest_cursor, before.cursor,
        "the unchanged live position must still carry the new durable revision"
    );

    let SessionObservationSubscription::Gap { observation, gap } = session
        .observe()
        .subscribe_from_cursor(&before.cursor)
        .await?
    else {
        panic!("a pre-commit cursor without replacement evidence must not subscribe cleanly");
    };
    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Unavailable);
    assert_eq!(gap.latest_revision, head);
    assert_eq!(gap.requested_cursor, before.cursor);
    assert_eq!(gap.latest_cursor, observation.cursor);
    assert_ne!(
        gap.latest_cursor, before.cursor,
        "subscribe must adopt the revision-stamped replacement cursor"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_session_reconnect_after_failed_append_yields_gap_without_another_commit() -> Result<()>
{
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .live_replay_store(Arc::new(FailingAppendReplayStore::new()))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(
            crate::SessionId::parse("idle-failed-commit-observation-reconciliation")
                .expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;
    let cursor = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;

    session
        .send(TurnInput::text("commit before becoming idle"))
        .output()
        .await?;

    let mut reconnect = session.observe().subscribe_and_recover(cursor);
    let item = tokio::time::timeout(std::time::Duration::from_millis(250), reconnect.next())
        .await
        .expect("an idle reconnect must not wait for a future commit")
        .expect("recovery stream remains open")?;
    assert!(matches!(
        item,
        crate::observe::SessionObservationStreamItem::Gap {
            gap: lash_core::facade_support::LiveReplayGap {
                reason: lash_core::LiveReplayGapReason::Unavailable,
                ..
            },
            ..
        }
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_subscribe_has_only_two_histories() -> Result<()> {
    for boundary in [
        PublicationBoundary::BeforePublish,
        PublicationBoundary::BeforeNotification,
    ] {
        let replay_store = Arc::new(PausedCommitReplayStore::at(boundary));
        let core = explicit_ephemeral_facets(LashCore::standard_builder(
            sqlite_memory_store_backend().await,
        ))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .live_replay_store(replay_store.clone())
        .build(crate::testing::runtime_lease_owner())?;
        let session_id = SessionId::fixture(format!("two-histories-{boundary:?}"));
        let session = core.session(session_id).created().await.open().await?;
        let before = session
            .observe()
            .snapshot()
            .await
            .expect("durable snapshot");
        let turn_session = session.clone();
        let turn = tokio::spawn(async move {
            turn_session
                .send(TurnInput::text("exactly once across the cut"))
                .output()
                .await
        });

        replay_store.wait_for_commit_append().await;
        let batch_is_visible = match lash_core::LiveReplayStore::replay_after_cursor(
            replay_store.as_ref(),
            &before.cursor,
        )
        .await
        .expect("boundary visibility probe must read replay")
        {
            lash_core::LiveReplayOutcome::Replayed(events) => events.iter().any(|event| {
                matches!(
                    &event.payload,
                    lash_core::SessionObservationEventPayload::Committed { entries: rows, .. } if !rows.is_empty()
                )
            }),
            lash_core::LiveReplayOutcome::Gap(reason) => {
                panic!("{boundary:?}: boundary probe unexpectedly gapped: {reason:?}")
            }
        };
        assert_eq!(
            batch_is_visible,
            boundary == PublicationBoundary::BeforeNotification,
            "{boundary:?}: only the pre-notification cut may expose the batch to replay"
        );
        let snapshot = session
            .observe()
            .snapshot()
            .await
            .expect("durable snapshot");
        let snapshot_is_new = snapshot.read_view.messages().iter().any(|message| {
            crate::tests::fixtures::role_and_text(message)
                .1
                .contains("exactly once across the cut")
        });
        assert!(
            snapshot_is_new,
            "{boundary:?}: the snapshot is the durable head, which commits before it publishes"
        );
        let mut stream = session.observe().subscribe_and_recover(snapshot.cursor);
        replay_store.release_commit_install();
        turn.await.expect("join publishing turn")?;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), stream.next())
                .await
                .is_err(),
            "{boundary:?}: a snapshot of the durable head must not redeliver its publication"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incarnation_change_invalidates_cursor() {
    let original = crate::observe::InMemoryLiveReplayStore::default();
    let preserved = crate::observe::InMemoryLiveReplayStore::reopen_preserving_history(&original);
    lash_conformance::incarnation_change_invalidates_cursor(
        Arc::new(original),
        Arc::new(crate::observe::InMemoryLiveReplayStore::default()),
        Arc::new(preserved),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn payload_authority_matches_revision_transition() -> Result<()> {
    let live = PublishedCommits::unbounded();
    let core = core_publishing_to(Arc::clone(&live)).await?;
    let session = core
        .session(
            crate::SessionId::parse("payload-authority-transition")
                .expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;
    let initial = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot");

    session
        .send(TurnInput::text("durable transition"))
        .output()
        .await?;
    live.published(1).await;
    let committed = session
        .observe()
        .resume_from_cursor(&initial.cursor)
        .await?;
    let SessionResume::Replayed { events } = committed else {
        panic!("durable transition must replay its committed evidence");
    };
    let committed = events
        .iter()
        .find(|event| {
            matches!(
                &event.payload,
                lash_core::SessionObservationEventPayload::Committed { entries: rows, .. } if !rows.is_empty()
            )
        })
        .expect("durable transition emitted Committed");
    let lash_core::SessionObservationEventPayload::Committed {
        base_revision,
        entries: rows,
    } = &committed.payload
    else {
        unreachable!()
    };
    assert_eq!(
        *base_revision,
        initial
            .cursor
            .parse_for_session(&session.session_id())
            .expect("the snapshot's cursor names its session")
            .revision,
        "the commit's rows extend the revision the snapshot held"
    );
    assert!(!rows.is_empty(), "the commit carries the rows it added");
    assert!(
        committed.revision() > *base_revision,
        "the commit's revision follows the one its rows extend"
    );

    Ok(())
}

impl PausedCommitReplayStore {
    fn at(boundary: PublicationBoundary) -> Self {
        let pause = Arc::new(PublicationPause {
            boundary_reached: std::sync::atomic::AtomicBool::new(false),
            release_boundary: std::sync::atomic::AtomicBool::new(false),
            pause_lock: StdMutex::new(()),
            pause_changed: std::sync::Condvar::new(),
        });
        let inner = if boundary == PublicationBoundary::BeforeNotification {
            let notification_pause = Arc::clone(&pause);
            lash_core::facade_support::InMemoryLiveReplayStore::with_before_notification_gate_for_testing(
                lash_core::facade_support::InMemoryLiveReplayStore::default(),
                move |events| {
                    if Self::is_authoritative_events(events) {
                        notification_pause.pause();
                    }
                },
            )
        } else {
            lash_core::facade_support::InMemoryLiveReplayStore::default()
        };
        Self {
            inner,
            boundary,
            pause,
        }
    }

    fn is_authoritative_events(events: &[Arc<lash_core::SessionObservationEvent>]) -> bool {
        events.iter().any(|event| {
            matches!(
                &event.payload,
                lash_core::SessionObservationEventPayload::Committed { entries: rows, .. }
                    if !rows.is_empty()
            ) || matches!(
                &event.payload,
                lash_core::SessionObservationEventPayload::ResidentChanged
            )
        })
    }

    async fn wait_for_commit_append(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !self
                .pause
                .boundary_reached
                .load(std::sync::atomic::Ordering::Acquire)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the turn never reached its commit's publication boundary");
    }

    fn release_commit_install(&self) {
        self.pause
            .release_boundary
            .store(true, std::sync::atomic::Ordering::Release);
        self.pause.pause_changed.notify_all();
    }
}

impl PublicationPause {
    fn pause(&self) {
        self.boundary_reached
            .store(true, std::sync::atomic::Ordering::Release);
        let mut guard = self.pause_lock.lock_recover();
        while !self
            .release_boundary
            .load(std::sync::atomic::Ordering::Acquire)
        {
            guard = self.pause_changed.wait(guard).recover();
        }
    }
}

#[async_trait::async_trait]
impl lash_core::LiveReplayStore for PausedCommitReplayStore {
    async fn publish(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
        events: Vec<lash_core::LiveReplayEventDraft>,
    ) -> std::result::Result<
        Vec<Arc<lash_core::SessionObservationEvent>>,
        lash_core::LiveReplayStoreError,
    > {
        let authoritative = events.iter().any(|event| {
            matches!(
                &event.payload,
                lash_core::SessionObservationEventPayload::Committed { entries: rows, .. }
                    if !rows.is_empty()
            ) || matches!(
                &event.payload,
                lash_core::SessionObservationEventPayload::ResidentChanged
            )
        });
        if authoritative && self.boundary == PublicationBoundary::BeforePublish {
            let pause = Arc::clone(&self.pause);
            tokio::task::spawn_blocking(move || pause.pause())
                .await
                .expect("join the publication pause");
        }
        self.inner.publish(session_id, revision, events).await
    }

    async fn replay_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplayOutcome, lash_core::LiveReplayStoreError> {
        self.inner.replay_after_cursor(cursor).await
    }

    async fn subscribe_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplaySubscribeOutcome, lash_core::LiveReplayStoreError>
    {
        self.inner.subscribe_after_cursor(cursor).await
    }

    fn current_cursor(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.inner.current_cursor(session_id, revision)
    }

    async fn invalidate_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.invalidate_session(session_id).await
    }

    async fn trim_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.trim_session(session_id).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recoverable_chat_conformance_deduplicates_redelivery_identity() -> Result<()> {
    let live = PublishedCommits::unbounded();
    let core = core_publishing_to(Arc::clone(&live)).await?;
    let session = core
        .session(
            crate::SessionId::parse("recoverable-chat-redelivery").expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;
    let cursor = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;
    session
        .send(TurnInput::text("redelivery identity"))
        .id(crate::TurnId::parse("recoverable-redelivery-turn").expect("nonblank host identity"))
        .output()
        .await?;
    live.published(1).await;

    let mut first_delivery = session.observe().subscribe_and_recover(cursor.clone());
    let first_id = match first_delivery.next().await.expect("first replay event")? {
        crate::observe::SessionObservationStreamItem::Event(event) => {
            crate::observe::SessionObservationEventId::of(&event)
        }
        crate::observe::SessionObservationStreamItem::Gap { .. } => {
            panic!("fresh cursor unexpectedly gapped")
        }
    };

    let mut redelivery = session
        .observe()
        .subscribe_and_recover(cursor)
        .with_applied_event_ids([first_id.clone()]);
    let next_id = match redelivery.next().await.expect("next replay event")? {
        crate::observe::SessionObservationStreamItem::Event(event) => {
            crate::observe::SessionObservationEventId::of(&event)
        }
        crate::observe::SessionObservationStreamItem::Gap { .. } => {
            panic!("fresh cursor unexpectedly gapped")
        }
    };
    assert_ne!(
        next_id, first_id,
        "an already-applied event identity must not be delivered twice"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gap_replacement_then_continuation_after_unavailable_history() -> Result<()> {
    let session_id = "recoverable-chat-restart-cursor";
    let backend = sqlite_memory_store_backend().await;
    let bootstrap_core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;
    Box::pin(
        bootstrap_core
            .session(crate::SessionId::parse(session_id).expect("nonblank host identity"))
            .created()
            .await
            .open()
            .await?
            .close(),
    )
    .await?;
    drop(bootstrap_core);

    let first_core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .live_replay_store(Arc::new(
            lash_core::facade_support::InMemoryLiveReplayStore::default(),
        ))
        .build(crate::testing::runtime_lease_owner())?;
    let first_session = first_core
        .session(crate::SessionId::parse(session_id).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let initial_cursor = first_session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;
    first_session
        .observe()
        .runtime
        .record_turn_activity(
            Some(&TurnId::from("before-restart-turn")),
            TurnActivity::independent(TurnEvent::AssistantProseDelta {
                text: "before replay-store restart".into(),
                block: bid(),
            }),
        )
        .await;
    let mut first_stream = first_session
        .observe()
        .subscribe_and_recover(initial_cursor);
    let old_id = match first_stream.next().await.expect("pre-restart event")? {
        crate::observe::SessionObservationStreamItem::Event(event) => {
            crate::observe::SessionObservationEventId::of(&event)
        }
        other => panic!("expected pre-restart provisional event, got {other:?}"),
    };
    let old_cursor = first_stream.cursor().clone();
    drop(first_stream);
    drop(first_session);
    drop(first_core);

    let second_core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .live_replay_store(Arc::new(
            lash_core::facade_support::InMemoryLiveReplayStore::default(),
        ))
        .build(crate::testing::runtime_lease_owner())?;
    let second_session = second_core
        .session(crate::SessionId::parse(session_id).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let restarted_at = second_session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;
    let mut retained_applied_ids = second_session
        .observe()
        .subscribe_and_recover(restarted_at)
        .with_applied_event_ids([old_id.clone()]);
    let mut recovered = second_session
        .observe()
        .subscribe_and_recover(old_cursor)
        .with_applied_event_ids([old_id.clone()]);
    let gap = recovered.next().await.expect("restart gap")?;
    assert!(matches!(
        gap,
        crate::observe::SessionObservationStreamItem::Gap {
            gap: lash_core::facade_support::LiveReplayGap {
                reason: lash_core::LiveReplayGapReason::Unavailable,
                ..
            },
            ..
        }
    ));

    second_session
        .observe()
        .runtime
        .record_turn_activity(
            Some(&TurnId::from("after-restart-turn")),
            TurnActivity::independent(TurnEvent::AssistantProseDelta {
                text: "after replay-store restart".into(),
                block: bid(),
            }),
        )
        .await;
    let gap_continuation =
        tokio::time::timeout(std::time::Duration::from_millis(500), recovered.next())
            .await
            .expect("gap stream did not continue with the new event")
            .expect("recovered stream remains open")?;
    let crate::observe::SessionObservationStreamItem::Event(gap_continuation_event) =
        gap_continuation
    else {
        panic!("expected post-gap provisional event");
    };
    let gap_continuation_id =
        crate::observe::SessionObservationEventId::of(&gap_continuation_event);
    let update = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        retained_applied_ids.next(),
    )
    .await
    .expect("retained pre-restart identity incorrectly suppressed the new event")
    .expect("recovered stream remains open")?;
    let crate::observe::SessionObservationStreamItem::Event(event) = update else {
        panic!("expected post-restart provisional event");
    };
    let id = crate::observe::SessionObservationEventId::of(&event);
    assert_ne!(
        id.cursor, old_id.cursor,
        "a fresh replay-store incarnation must change the opaque cursor even at the same numeric position"
    );
    assert_ne!(
        id, old_id,
        "a fresh replay-store incarnation must distinguish a reused cursor without relying on gap clearing"
    );
    assert_eq!(gap_continuation_id, id);
    assert_eq!(
        observation_assistant_delta(&gap_continuation_event).as_deref(),
        Some("after replay-store restart")
    );
    assert_eq!(
        observation_assistant_delta(&event).as_deref(),
        Some("after replay-store restart")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gap_replacement_then_continuation_after_trimmed_history() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .live_replay_store(Arc::new(
        lash_core::facade_support::InMemoryLiveReplayStore::new(
            lash_core::facade_support::InMemoryLiveReplayStoreConfig {
                max_events_per_session: 1,
                ..lash_core::facade_support::InMemoryLiveReplayStoreConfig::default()
            },
        ),
    ))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("recoverable-chat-gap").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let cursor = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;
    session
        .send(TurnInput::text("trim the initial cursor"))
        .output()
        .await?;
    let mut stream = session.observe().subscribe_and_recover(cursor);
    let update = stream.next().await.expect("gap update")?;
    let crate::observe::SessionObservationStreamItem::Gap {
        observation: snapshot,
        gap,
    } = update
    else {
        panic!("trimmed cursor must be forwarded as a recoverable gap");
    };
    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Trimmed);
    assert_eq!(gap.latest_cursor, snapshot.cursor);

    session
        .send(TurnInput::text("live after gap"))
        .output()
        .await?;
    let continued = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match stream.next().await.expect("post-gap live update")? {
                crate::observe::SessionObservationStreamItem::Gap {
                    observation: snapshot,
                    ..
                } => {
                    break Ok::<_, crate::EmbedError>(
                        snapshot
                            .read_view
                            .messages()
                            .iter()
                            .map(|message| crate::tests::fixtures::role_and_text(message).1)
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
                crate::observe::SessionObservationStreamItem::Event(event) => {
                    if let lash_core::SessionObservationEventPayload::Committed {
                        entries: rows,
                        ..
                    } = &event.payload
                    {
                        break Ok(format!("{rows:?}"));
                    }
                }
            }
        }
    })
    .await
    .expect("post-gap continuation timeout")?;
    assert!(
        continued.contains("live after gap"),
        "continued recovery must carry the next turn"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscriber_lag_with_trimmed_suffix_forces_gap_then_continues() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .live_replay_store(Arc::new(
        lash_core::facade_support::InMemoryLiveReplayStore::new(
            lash_core::facade_support::InMemoryLiveReplayStoreConfig {
                max_events_per_session: 1,
                ..lash_core::facade_support::InMemoryLiveReplayStoreConfig::default()
            },
        ),
    ))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(
            crate::SessionId::parse("subscriber-lag-trimmed-recovery")
                .expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;
    let cursor = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;
    let mut stream = session.observe().subscribe_and_recover(cursor);
    assert!(
        futures_util::poll!(stream.next()).is_pending(),
        "the initial poll must wait for a live event"
    );

    for text in ["lag one", "lag two", "lag three"] {
        session
            .observe()
            .runtime
            .record_turn_activity(
                Some(&TurnId::from("lagged-turn")),
                TurnActivity::independent(TurnEvent::AssistantProseDelta {
                    text: text.into(),
                    block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                }),
            )
            .await;
    }

    let gap = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
        .await
        .expect("lag recovery timed out")
        .expect("lag recovery stream remains open")?;
    assert!(matches!(
        gap,
        crate::observe::SessionObservationStreamItem::Gap {
            gap: lash_core::facade_support::LiveReplayGap {
                reason: lash_core::LiveReplayGapReason::Trimmed,
                ..
            },
            ..
        }
    ));

    session
        .observe()
        .runtime
        .record_turn_activity(
            Some(&TurnId::from("after-lag-turn")),
            TurnActivity::independent(TurnEvent::AssistantProseDelta {
                text: "after lag".into(),
                block: bid(),
            }),
        )
        .await;
    let continued = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
        .await
        .expect("post-lag continuation timed out")
        .expect("post-lag stream remains open")?;
    let crate::observe::SessionObservationStreamItem::Event(event) = continued else {
        panic!("lag recovery must continue with the next live event");
    };
    assert_eq!(
        observation_assistant_delta(&event).as_deref(),
        Some("after lag")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recoverable_chat_conformance_disconnect_does_not_cancel_server_work() -> Result<()> {
    let (entered_tx, entered_rx) = oneshot::channel();
    let entered_tx = Arc::new(StdMutex::new(Some(entered_tx)));
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = crate::testing::TestProvider::builder()
        .kind("recoverable-chat-disconnect")
        .complete({
            let entered_tx = Arc::clone(&entered_tx);
            let release = Arc::clone(&release);
            move |_request| {
                let entered_tx = Arc::clone(&entered_tx);
                let release = Arc::clone(&release);
                async move {
                    if let Some(tx) = entered_tx.lock_recover().take() {
                        let _ = tx.send(());
                    }
                    release.notified().await;
                    Ok(text_response("completed after observer disconnect"))
                }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(
            crate::SessionId::parse("recoverable-chat-disconnect").expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;
    let cursor = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;
    let stream = session.observe().subscribe_and_recover(cursor);
    let run_session = session.clone();
    let mut turn = tokio::spawn(async move {
        run_session
            .send(TurnInput::text("keep running"))
            .id(crate::TurnId::parse("disconnect-is-not-cancel").expect("nonblank host identity"))
            .output()
            .await
    });
    entered_rx.await.expect("provider entered");
    drop(stream);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut turn)
            .await
            .is_err(),
        "disconnecting observation must not cancel server work"
    );
    release.notify_one();
    let result = turn.await.expect("join turn")?;
    assert!(matches!(result.result.outcome, TurnOutcome::Finished(_)));
    Ok(())
}
