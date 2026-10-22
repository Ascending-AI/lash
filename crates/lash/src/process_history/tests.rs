//! Durable history paging and the effect evidence a snapshot folds from it,
//! over a real SQLite registry.

use super::*;
use lash_core::ProcessEffectCoverage;
use lash_core::ProcessEffectGapReason;
use lash_core::ProcessReadView;
use lash_execution_contracts::process::ProcessEffectReport;
use lash_execution_contracts::process::ProcessEvent;
use lash_execution_contracts::process::ProcessEventPageEvents;
use lash_execution_contracts::process::ProcessEventPageMore;

struct Fixture {
    _dir: tempfile::TempDir,
    registry: Arc<dyn ProcessRegistry>,
    /// The same registry, seeding the history an owner's commits append.
    seeding: Arc<dyn lash_execution_contracts::process::ConformanceProcessRegistry>,
    process_id: ProcessId,
}

impl Fixture {
    /// A registered engine process, whose history carries runtime-owned
    /// effect-summary events.
    async fn new(label: &str) -> Self {
        let dir = tempfile::tempdir().expect("history tempdir");
        let seeding: Arc<dyn lash_execution_contracts::process::ConformanceProcessRegistry> =
            Arc::new(
                lash_sqlite_store::SqliteProcessRegistry::open_standalone_for_testing(
                    &dir.path().join("processes.db"),
                )
                .await
                .expect("open SQLite process registry"),
            );
        let registry: Arc<dyn ProcessRegistry> = seeding.clone();
        let process_id = registry
            .register_process(
                lash_execution_contracts::process::ProcessRegistration::new(
                    lash_execution_contracts::process::ProcessInput::Engine {
                        kind: "history-fixture".to_string(),
                        payload: serde_json::Value::Null,
                        launch: lash_execution_contracts::process::EngineLaunch::new(
                            lash_execution_contracts::testing::process_execution_env_fixture_ref(),
                            None,
                        ),
                    },
                    lash_execution_contracts::process::ProcessProvenance::host(),
                    lash_execution_contracts::process::Lifetime::Detached,
                )
                .with_admitted_identity(
                    lash_execution_contracts::process::AdmittedProcessIdentity::for_testing(
                        lash_execution_contracts::process::ProcessIdentity::for_definition(
                            lash_execution_contracts::process::ProcessDefinitionRef::unclaimed(
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
        Self {
            _dir: dir,
            registry,
            seeding,
            process_id,
        }
    }

    /// Commit one runtime-owned effect-summary occurrence.
    async fn commit_outcome(&self, occurrence: u64) -> ProcessEvent {
        let outcome = lash_execution_contracts::process::ProcessEffectOccurrence::new(
            lash_vocabulary::sites::EffectLocation::authored(
                lash_protocol_types::effect_identity_fixture("node", occurrence),
            ),
            "tools.echo",
            lash_execution_contracts::process::ProcessEffectOutcomeClass::Success,
            None,
            format!("history-effect:{occurrence}"),
            lash_core::FleetFormat::current(),
        );
        self.seeding
            .append_event_for_testing(&self.process_id, outcome.append_request())
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
        self.read_view_with_retention(
            limits,
            lash_core_store::execution_limits::EvidenceLimits::standard()
                .process_effect_occurrences(),
        )
        .await
    }

    async fn read_view_with_retention(
        &self,
        limits: lash_trace::ObservationWorkLimits,
        cap: u64,
    ) -> ProcessReadView {
        let replay = Arc::new(lash_core::InMemoryProcessReplayStore::new(
            lash_core::InMemoryProcessReplayStoreConfig::standard(),
        ));
        let publisher = Arc::new(
            crate::language_observation::LanguageObservationPublisher::new(
                replay.clone(),
                Arc::new(lash_core::facade_support::InMemoryLiveReplayStore::new(
                    lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
                )),
                lash_trace::ObservationWorkLimits::standard(),
            ),
        );
        crate::process_feed::ProcessFeedSource::new(
            self.process_id.clone(),
            Arc::clone(&self.registry),
            lash_execution_contracts::process::ProcessWorkObserver::new(Arc::clone(&self.registry))
                .with_work_limits(limits),
            lash_core::ProcessEngineRegistry::default(),
            replay,
            limits,
            crate::process_feed::FeedReconcile {
                publisher,
                changes: lash_core::runtime::ProcessChangeHub::new(),
            },
        )
        .with_effect_retention(cap)
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
    lash_execution_contracts::process::ProcessRetention::release_process_events(
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
    let ProcessEventReadOutcome::Retained(lash_execution_contracts::process::ProcessEventPage {
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

/// FIG-6266: the read side must accommodate the supported per-node retention.
#[tokio::test]
async fn maximum_retention_four_nodes_of_5000_has_complete_effect_evidence() {
    let fixture = Fixture::new("maximum-effect-retention").await;
    let cap = lash_core_store::execution_limits::EvidenceLimitsField::ProcessEffectOccurrences
        .bounds()
        .max;
    lash_core_store::execution_limits::EvidenceLimits::builder()
        .process_effect_occurrences(cap)
        .build()
        .expect("the documented maximum is admitted");
    let omissions = fixture.commit_effect_occurrences(4, 5000, cap).await;
    let ProcessReadView::Retained(live) = fixture
        .read_view_with_retention(lash_trace::ObservationWorkLimits::standard(), cap)
        .await
    else {
        panic!("retained running process");
    };
    assert_eq!(live.effects.coverage, ProcessEffectCoverage::Complete);
    assert_eq!(live.effects.report.nodes().len(), 4);
    for node in live.effects.report.nodes() {
        assert_eq!(node.occurrences.len() as u64, cap);
        assert_eq!(node.omitted.total(), 0, "no terminal omission record yet");
    }
    fixture.complete_effect_history(omissions).await;
    // A replacement host can select a lower cut without making the old
    // process's recorded terminal fold unreadable.
    let ProcessReadView::Retained(view) = fixture
        .read_view(lash_trace::ObservationWorkLimits::standard())
        .await
    else {
        panic!("retained process");
    };
    assert_eq!(view.effects.coverage, ProcessEffectCoverage::Complete);
    assert_eq!(
        view.effects.observed_through.as_u64(),
        fixture.high_water().await
    );
    assert_eq!(view.effects.report.nodes().len(), 4);
    for node in view.effects.report.nodes() {
        assert_eq!(node.occurrences.len() as u64, cap);
        assert_eq!(node.omitted.success, 5000 - cap);
    }
}

/// FIG-6266: omission counts have priority over any occurrence acquisition budget.
#[tokio::test]
async fn effect_omissions_survive_zero_and_tiny_acquisition_budgets() {
    let fixture = Fixture::new("omission-priority").await;
    let omissions = fixture.commit_effect_occurrences(2, 10, 4).await;
    fixture.complete_effect_history(omissions).await;
    // Observer audit events may follow a terminal; the omission is not always
    // one of the last two events of the snapshot.
    for n in 0..4 {
        fixture
            .registry
            .add_observer(
                &lash_vocabulary::identity::SessionId::fixture(format!("late-observer-{n}")),
                &fixture.process_id,
                lash_execution_contracts::process::ProcessObserverBy::host(format!(
                    "late-observer-{n}"
                )),
            )
            .await
            .expect("late observer");
    }
    for (pages, page_size) in [(0, 1), (0, 256), (1, 1), (1, 2), (2, 2)] {
        let ProcessReadView::Retained(view) = fixture
            .read_view(lash_trace::ObservationWorkLimits {
                process_effect_fold_pages: pages,
                process_effect_fold_page_size: std::num::NonZeroUsize::new(page_size).unwrap(),
                ..Default::default()
            })
            .await
        else {
            panic!("retained process");
        };
        assert_eq!(
            view.effects.report.nodes().len(),
            2,
            "budget {pages} × {page_size}"
        );
        for node in view.effects.report.nodes() {
            assert_eq!(node.omitted.success, 6, "budget {pages} × {page_size}");
        }
        assert_eq!(
            view.effects.coverage,
            ProcessEffectCoverage::Incomplete {
                reason: ProcessEffectGapReason::AcquisitionBudgetExhausted,
            }
        );
        assert_eq!(
            view.effects.observed_through.as_u64(),
            (pages * page_size) as u64
        );
    }
    let before_terminal = crate::process_feed::fold_effects(
        fixture.registry.as_ref(),
        &fixture.process_id,
        1,
        crate::process_feed::EffectFoldBudget::of(lash_trace::ObservationWorkLimits {
            process_effect_fold_pages: 0,
            ..Default::default()
        }),
        None,
    )
    .await
    .unwrap()
    .ok()
    .expect("retained earlier revision");
    assert_eq!(
        before_terminal.report.nodes().len(),
        0,
        "no future omission leaks into an earlier revision"
    );
    let key = format!("process:{}:effect-omissions", fixture.process_id);
    let ProcessEventReadOutcome::Retained(Some(omission)) = fixture
        .registry
        .event_by_replay_key(&fixture.process_id, &key, fixture.high_water().await)
        .await
        .unwrap()
    else {
        panic!("retained omission");
    };
    fixture
        .registry
        .release_process_events(&fixture.process_id, omission.sequence)
        .await
        .expect("release the omission explicitly");
    let ProcessReadView::Retained(view) = fixture.read_view(Default::default()).await else {
        panic!("retained process");
    };
    assert_eq!(
        view.effects.report.nodes().len(),
        0,
        "released omission counts are not resurrected"
    );
    assert_eq!(
        view.effects.coverage,
        ProcessEffectCoverage::Incomplete {
            reason: ProcessEffectGapReason::HistoryReleased,
        }
    );
}

impl Fixture {
    /// The durable history of `nodes` nodes settling `count` successful
    /// occurrences apiece, under the run's recorded `cap`.
    async fn commit_effect_occurrences(
        &self,
        nodes: u64,
        count: u64,
        cap: u64,
    ) -> lash_execution_contracts::process::ProcessEffectOmissions {
        let mut pending = Vec::new();
        let mut omitted = std::collections::BTreeMap::new();
        for node in 0..nodes {
            let node_name = format!("node-{node}");
            for occurrence in 0..count.min(cap) {
                pending.push(
                    lash_execution_contracts::process::ProcessEffectOccurrence::new(
                        lash_vocabulary::sites::EffectLocation::authored(
                            lash_protocol_types::effect_identity_fixture(&node_name, occurrence),
                        ),
                        "tools.echo",
                        lash_execution_contracts::process::ProcessEffectOutcomeClass::Success,
                        None,
                        format!("history-effect:{node}:{occurrence}"),
                        lash_core::FleetFormat::current(),
                    )
                    .append_request(),
                );
            }
            omitted.insert(
                lash_protocol_types::effect_identity_fixture(&node_name, 0)
                    .position()
                    .to_string(),
                lash_execution_contracts::process::ProcessEffectOmittedCounts {
                    success: count - count.min(cap),
                    failure: 0,
                    cancelled: 0,
                },
            );
        }
        self.seeding
            .append_events_for_testing(&self.process_id, pending)
            .await
            .expect("commit retained occurrences");
        lash_execution_contracts::process::ProcessEffectOmissions::new(
            omitted,
            cap,
            lash_core::FleetFormat::current(),
        )
    }

    async fn complete_effect_history(
        &self,
        omissions: lash_execution_contracts::process::ProcessEffectOmissions,
    ) {
        self.registry
            .complete_process_with_prelude(
                &self.process_id,
                lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
                    lash_core::ToolCallOutput::success(serde_json::Value::Null),
                ),
                vec![
                    omissions
                        .append_request(format!("process:{}:effect-omissions", self.process_id)),
                ],
                lash_execution_contracts::process::ProcessCompletionAuthority::workflow_key(
                    self.process_id.to_string(),
                ),
            )
            .await
            .expect("commit the terminal effect history");
    }
}

/// FIG-6353: trusted relays can encode every change and its page without
/// reconstructing the decoder's wire shape in host code.
#[tokio::test]
async fn durable_change_page_round_trips_upsert_and_tombstone() {
    use lash_execution_contracts::process::ObservedProcessChange;
    use lash_execution_contracts::process::ObservedProcessChangeKind;
    use lash_execution_contracts::process::ProcessWorkObserver;
    let fixture = Fixture::new("durable-change-page").await;
    fixture
        .registry
        .complete_process(
            &fixture.process_id,
            lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
                lash_core::ToolCallOutput::success(
                    serde_json::json!({"$lash_tool_value": "foreign", "answer": 42}),
                ),
            ),
            lash_execution_contracts::process::ProcessCompletionAuthority::workflow_key(
                &fixture.process_id,
            ),
        )
        .await
        .expect("complete process");
    let process = ProcessWorkObserver::new(fixture.registry.clone())
        .process(&fixture.process_id)
        .await
        .expect("read")
        .expect("retained");
    let page = crate::ChangePage {
        changes: vec![
            ObservedProcessChange {
                cursor: lash_execution_contracts::process::ProcessChangeCursor::from_store_sequence(
                    7,
                ),
                kind: ObservedProcessChangeKind::Upsert {
                    process: Box::new(process),
                },
            },
            ObservedProcessChange {
                cursor: lash_execution_contracts::process::ProcessChangeCursor::from_store_sequence(
                    8,
                ),
                kind: ObservedProcessChangeKind::Deleted {
                    tombstone: lash_execution_contracts::process::ProcessTombstone {
                        process_id: fixture.process_id.clone(),
                        terminal_label:
                            lash_execution_contracts::process::RetiredProcessStatus::Completed,
                        pruned_at_ms: 123,
                        pruned_change_seq: 8,
                    },
                },
            },
        ],
        next: lash_execution_contracts::process::ProcessChangeCursor::from_store_sequence(8),
        retained_after: Some(
            lash_execution_contracts::process::ProcessChangeCursor::from_store_sequence(3),
        ),
    };
    for change in &page.changes {
        let encoded =
            serde_json::to_value(lash_protocol_types::Durable(change)).expect("encode change");
        let decoded: ObservedProcessChange =
            serde_json::from_value(encoded).expect("decode change");
        assert_eq!(&decoded, change);
    }
    let encoded = serde_json::to_value(lash_protocol_types::Durable(&page)).expect("encode page");
    assert_eq!(encoded["changes"][0]["kind"], "upsert");
    assert_eq!(encoded["changes"][1]["kind"], "deleted");
    let decoded: crate::ChangePage<
        ObservedProcessChange,
        lash_execution_contracts::process::ProcessChangeCursor,
    > = serde_json::from_value(encoded).expect("decode page");
    assert_eq!(decoded, page);
}

/// FIG-6353: encoding an events read retains full facts, lite metadata,
/// retention outcomes and the next-page continuation accepted by its decoder.
#[tokio::test]
async fn durable_events_read_round_trips_with_next_page() {
    let fixture = Fixture::new("durable-events-read").await;
    fixture.commit_outcome(1).await;
    fixture.commit_outcome(2).await;
    for mode in [ProcessEventQueryMode::Full, ProcessEventQueryMode::Lite] {
        let read = read_events(
            &fixture.registry,
            ProcessHistoryContinuation::start(fixture.process_id.clone()),
            std::num::NonZeroUsize::new(1).expect("nonzero"),
            mode,
        )
        .await
        .expect("read events");
        let ProcessEventReadOutcome::Retained(page) = &read.outcome else {
            panic!("retained")
        };
        assert!(matches!(page.more, ProcessEventPageMore::More { .. }));
        let encoded =
            serde_json::to_value(lash_protocol_types::Durable(&read)).expect("encode read");
        let decoded: ProcessEventsRead = serde_json::from_value(encoded).expect("decode read");
        assert_eq!(decoded.outcome, read.outcome);
        assert_eq!(decoded.next, read.next);
    }
    let read = ProcessEventsRead {
        outcome: ProcessEventReadOutcome::NoLongerRetained(
            ProcessEventHistoryRetention::Released {
                released_through: 2,
            },
        ),
        next: ProcessHistoryContinuation::after(fixture.process_id, 2),
    };
    let decoded: ProcessEventsRead = serde_json::from_value(
        serde_json::to_value(lash_protocol_types::Durable(&read)).expect("encode retention"),
    )
    .expect("decode retention");
    assert_eq!(decoded.outcome, read.outcome);
    assert_eq!(decoded.next, read.next);
}
