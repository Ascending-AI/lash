//! Durable history paging and the effect evidence a snapshot folds from it,
//! over a real SQLite registry.

use super::*;
use lash_core::{
    ProcessEffectCoverage, ProcessEffectGapReason, ProcessEffectReport, ProcessEvent,
    ProcessEventPageEvents, ProcessEventPageMore, ProcessReadView,
};

struct Fixture {
    _dir: tempfile::TempDir,
    registry: Arc<dyn ProcessRegistry>,
    process_id: ProcessId,
    authority: lash_core::ProcessExecutionWriteAuthority,
}

impl Fixture {
    /// A registered, started engine process, which may write runtime-owned
    /// effect-summary events.
    async fn new(label: &str) -> Self {
        let dir = tempfile::tempdir().expect("history tempdir");
        let registry: Arc<dyn ProcessRegistry> = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open_standalone_for_testing(
                &dir.path().join("processes.db"),
            )
            .await
            .expect("open SQLite process registry"),
        );
        let process_id = registry
            .register_process(
                lash_core::ProcessRegistration::new(
                    lash_core::ProcessInput::Engine {
                        kind: "history-fixture".to_string(),
                        payload: serde_json::Value::Null,
                    },
                    lash_core::ProcessProvenance::host(),
                    lash_core::Lifetime::Detached,
                )
                .with_execution_env_ref(Some(
                    lash_core::testing::process_execution_env_fixture_ref(),
                ))
                .with_admitted_identity(
                    lash_core::AdmittedProcessIdentity::for_testing(
                        lash_core::ProcessIdentity::for_definition(
                            lash_core::ProcessDefinitionRef::unclaimed(
                                "history-fixture",
                                serde_json::Value::Null,
                            ),
                            Some(label),
                        ),
                    ),
                ),
            )
            .await
            .expect("register the history process")
            .id;
        let authority = lash_core::ProcessExecutionWriteAuthority::invocation(
            process_id.clone(),
            "history-invocation",
        )
        .bind_attempt(1);
        registry
            .record_first_started_with_authority(
                &process_id,
                authority
                    .invocation_started()
                    .expect("the authority is bound to attempt one"),
                &authority,
            )
            .await
            .expect("record the execution start");
        Self {
            _dir: dir,
            registry,
            process_id,
            authority,
        }
    }

    /// Commit one runtime-owned effect-summary occurrence.
    async fn commit_outcome(&self, occurrence: u64) -> ProcessEvent {
        let outcome = lash_core::ProcessEffectOccurrence::new(
            lash_sansio::effect_identity_fixture("node", occurrence),
            "tools.echo",
            lash_core::ProcessEffectOutcomeClass::Success,
            None,
            format!("history-effect:{occurrence}"),
            lash_core::FleetFormat::current(),
        );
        self.registry
            .append_event_with_authority(
                &self.process_id,
                outcome.append_request(),
                &self.authority,
            )
            .await
            .expect("commit an effect outcome")
            .event
    }

    async fn high_water(&self) -> u64 {
        self.registry
            .get_process(&self.process_id)
            .await
            .expect("read record")
            .expect("record")
            .last_event_sequence
    }

    /// The process's snapshot under `limits`, through a feed source nothing
    /// publishes to.
    async fn read_view(&self, limits: lash_trace::ObservationWorkLimits) -> ProcessReadView {
        let replay = Arc::new(lash_core::InMemoryProcessReplayStore::new(
            lash_core::InMemoryProcessReplayStoreConfig::standard(),
        ));
        let publisher = Arc::new(
            crate::language_observation::LanguageObservationPublisher::new(
                replay.clone(),
                Arc::new(lash_core::facade_support::InMemoryLiveReplayStore::new(
                    lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
                )),
            ),
        );
        crate::process_feed::ProcessFeedSource::new(
            self.process_id.clone(),
            Arc::clone(&self.registry),
            lash_core::facade_support::ProcessWorkObserver::new(Arc::clone(&self.registry))
                .with_work_limits(limits),
            lash_core::ProcessEngineRegistry::default(),
            replay,
            limits,
            crate::process_feed::FeedReconcile {
                publisher,
                changes: lash_core::runtime::ProcessChangeHub::new(),
            },
        )
        .snapshot()
        .await
        .expect("snapshot")
        .read_view
    }
}

/// Full and Lite pages continue one history: each page's continuation is
/// its last sequence, and the pages together are every event once.
#[tokio::test]
async fn full_and_lite_pages_continue_one_history() {
    let fixture = Fixture::new("history-pages").await;
    for occurrence in 1..=5 {
        fixture.commit_outcome(occurrence).await;
    }
    let limit = std::num::NonZeroUsize::new(2).expect("page size");
    let mut from = ProcessHistoryContinuation::start(fixture.process_id.clone());
    let mut sequences = Vec::new();
    let mut mode = ProcessEventQueryMode::Lite;
    loop {
        let read = read_events(&fixture.registry, from, limit, mode)
            .await
            .expect("read a page");
        let ProcessEventReadOutcome::Retained(page) = read.outcome else {
            panic!("retained history");
        };
        let page_sequences: Vec<u64> = match &page.events {
            ProcessEventPageEvents::Full(events) => {
                assert_eq!(mode, ProcessEventQueryMode::Full);
                events.iter().map(|event| event.sequence).collect()
            }
            ProcessEventPageEvents::Lite(events) => {
                assert_eq!(mode, ProcessEventQueryMode::Lite);
                events.iter().map(|event| event.sequence).collect()
            }
        };
        assert_eq!(read.next.process_id(), &fixture.process_id);
        assert_eq!(Some(&read.next.after_sequence()), page_sequences.last());
        sequences.extend(page_sequences);
        mode = match mode {
            ProcessEventQueryMode::Lite => ProcessEventQueryMode::Full,
            ProcessEventQueryMode::Full => ProcessEventQueryMode::Lite,
        };
        from = read.next;
        if matches!(page.more, ProcessEventPageMore::Complete) {
            break;
        }
    }
    let high_water = fixture.high_water().await;
    assert_eq!(sequences, (1..=high_water).collect::<Vec<_>>());
    assert_eq!(from.after_sequence(), high_water);
}

/// A host-released prefix (FIG-3482) is never served short: a read from the
/// start answers the typed release and continues after it, and a snapshot
/// folds the retained events and says its evidence is missing the released
/// ones.
#[tokio::test]
async fn a_released_prefix_is_typed_in_reads_and_in_snapshot_evidence() {
    let fixture = Fixture::new("history-released").await;
    let mut committed = Vec::new();
    for occurrence in 1..=4 {
        committed.push(fixture.commit_outcome(occurrence).await);
    }
    let horizon = committed[1].sequence;
    lash_core::ProcessRetention::release_process_events(
        fixture.registry.as_ref(),
        &fixture.process_id,
        horizon,
    )
    .await
    .expect("release the prefix");

    let limit = std::num::NonZeroUsize::new(16).expect("page size");
    let read = read_events(
        &fixture.registry,
        ProcessHistoryContinuation::start(fixture.process_id.clone()),
        limit,
        ProcessEventQueryMode::Full,
    )
    .await
    .expect("read from the start");
    assert_eq!(
        read.outcome,
        ProcessEventReadOutcome::NoLongerRetained(ProcessEventHistoryRetention::Released {
            released_through: horizon,
        })
    );
    assert_eq!(read.next.after_sequence(), horizon);
    let read = read_events(
        &fixture.registry,
        read.next,
        limit,
        ProcessEventQueryMode::Full,
    )
    .await
    .expect("read after the release");
    let ProcessEventReadOutcome::Retained(lash_core::ProcessEventPage {
        events: ProcessEventPageEvents::Full(retained),
        ..
    }) = read.outcome
    else {
        panic!("the events after the release are retained");
    };
    assert_eq!(
        retained
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        committed[2..]
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>()
    );

    let mut expected = ProcessEffectReport::default();
    for event in &retained {
        expected
            .fold_event(&event.fact, lash_core::FleetFormat::current())
            .expect("fold");
    }
    let ProcessReadView::Retained(view) = fixture
        .read_view(lash_trace::ObservationWorkLimits::standard())
        .await
    else {
        panic!("retained");
    };
    assert_eq!(view.process.last_event_sequence, fixture.high_water().await);
    assert_eq!(view.effects.report, expected);
    assert_eq!(
        view.effects.coverage,
        ProcessEffectCoverage::Incomplete {
            reason: ProcessEffectGapReason::HistoryReleased
        }
    );
}

/// A snapshot folds effect evidence within the core's work limits, and one
/// that ran out says how far it read.
#[tokio::test]
async fn snapshot_effect_evidence_is_bounded_by_the_work_limits_and_says_so() {
    let fixture = Fixture::new("history-budget").await;
    for occurrence in 1..=4 {
        fixture.commit_outcome(occurrence).await;
    }
    let ProcessReadView::Retained(view) = fixture
        .read_view(lash_trace::ObservationWorkLimits {
            process_effect_fold_pages: 1,
            process_effect_fold_page_size: std::num::NonZeroUsize::new(2).expect("page size"),
            ..lash_trace::ObservationWorkLimits::standard()
        })
        .await
    else {
        panic!("retained");
    };
    assert_eq!(view.process.last_event_sequence, fixture.high_water().await);
    assert_eq!(
        view.effects.coverage,
        ProcessEffectCoverage::Incomplete {
            reason: ProcessEffectGapReason::AcquisitionBudgetExhausted
        }
    );
    assert_eq!(view.effects.observed_through.as_u64(), 2);
}
