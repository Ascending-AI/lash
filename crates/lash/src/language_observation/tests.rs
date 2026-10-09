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

/// FIG-5548: recovery uses the same FIFO barrier as after-commit publication.
#[tokio::test]
async fn a_reconciled_commit_cannot_overtake_an_accepted_language_observation() {
    let stores = crate::tests::sqlite_memory_store_set().await;
    let registry: Arc<dyn lash_core::ProcessRegistry> = stores.process_registry();
    let process = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::testing::held_engine_input(serde_json::Value::Null),
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_execution_env_ref(Some(lash_core::testing::process_execution_env_fixture_ref())),
        )
        .await
        .expect("register process")
        .id;
    let replay = Arc::new(lash_core::InMemoryProcessReplayStore::new(
        lash_core::InMemoryProcessReplayStoreConfig::standard(),
    ));
    let publisher = Arc::new(LanguageObservationPublisher::new(
        replay.clone(),
        Arc::new(lash_core::facade_support::InMemoryLiveReplayStore::new(
            lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
        )),
    ));
    let observed = crate::process_feed::ObservableProcess {
        source: crate::process_feed::ProcessFeedSource::new(
            process.clone(),
            registry.clone(),
            lash_core::ProcessEngineRegistry::default(),
            replay.clone(),
            lash_trace::ObservationWorkLimits::standard(),
            crate::process_feed::FeedReconcile {
                publisher: Arc::clone(&publisher),
                changes: lash_core::runtime::ProcessChangeHub::new(),
                pacing: lash_core::runtime::PollPacing::new(
                    Duration::from_secs(3600),
                    Duration::from_secs(3600),
                )
                .expect("pacing"),
            },
        ),
    };
    let snapshot = observed.snapshot().await.expect("snapshot before commit");
    let language = lash_core::testing::process_language_observation(
        &process,
        "accepted-first",
        "node started",
    );
    let charge = worker::charge(&(&process, &language));
    // A stalled worker may leave an admitted language fact queued when a
    // follower discovers a durable commit. Keep it queued until recovery
    // starts the publisher, making the ordering independent of scheduling.
    publisher.process.enqueue(charge, || ProcessPublication {
        id: process.clone(),
        draft: ProcessReplayEventDraft::language_execution(ProcessSequence(0), language),
        completion: None,
    });
    let authority =
        lash_core::ProcessExecutionWriteAuthority::invocation(process.clone(), "ordered-recovery")
            .bind_attempt(1);
    registry
        .record_first_started_with_authority(
            &process,
            authority.invocation_started().expect("started fact"),
            &authority,
        )
        .await
        .expect("commit without publishing");
    let mut feed = observed.subscribe_and_recover(snapshot.cursor);
    let first = next(&mut feed).await;
    assert!(
        matches!(first, crate::process::ProcessObservationStreamItem::Event(event)
        if matches!(event.payload, crate::process::ProcessObservationEventPayload::LanguageExecution(_))),
        "a reconciled commit cannot overtake accepted language evidence"
    );
    let second = next(&mut feed).await;
    assert!(
        matches!(second, crate::process::ProcessObservationStreamItem::Event(event)
        if matches!(event.payload, crate::process::ProcessObservationEventPayload::Committed { .. }))
    );
    publisher.shutdown().await;
}
