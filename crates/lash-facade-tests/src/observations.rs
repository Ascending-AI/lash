use super::*;
use lash::TurnEvent;
#[cfg(feature = "rlm")]
use lash_core::llm::transport::LlmTransportError;
use std::sync::Mutex as StdMutex;
#[cfg(feature = "rlm")]
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_sansio::llm::types::{StreamBlockEvent, StreamBlockKind};
use lash_sansio::sync::LockResultExt as _;
pub fn bid() -> lash_core::llm::types::StreamBlockIdentity {
    lash_core::llm::types::StreamBlockIdentity::new("text:0", 0)
}

/// A live replay store that counts the commits published into it: a node
/// publishes a turn's `Committed` after the turn's commit is acknowledged,
/// so a law that reads the replay after a send waits for its publication.
pub struct PublishedCommits {
    pub inner: Arc<dyn lash_core::LiveReplayStore>,
    pub commits: tokio::sync::watch::Sender<(usize, lash_core::SessionRevision)>,
}

impl std::fmt::Debug for PublishedCommits {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PublishedCommits")
            .field("commits", &self.commits.borrow().0)
            .finish_non_exhaustive()
    }
}

impl PublishedCommits {
    pub fn over(inner: Arc<dyn lash_core::LiveReplayStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            commits: tokio::sync::watch::channel((0, lash_core::SessionRevision::new(0))).0,
        })
    }

    pub fn unbounded() -> Arc<Self> {
        Self::over(Arc::new(
            lash_core::facade_support::InMemoryLiveReplayStore::new(
                lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
            ),
        ))
    }

    /// Wait until `count` commits carrying rows were published, and
    /// answer the newest one's revision.
    pub async fn published(&self, count: usize) -> lash_core::SessionRevision {
        let mut commits = self.commits.subscribe();
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            commits.wait_for(|(published, _)| *published >= count),
        )
        .await
        .expect("the commit reaches the live replay")
        .expect("the store outlives the wait")
        .1
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
        if commits > 0 {
            self.commits.send_modify(|(count, newest)| {
                *count += commits;
                *newest = (*newest).max(revision);
            });
        }
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

    fn earliest_cursor(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.inner.earliest_cursor(session_id, revision)
    }

    async fn invalidate_all(&self) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.invalidate_all().await
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
pub async fn core_publishing_to(live: Arc<PublishedCommits>) -> Result<LashCore> {
    explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .live_replay_store(live)
    .build(runtime_lease_owner())
}

pub fn reasoning_output_part(text: &str, item_id: &str) -> LlmOutputPart {
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

pub fn reasoning_activities(output: &lash::turn::TurnOutput) -> Vec<&str> {
    output
        .activities
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                kind: StreamBlockKind::Reasoning,
                text,
                ..
            }) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[cfg(feature = "rlm")]
pub fn output_then_failing_rlm_prose_provider(
    transport_calls: Arc<AtomicUsize>,
    requests: Arc<StdMutex<Vec<lash_core::LlmRequest>>>,
) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("retrying-rlm-prose")
        .requires_streaming(true)
        .options(lash_core::facade_support::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(Some(2))
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
                    stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                        kind: StreamBlockKind::AssistantText,
                        block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                        text: "retry observer single-copy marker\n<typescript>\n".to_string(),
                    }));
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
                    2 => "<typescript>\nawait control.finish(\"provider retry succeeded\");\n</typescript>",
                    _ => "<typescript>\nawait control.finish(\"subsequent turn succeeded\");\n</typescript>",
                };
                stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta { kind: StreamBlockKind::AssistantText,block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0), text: text.to_string() }));
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
pub fn natural_prose_reasoning_provider(
    requests: Arc<StdMutex<Vec<lash_core::LlmRequest>>>,
) -> ProviderHandle {
    let calls = Arc::new(AtomicUsize::new(0));
    lash_core::testing::TestProvider::builder()
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
pub fn provider_request_text(request: &lash_core::LlmRequest) -> String {
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

#[derive(Debug)]
pub struct PausedCommitReplayStore {
    pub inner: lash_core::facade_support::InMemoryLiveReplayStore,
    pub boundary: PublicationBoundary,
    pub pause: Arc<PublicationPause>,
}

#[derive(Debug)]
pub struct PublicationPause {
    pub boundary_reached: std::sync::atomic::AtomicBool,
    pub release_boundary: std::sync::atomic::AtomicBool,
    pub pause_lock: StdMutex<()>,
    pub pause_changed: std::sync::Condvar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicationBoundary {
    /// Before the store assigns the commit's batch its positions.
    BeforePublish,
    /// After the batch is replay-visible, before its subscribers hear it.
    BeforeNotification,
}

#[derive(Debug)]
pub struct FailingAppendReplayStore {
    pub inner: lash_core::facade_support::InMemoryLiveReplayStore,
}

impl Default for FailingAppendReplayStore {
    fn default() -> Self {
        Self::new()
    }
}

impl FailingAppendReplayStore {
    pub fn new() -> Self {
        Self {
            inner: lash_core::facade_support::InMemoryLiveReplayStore::new(
                lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
            ),
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

    fn earliest_cursor(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.inner.earliest_cursor(session_id, revision)
    }

    async fn invalidate_all(&self) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.invalidate_all().await
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

impl PausedCommitReplayStore {
    pub fn at(boundary: PublicationBoundary) -> Self {
        let pause = Arc::new(PublicationPause {
            boundary_reached: std::sync::atomic::AtomicBool::new(false),
            release_boundary: std::sync::atomic::AtomicBool::new(false),
            pause_lock: StdMutex::new(()),
            pause_changed: std::sync::Condvar::new(),
        });
        let inner = if boundary == PublicationBoundary::BeforeNotification {
            let notification_pause = Arc::clone(&pause);
            lash_core::facade_support::InMemoryLiveReplayStore::with_before_notification_gate_for_testing(
                lash_core::facade_support::InMemoryLiveReplayStore::new(lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard()),
                move |events| {
                    if Self::is_authoritative_events(events) {
                        notification_pause.pause();
                    }
                },
            )
        } else {
            lash_core::facade_support::InMemoryLiveReplayStore::new(
                lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
            )
        };
        Self {
            inner,
            boundary,
            pause,
        }
    }

    pub fn is_authoritative_events(events: &[Arc<lash_core::SessionObservationEvent>]) -> bool {
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

    pub async fn wait_for_commit_append(&self) {
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

    pub fn release_commit_install(&self) {
        self.pause
            .release_boundary
            .store(true, std::sync::atomic::Ordering::Release);
        self.pause.pause_changed.notify_all();
    }
}

impl PublicationPause {
    pub fn pause(&self) {
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

    fn earliest_cursor(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.inner.earliest_cursor(session_id, revision)
    }

    async fn invalidate_all(&self) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.invalidate_all().await
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

pub fn observation_assistant_delta(event: &lash_core::SessionObservationEvent) -> Option<String> {
    match &event.payload {
        lash_core::SessionObservationEventPayload::TurnActivity(activity) => {
            match &activity.event {
                TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                    kind: StreamBlockKind::AssistantText,
                    text,
                    ..
                }) => Some(text.to_string()),
                _ => None,
            }
        }
        _ => None,
    }
}
