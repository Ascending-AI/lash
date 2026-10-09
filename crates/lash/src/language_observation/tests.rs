//! FIG-5548: the feed owns terminal authority; the host owns the pure fold.
use super::*;
use futures_util::StreamExt as _;
use lash_core::SessionRevision;
use lash_trace::{
    TraceLanguageExecution, TraceLanguageExecutionIdentity, TraceLanguageExecutionPayload,
    TraceRuntimeScope, TraceRuntimeSubject,
};
use std::time::Duration;

fn record(subject: TraceRuntimeSubject, occurrence: u64) -> TraceRecord {
    let payload = if occurrence == 0 {
        TraceLanguageExecutionPayload::ExecutionStarted {
            document: lash_trace::WorkflowDocumentRef {
                source_identity: "source".into(),
                module_ref: "module".into(),
                entry: lash_trace::WorkflowDocumentEntry::Main,
                ir_version: 1,
            },
        }
    } else {
        TraceLanguageExecutionPayload::NodeStarted {
            node_id: "node".into(),
            occurrence,
            call_id: None,
            context: Default::default(),
        }
    };
    fixture_record(
        lash_trace::TraceContext::default(),
        TraceEvent::LanguageExecution {
            language: "fixture-language".into(),
            event: TraceLanguageExecution {
                event_key: format!("{subject:?}:{occurrence}"),
                identity: TraceLanguageExecutionIdentity {
                    scope: TraceRuntimeScope::none(),
                    subject,
                    source_identity: "source".into(),
                    module_ref: "module".into(),
                    entry_kind: "main".into(),
                    entry_ref: None,
                    entry_name: "main".into(),
                    engine_execution_id: None,
                    generation: None,
                },
                payload,
            },
        },
    )
}

fn fixture_record(context: lash_trace::TraceContext, event: TraceEvent) -> TraceRecord {
    TraceRecord {
        schema_version: lash_trace::TRACE_SCHEMA_VERSION,
        id: "language-fixture".into(),
        content: lash_trace::TelemetryContent::Captured,
        timestamp: "2026-10-09T00:00:00Z".parse().expect("fixture time"),
        context,
        event,
    }
}

async fn next(
    feed: &mut crate::process::ProcessObservationStream,
) -> crate::process::ProcessObservationStreamItem {
    tokio::time::timeout(Duration::from_secs(30), feed.next())
        .await
        .expect("feed wakes")
        .expect("feed item")
        .expect("observation")
}

/// FIG-5548: a committed cancellation settles a blocked process's observed
/// occurrences without inventing observations of the untouched tail.
/// FIG-5576: the body start of an admitted step reaches the process's feed
/// as its own observation, behind the language observations before it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_process_cancelled_while_blocked_has_a_committed_cancelled_graph() {
    use lash_trace::{TraceLanguageExecutionStatus, WorkflowOverlayOccurrence};

    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite stores"),
    );
    lash_core::testing::process_execution_env_fixture(stores.process_env_store().as_ref()).await;
    let backend = crate::durable::DurableBackendBuilder::new(stores)
        .process_engine(Arc::new(lash_core::testing::HeldProcessEngine))
        .build()
        .expect("durable engine");
    let core = crate::tests::explicit_ephemeral_facets(crate::LashCore::standard_builder(backend))
        .build(crate::testing::runtime_lease_owner())
        .expect("build a durable core");
    let started = core
        .processes()
        .start(
            lash_core::ProcessStartRequest::new(
                lash_core::testing::held_engine_input(serde_json::Value::Null),
                lash_core::ProcessOriginator::host(),
                lash_core::LifetimeDecision::Detached,
            )
            .with_env_ref(lash_core::testing::process_execution_env_fixture_ref()),
            core.effect_host(),
        )
        .await
        .expect("start a blocked process");
    let process = &started.process_id;
    let observed = core.processes().observe(process);
    let snapshot = observed.snapshot().await.expect("initial durable snapshot");
    let mut subscription = observed.subscribe_and_recover(snapshot.cursor);
    let mut graph = lash_trace::WorkflowExecutionOverlayAccumulator::default();
    // The observation boundary receives these records from the language VM.
    // The held engine isolates its terminal writer from VM segment execution.
    for occurrence in 0..=4 {
        let mut traced = record(
            lash_trace::TraceRuntimeSubject::Process {
                process_id: process.clone(),
            },
            occurrence,
        );
        let TraceEvent::LanguageExecution { event, .. } = &mut traced.event else {
            unreachable!()
        };
        event.identity.generation = None;
        event.identity.engine_execution_id = Some(process.to_string());
        match &mut event.payload {
            _ if occurrence == 2 => {
                event.payload = TraceLanguageExecutionPayload::NodeWaiting {
                    node_id: "node".into(),
                    occurrence: 1,
                    awaited: lash_trace::TraceNodeAwaited::Sleep { deadline_ms: None },
                    context: Default::default(),
                };
            }
            TraceLanguageExecutionPayload::NodeStarted {
                node_id,
                occurrence: node_occurrence,
                ..
            } if occurrence == 3 => {
                *node_id = "running".into();
                *node_occurrence = 1;
            }
            _ if occurrence == 4 => {
                event.payload = TraceLanguageExecutionPayload::NodeCompleted {
                    node_id: "done".into(),
                    occurrence: 1,
                    call_id: None,
                    context: Default::default(),
                };
            }
            _ => {}
        }
        core.env.core.tracing.emitter().observe_product(|| traced);
    }
    let step = lash_trace::StepBodyStarted {
        process_id: process.clone(),
        node_id: "stepped".into(),
        occurrence: 1,
        context: Default::default(),
        call_id: lash_sansio::ToolCallId::fixture("stepped"),
        attempt: 1,
    };
    let stepped = step.clone();
    core.env.core.tracing.emitter().observe_product(|| {
        fixture_record(
            lash_trace::TraceContext::default(),
            TraceEvent::StepBodyStarted { step: stepped },
        )
    });
    let mut received = 0;
    let mut step_position = None;
    while received < 6 {
        let item = next(&mut subscription).await;
        match item {
            crate::process::ProcessObservationStreamItem::Event(event) => match &event.payload {
                crate::process::ProcessObservationEventPayload::LanguageExecution(observation) => {
                    graph
                        .observe(observation)
                        .expect("fold retained language observation");
                    received += 1;
                }
                crate::process::ProcessObservationEventPayload::StepBodyStarted(observation) => {
                    assert_eq!(observation.step, step);
                    graph
                        .step_body_started(observation)
                        .expect("fold the step body start");
                    received += 1;
                    step_position = Some(received);
                }
                _ => {}
            },
            crate::process::ProcessObservationStreamItem::Gap { .. } => {
                graph.reset_live();
                received = 0;
                step_position = None;
            }
        }
    }
    assert_eq!(
        step_position,
        Some(6),
        "the step's body start keeps its place in the process's order"
    );
    let before = graph.snapshot().expect("observed graph");
    assert!(
        before
            .sites
            .iter()
            .any(|site| matches!(site.occurrence, WorkflowOverlayOccurrence::Waiting { .. }))
    );
    let stepped = before
        .sites
        .iter()
        .find(|site| site.site == step.site())
        .expect("the step's site");
    assert!(matches!(
        stepped.occurrence,
        WorkflowOverlayOccurrence::Running { occurrence: 1, .. }
    ));
    assert_eq!(
        stepped.call.as_ref().map(|call| &call.call_id),
        Some(&step.call_id)
    );
    assert_eq!(
        before.sites.len(),
        4,
        "only observed sites are in the overlay: {:#?}",
        before.sites
    );
    core.processes()
        .cancel(process, core.effect_host())
        .await
        .expect("cancel the blocked process");
    let committed = tokio::time::timeout(
        Duration::from_secs(30),
        core.processes().await_output(process),
    )
    .await
    .expect("cancellation settles")
    .expect("committed terminal");
    assert_eq!(
        committed.terminal_status(),
        Some(lash_core::TerminalProcessStatus::Cancelled)
    );
    loop {
        let item = next(&mut subscription).await;
        if let crate::process::ProcessObservationStreamItem::Event(event) = item
            && let crate::process::ProcessObservationEventPayload::Committed { event } =
                &event.payload
            && let Some(terminal) = event.fact.terminal()
        {
            assert_eq!(
                terminal.status(),
                lash_core::TerminalProcessStatus::Cancelled
            );
            graph.settle(lash_trace::WorkflowOverlaySettlement {
                terminal: lash_trace::WorkflowOverlayTerminal::Cancelled,
                occurred_at: Some(
                    (std::time::UNIX_EPOCH + Duration::from_millis(event.occurred_at_ms)).into(),
                ),
            });
            break;
        }
    }
    let after = graph.snapshot().expect("terminal graph");
    assert_eq!(after.status, TraceLanguageExecutionStatus::Cancelled);
    for site in &before.sites {
        let settled = after
            .sites
            .iter()
            .find(|current| current.site == site.site)
            .expect("same site");
        match site.occurrence {
            WorkflowOverlayOccurrence::Running { occurrence, .. }
            | WorkflowOverlayOccurrence::Waiting { occurrence, .. } => {
                assert!(
                    matches!(settled.occurrence, WorkflowOverlayOccurrence::Cancelled { occurrence: ended, .. } if ended == occurrence)
                );
            }
            _ => assert_eq!(
                settled.occurrence, site.occurrence,
                "only in-flight occurrences settle"
            ),
        }
    }
    assert_eq!(after.sites.len(), before.sites.len());
    core.shutdown().await.expect("stop core");
}

/// FIG-5548: either count or byte overflow loses continuity explicitly for
/// every retained subject, and the feed resumes with typed language facts.
#[tokio::test]
async fn language_ingress_overflow_resnapshots_every_session_and_continues() {
    use crate::tests::CreatedSession as _;
    for by_bytes in [false, true] {
        let replay = Arc::new(lash_core::facade_support::InMemoryLiveReplayStore::new(
            lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
        ));
        let provider = crate::testing::TestProvider::builder()
            .kind("embed-test")
            .complete(|_| async { Ok(lash_core::llm::types::LlmResponse::default()) })
            .build()
            .into_handle();
        let core = crate::tests::explicit_ephemeral_facets(crate::LashCore::standard_builder(
            crate::tests::sqlite_memory_store_backend().await,
        ))
        .serve_test_llm_profile(provider, crate::tests::mock_llm_profile_spec())
        .live_replay_store(replay.clone())
        .build(crate::testing::runtime_lease_owner())
        .expect("core");
        let mut feeds = Vec::new();
        for (index, name) in ["overflow-first", "overflow-second"]
            .into_iter()
            .enumerate()
        {
            let session = core
                .session(SessionId::from(name))
                .created()
                .await
                .open()
                .await
                .expect("session");
            let snapshot = session.observe().snapshot().await.expect("snapshot");
            let revision = snapshot.cursor.parse().expect("cursor").revision;
            let mut feed = session.observe().subscribe_and_recover(snapshot.cursor);
            replay
                .publish(
                    &session.session_id(),
                    revision,
                    vec![LiveReplayEventDraft::new(
                        None::<lash_core::TurnId>,
                        SessionObservationEventPayload::ResidentChanged,
                    )],
                )
                .await
                .expect("seed replay");
            assert!(matches!(
                feed.next().await.expect("event").expect("observation"),
                crate::observe::SessionObservationStreamItem::Event(_)
            ));
            let scope = if index == 0 {
                lash_sansio::ExecutionScope::turn(session.session_id().clone(), "language-turn")
            } else {
                lash_sansio::ExecutionScope::session_operation(
                    session.session_id().clone(),
                    "language-operation",
                )
            };
            let subject = TraceRuntimeSubject::Effect {
                address: lash_sansio::EffectAddress::new(scope, "language-replay")
                    .expect("effect address"),
                effect_id: "cell".into(),
            };
            feeds.push((session, revision, feed, subject));
        }
        // This current-thread law admits the whole burst synchronously,
        // before the dispatcher gets to drain any of it.
        let count = if by_bytes { 1 } else { ingress::MAX_EVENTS + 1 };
        for occurrence in 0..count {
            let mut traced = record(feeds[0].3.clone(), occurrence as u64);
            if by_bytes && let TraceEvent::LanguageExecution { event, .. } = &mut traced.event {
                event.identity.entry_name = "x".repeat(ingress::MAX_BYTES);
            }
            core.env.core.tracing.emitter().observe_product(|| traced);
        }
        for (session, revision, mut feed, subject) in feeds {
            let item = tokio::time::timeout(Duration::from_secs(10), feed.next())
                .await
                .expect("overflow wakes feed")
                .expect("gap")
                .expect("replacement");
            let crate::observe::SessionObservationStreamItem::Gap { observation, gap } = item
            else {
                panic!("overflow cannot leave a clean suffix");
            };
            assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Unavailable);
            assert_eq!(gap.latest_revision, revision);
            assert_eq!(
                observation.read_view.session_id(),
                session.session_id().as_str()
            );
            core.env
                .core
                .tracing
                .emitter()
                .observe_product(|| record(subject.clone(), 0));
            let item = tokio::time::timeout(Duration::from_secs(10), feed.next())
                .await
                .expect("new publication arrives")
                .expect("event")
                .expect("continued feed");
            let crate::observe::SessionObservationStreamItem::Event(event) = item else {
                panic!("one overflow yields one gap");
            };
            let SessionObservationEventPayload::LanguageExecution(observation) = &event.payload
            else {
                panic!("typed language observation");
            };
            assert_eq!(
                observation.language, "fixture-language",
                "routing is independent of dialect name"
            );
            assert_eq!(observation.execution.identity.subject, subject);
            assert_eq!(
                event.revision(),
                SessionRevision::new(0),
                "language evidence proves no durable advance"
            );
        }
        core.shutdown().await.expect("shutdown");
    }
}

/// A dispatcher over a SQLite process registry and a replay store a law
/// chooses, whose feeds look at the durable process only when told to.
struct Dispatch {
    registry: Arc<dyn lash_core::ProcessRegistry>,
    replay: Arc<dyn ProcessReplayStore>,
    publisher: Arc<LanguageObservationPublisher>,
}

impl Dispatch {
    async fn new(replay: Arc<dyn ProcessReplayStore>, events: usize, bytes: usize) -> Self {
        let publisher = Arc::new(LanguageObservationPublisher::with_limits(
            replay.clone(),
            Arc::new(lash_core::facade_support::InMemoryLiveReplayStore::new(
                lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
            )),
            events,
            bytes,
        ));
        Self {
            registry: crate::tests::sqlite_memory_store_set()
                .await
                .process_registry(),
            replay,
            publisher,
        }
    }

    async fn register(&self) -> ProcessId {
        self.registry
            .register_process(
                lash_core::ProcessRegistration::new(
                    lash_core::testing::held_engine_input(serde_json::Value::Null),
                    lash_core::ProcessProvenance::host(),
                    lash_core::Lifetime::Detached,
                )
                .with_execution_env_ref(Some(
                    lash_core::testing::process_execution_env_fixture_ref(),
                )),
            )
            .await
            .expect("register process")
            .id
    }

    fn observe(&self, process: &ProcessId) -> crate::process_feed::ObservableProcess {
        crate::process_feed::ObservableProcess {
            source: crate::process_feed::ProcessFeedSource::new(
                process.clone(),
                self.registry.clone(),
                lash_core::facade_support::ProcessWorkObserver::new(self.registry.clone()),
                lash_core::ProcessEngineRegistry::default(),
                self.replay.clone(),
                lash_trace::ObservationWorkLimits::standard(),
                crate::process_feed::FeedReconcile {
                    publisher: Arc::clone(&self.publisher),
                    changes: lash_core::runtime::ProcessChangeHub::new(),
                    pacing: lash_core::runtime::PollPacing::new(
                        Duration::from_secs(3600),
                        Duration::from_secs(3600),
                    )
                    .expect("pacing"),
                },
            ),
        }
    }

    /// Commit the process's execution start, unpublished, and answer the
    /// authority its later appends are written under.
    async fn commit_started(
        &self,
        process: &ProcessId,
    ) -> lash_core::ProcessExecutionWriteAuthority {
        let authority =
            lash_core::ProcessExecutionWriteAuthority::invocation(process.clone(), "dispatched")
                .bind_attempt(1);
        self.registry
            .record_first_started_with_authority(
                process,
                authority.invocation_started().expect("started fact"),
                &authority,
            )
            .await
            .expect("commit without publishing");
        authority
    }

    /// Admit one language observation of `process` without waking the
    /// worker: a stalled worker leaves it queued.
    fn admit_language(&self, process: &ProcessId, key: &str) {
        let language = lash_core::testing::process_language_observation(process, key, key);
        let charge = self.publisher.charge(&(process, &language));
        self.publisher
            .process
            .enqueue(charge, || ProcessPublication {
                id: process.clone(),
                draft: Some(ProcessReplayEventDraft::language_execution(
                    ProcessSequence(0),
                    language,
                )),
                completion: None,
                mark: None,
            });
    }

    /// Wait until the worker holds nothing admitted and owes no invalidation.
    async fn drained(&self) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while self.publisher.process.has_work() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the dispatcher drains");
    }
}

fn memory_replay(
    config: lash_core::InMemoryProcessReplayStoreConfig,
) -> Arc<lash_core::InMemoryProcessReplayStore> {
    Arc::new(lash_core::InMemoryProcessReplayStore::new(config))
}

fn is_language(item: &crate::process::ProcessObservationStreamItem) -> bool {
    matches!(item, crate::process::ProcessObservationStreamItem::Event(event)
        if matches!(event.payload, crate::process::ProcessObservationEventPayload::LanguageExecution(_)))
}

#[track_caller]
fn gap_cause(
    item: crate::process::ProcessObservationStreamItem,
) -> lash_core::ProcessObservationGapCause {
    match item {
        crate::process::ProcessObservationStreamItem::Gap { gap, .. } => gap.cause,
        other => panic!("expected a gap, got {other:?}"),
    }
}

/// FIG-5548: recovery uses the same FIFO barrier as after-commit publication.
#[tokio::test]
async fn a_reconciled_commit_cannot_overtake_an_accepted_language_observation() {
    let dispatch = Dispatch::new(
        memory_replay(lash_core::InMemoryProcessReplayStoreConfig::standard()),
        ingress::MAX_EVENTS,
        ingress::MAX_BYTES,
    )
    .await;
    let process = dispatch.register().await;
    let observed = dispatch.observe(&process);
    let snapshot = observed.snapshot().await.expect("snapshot before commit");
    // A stalled worker may leave an admitted language fact queued when a
    // follower discovers a durable commit. Keep it queued until recovery
    // starts the publisher, making the ordering independent of scheduling.
    dispatch.admit_language(&process, "accepted-first");
    dispatch.commit_started(&process).await;
    let mut feed = observed.subscribe_and_recover(snapshot.cursor);
    let first = next(&mut feed).await;
    assert!(
        is_language(&first),
        "a reconciled commit cannot overtake accepted language evidence"
    );
    let second = next(&mut feed).await;
    assert!(
        matches!(second, crate::process::ProcessObservationStreamItem::Event(event)
        if matches!(event.payload, crate::process::ProcessObservationEventPayload::Committed { .. }))
    );
    dispatch.publisher.shutdown().await;
}

/// A replay store whose publications wait until a law lets them through.
struct GatedReplay {
    inner: Arc<lash_core::InMemoryProcessReplayStore>,
    gate: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl ProcessReplayStore for GatedReplay {
    async fn publish(
        &self,
        process_id: &ProcessId,
        events: Vec<ProcessReplayEventDraft>,
    ) -> Result<Vec<Arc<lash_core::ProcessObservationEvent>>, lash_core::ProcessReplayStoreError>
    {
        let _open = self.gate.acquire().await.expect("the gate stays");
        self.inner.publish(process_id, events).await
    }
    async fn replay_after_cursor(
        &self,
        cursor: &lash_core::ProcessObservationCursor,
    ) -> Result<lash_core::ProcessReplayOutcome, lash_core::ProcessReplayStoreError> {
        self.inner.replay_after_cursor(cursor).await
    }
    async fn subscribe_after_cursor(
        &self,
        cursor: &lash_core::ProcessObservationCursor,
    ) -> Result<lash_core::ProcessReplaySubscribeOutcome, lash_core::ProcessReplayStoreError> {
        self.inner.subscribe_after_cursor(cursor).await
    }
    async fn current_cursor(
        &self,
        process_id: &ProcessId,
        sequence: ProcessSequence,
    ) -> Result<lash_core::ProcessObservationCursor, lash_core::ProcessReplayStoreError> {
        self.inner.current_cursor(process_id, sequence).await
    }
    async fn earliest_cursor(
        &self,
        process_id: &ProcessId,
        sequence: ProcessSequence,
    ) -> Result<lash_core::ProcessObservationCursor, lash_core::ProcessReplayStoreError> {
        self.inner.earliest_cursor(process_id, sequence).await
    }
    async fn invalidate_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<(), lash_core::ProcessReplayStoreError> {
        self.inner.invalidate_process(process_id).await
    }
    async fn invalidate_all(&self) -> Result<(), lash_core::ProcessReplayStoreError> {
        self.inner.invalidate_all().await
    }
    async fn trim_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<(), lash_core::ProcessReplayStoreError> {
        self.inner.trim_process(process_id).await
    }
}

/// FIG-5624: the dispatcher overflows while a feed's reconcile waits for
/// its fact's acknowledgement behind a stalled publication. The dropped
/// fact is continuity loss, not the end of the feed: the feed answers the
/// unbridged commit with the durable process, the store-wide invalidation
/// that follows is one more gap, and the feed delivers what comes after.
#[tokio::test]
async fn a_feed_whose_reconcile_publication_overflowed_gaps_and_keeps_delivering() {
    const EVENTS: usize = 4;
    let gated = Arc::new(GatedReplay {
        inner: memory_replay(lash_core::InMemoryProcessReplayStoreConfig::standard()),
        gate: tokio::sync::Semaphore::new(0),
    });
    let dispatch = Dispatch::new(gated.clone(), EVENTS, ingress::MAX_BYTES).await;
    let followed = dispatch.register().await;
    let noisy = ProcessId::fixture("noisy-process");
    let observed = dispatch.observe(&followed);
    let snapshot = observed.snapshot().await.expect("snapshot before commit");
    dispatch.commit_started(&followed).await;

    // The worker takes the noisy process's observation first and stalls in
    // the store; the feed's reconciled fact waits behind it.
    dispatch.admit_language(&noisy, "stalled");
    let mut feed = observed.subscribe_and_recover(snapshot.cursor);
    while dispatch.publisher.process.admitted() < 2 {
        assert!(
            tokio::time::timeout(Duration::from_millis(10), feed.next())
                .await
                .is_err(),
            "the feed waits for its fact's acknowledgement"
        );
    }
    for burst in 0..EVENTS {
        dispatch.admit_language(&noisy, &format!("burst-{burst}"));
    }
    assert_eq!(
        gap_cause(next(&mut feed).await),
        lash_core::ProcessObservationGapCause::CommitUnbridged
    );

    gated.gate.add_permits(EVENTS);
    dispatch.drained().await;
    assert_eq!(
        gap_cause(next(&mut feed).await),
        lash_core::ProcessObservationGapCause::Replay {
            reason: lash_core::ProcessReplayGapReason::Unavailable
        },
        "the overflow invalidated every process"
    );
    dispatch.admit_language(&followed, "after the overflow");
    assert!(is_language(&next(&mut feed).await));
    dispatch.publisher.shutdown().await;
}

/// FIG-5624: a committed fact too large for the ingress, or for the store's
/// window, loses its own process's continuity and nothing else. Another
/// process's observer sees no gap, and the process itself is still followed.
#[tokio::test]
async fn an_oversized_committed_fact_invalidates_only_its_own_process() {
    const LIMIT: usize = 8 * 1024;
    for refused_by_store in [false, true] {
        let dispatch = if refused_by_store {
            let config = lash_core::InMemoryProcessReplayStoreConfig {
                max_bytes_per_process: LIMIT,
                ..lash_core::InMemoryProcessReplayStoreConfig::standard()
            };
            Dispatch::new(
                memory_replay(config),
                ingress::MAX_EVENTS,
                ingress::MAX_BYTES,
            )
            .await
        } else {
            let config = lash_core::InMemoryProcessReplayStoreConfig::standard();
            Dispatch::new(memory_replay(config), ingress::MAX_EVENTS, LIMIT).await
        };
        let large = dispatch.register().await;
        let other = dispatch.register().await;
        let authority = dispatch.commit_started(&large).await;
        let observed = dispatch.observe(&large);
        let snapshot = observed.snapshot().await.expect("snapshot");
        let mut large_feed = observed.subscribe_and_recover(snapshot.cursor);
        let observed = dispatch.observe(&other);
        let snapshot = observed.snapshot().await.expect("snapshot");
        let mut other_feed = observed.subscribe_and_recover(snapshot.cursor);
        dispatch.admit_language(&other, "other before");
        dispatch.publisher.start();
        assert!(is_language(&next(&mut other_feed).await));

        let wait = lash_core::WaitState {
            since_ms: 0,
            kind: lash_core::WaitKind::Call {
                call_id: lash_core::ToolCallId::fixture("oversized"),
                tool_id: lash_core::ToolId::from("x".repeat(2 * LIMIT)),
            },
            site: None,
        };
        let oversized = dispatch
            .registry
            .append_event_with_authority(
                &large,
                lash_core::ProcessEventAppendRequest::wait_entered(&large, &wait),
                &authority,
            )
            .await
            .expect("commit the oversized fact")
            .event;
        dispatch
            .publisher
            .enqueue_committed(&large, oversized.into());
        dispatch.drained().await;

        assert_eq!(
            gap_cause(next(&mut large_feed).await),
            lash_core::ProcessObservationGapCause::Replay {
                reason: lash_core::ProcessReplayGapReason::Unavailable
            },
            "refused by the store: {refused_by_store}"
        );
        dispatch.admit_language(&other, "other after");
        dispatch.admit_language(&large, "large after");
        dispatch.publisher.start();
        assert!(
            is_language(&next(&mut other_feed).await),
            "another process's observer sees no gap (refused by the store: {refused_by_store})"
        );
        assert!(is_language(&next(&mut large_feed).await));
        dispatch.publisher.shutdown().await;
    }
}
