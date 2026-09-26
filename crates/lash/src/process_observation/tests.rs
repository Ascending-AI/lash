//! Law L8 (FIG-3571 §E): one process cursor across durable history and the
//! live hub, over a real SQLite registry.
//!
//! Durable commits reach the hub the way the ADR 0017 sink delivers them —
//! after the commit, as `Committed` items — and a test that withholds that
//! call is a lost or late publication.

use super::*;
use lash_trace::{
    TraceContext, TraceLanguageExecution, TraceLanguageExecutionGeneration,
    TraceLanguageExecutionIdentity, TraceLanguageExecutionMap, TraceLanguageExecutionMapNode,
    TraceRuntimeScope,
};

const TICK: &str = "fixture.tick";

fn record(process_id: &ProcessId, attempt: u32, occurrence: u64) -> TraceRecord {
    let payload = if occurrence == 0 {
        TraceLanguageExecutionPayload::ExecutionStarted {
            execution_map: TraceLanguageExecutionMap {
                nodes: vec![TraceLanguageExecutionMapNode {
                    id: "node".to_string(),
                    site: lash_sansio::WorkflowExecutionSite::new(
                        "main",
                        [0],
                        lash_sansio::ExecutionNodeKind::Call,
                        "call()",
                    ),
                    kind: lash_sansio::ExecutionNodeKind::Call,
                    label: "call()".to_string(),
                    branch_memberships: Vec::new(),
                    label_metadata: None,
                }],
                edges: Vec::new(),
            },
        }
    } else {
        TraceLanguageExecutionPayload::NodeStarted {
            node_id: "node".to_string(),
            node_kind: lash_sansio::ExecutionNodeKind::Call,
            label: "call()".to_string(),
            occurrence,
            call_id: None,
        }
    };
    TraceRecord::new(
        TraceContext::default(),
        TraceEvent::LanguageExecution {
            language: "lashlang".to_string(),
            event: TraceLanguageExecution {
                event_key: format!("{process_id}:{attempt}:{occurrence}"),
                identity: TraceLanguageExecutionIdentity {
                    scope: TraceRuntimeScope::none(),
                    subject: TraceRuntimeSubject::Process {
                        process_id: process_id.clone(),
                    },
                    source_identity: "source".to_string(),
                    module_ref: "module".to_string(),
                    entry_kind: "main".to_string(),
                    entry_ref: None,
                    entry_name: "main".to_string(),
                    engine_execution_id: None,
                    generation: Some(TraceLanguageExecutionGeneration::new(attempt)),
                },
                payload,
            },
        },
    )
}

fn finished_record(process_id: &ProcessId) -> TraceRecord {
    let mut finished = record(process_id, 1, 1);
    let TraceEvent::LanguageExecution { event, .. } = &mut finished.event else {
        unreachable!()
    };
    event.payload = TraceLanguageExecutionPayload::ExecutionFinished {
        status: lash_trace::TraceLanguageExecutionStatus::Completed,
        error: None,
    };
    finished
}

fn tick_type() -> lash_core::ProcessEventType {
    lash_core::ProcessEventType {
        name: TICK.to_string(),
        payload_schema: lash_core::LashSchema::any(),
        semantics: lash_core::ProcessEventSemanticsSpec::default(),
    }
}

/// An engine process with an execution lease (for runtime-owned summary
/// events) or a host-owned one (so a test can complete and prune it).
fn registration(label: &str, leased: bool) -> lash_core::ProcessRegistration {
    let (input, contract) = if leased {
        (
            lash_core::ProcessInput::Engine {
                kind: "l8-fixture".to_string(),
                payload: serde_json::Value::Null,
            },
            lash_core::RecoveryContract::Rerunnable,
        )
    } else {
        (
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::RecoveryContract::ExternallyOwned,
        )
    };
    let registration = lash_core::ProcessRegistration::new(
        input,
        contract,
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
    .with_extra_event_types([tick_type()]);
    if leased {
        registration
            .with_execution_env_ref(Some(lash_core::ProcessExecutionEnvRef::new("l8-env")))
            .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
                lash_core::ProcessIdentity::for_definition(
                    lash_core::ProcessDefinitionRef::unclaimed(
                        "l8-fixture",
                        serde_json::Value::Null,
                    ),
                    Some(label),
                ),
            ))
    } else {
        registration
    }
}

struct Fixture {
    _dir: Option<tempfile::TempDir>,
    registry: Arc<dyn ProcessRegistry>,
    hub: Arc<ProcessObservationHub>,
    process_id: ProcessId,
    lease: Option<lash_core::ProcessLease>,
}

impl Fixture {
    async fn new(name: &str, leased: bool) -> Self {
        Self::with_config(name, leased, ProcessObservationConfig::default()).await
    }

    async fn with_config(name: &str, leased: bool, config: ProcessObservationConfig) -> Self {
        // A leased fixture stays on the file registry: `claim_process_lease`
        // below is an S6 fence signal (D5). An unleased fixture only needs
        // the store port, so it runs on the memory store set.
        let (dir, registry): (_, Arc<dyn ProcessRegistry>) = if leased {
            let dir = tempfile::tempdir().expect("L8 tempdir");
            let registry = Arc::new(
                lash_sqlite_store::SqliteProcessRegistry::open(
                    &dir.path().join("processes.db"),
                    dir.path().join("sessions"),
                )
                .await
                .expect("open SQLite process registry"),
            );
            (Some(dir), registry)
        } else {
            (
                None,
                crate::tests::memory_store_set().await.process_registry(),
            )
        };
        let process_id = registry
            .register_process(registration(name, leased))
            .await
            .expect("register L8 process")
            .id;
        let lease = if leased {
            registry
                .claim_process_lease(
                    &process_id,
                    &lash_core::LeaseOwnerIdentity::opaque("l8", "l8:1"),
                    60_000,
                )
                .await
                .expect("claim L8 lease")
                .acquired()
        } else {
            None
        };
        Self {
            _dir: dir,
            registry,
            hub: Arc::new(ProcessObservationHub::new(config)),
            process_id,
            lease,
        }
    }

    /// Commit one durable event; `publish` is the ADR 0017 sink delivering it.
    async fn commit(&self, publish: bool) -> ProcessEvent {
        let event = self
            .registry
            .append_event(
                &self.process_id,
                lash_core::ProcessEventAppendRequest::new(TICK, serde_json::json!({})),
            )
            .await
            .expect("commit a tick")
            .event;
        if publish {
            self.hub.publish_committed(&event);
        }
        event
    }

    /// Commit one runtime-owned effect-summary occurrence.
    async fn commit_outcome(&self, occurrence: u64, publish: bool) -> ProcessEvent {
        let outcome = lash_core::ProcessEffectSummaryOccurrence::new(
            "node",
            occurrence,
            "tools.echo",
            if occurrence.is_multiple_of(2) {
                lash_core::ProcessEffectOutcomeClass::Failure
            } else {
                lash_core::ProcessEffectOutcomeClass::Success
            },
            None,
            format!("l8-effect:{occurrence}"),
            lash_core::FleetFormat::current(),
        );
        let event = self
            .registry
            .append_event_with_authority(
                &self.process_id,
                outcome.append_request(),
                &lash_core::ProcessExecutionWriteAuthority::lease(
                    self.lease.clone().expect("a leased fixture"),
                ),
            )
            .await
            .expect("commit an effect outcome")
            .event;
        if publish {
            self.hub.publish_committed(&event);
        }
        event
    }

    fn live(&self, attempt: u32, occurrence: u64) {
        self.hub
            .append(&record(&self.process_id, attempt, occurrence))
            .expect("publish a live observation");
    }

    async fn subscribe(&self, from: Option<&ProcessCursor>) -> ProcessObservationSubscription {
        self.hub
            .subscribe(Arc::clone(&self.registry), &self.process_id, from)
            .await
            .expect("subscribe")
    }

    async fn high_water(&self) -> u64 {
        self.registry
            .get_process(&self.process_id)
            .await
            .expect("read record")
            .expect("record")
            .last_event_sequence
    }

    /// The effect-summary fold through `high_water`, read independently of
    /// the hub.
    async fn fold_through(&self, high_water: u64) -> ProcessEffectSummary {
        let mut summary = ProcessEffectSummary::default();
        let limit = std::num::NonZeroUsize::new(4096).expect("page size");
        let ProcessEventReadOutcome::Retained(page) = self
            .registry
            .event_page_after(&self.process_id, 0, limit, ProcessEventQueryMode::Full)
            .await
            .expect("read history")
        else {
            panic!("the fixture history is retained");
        };
        let ProcessEventPageEvents::Full(events) = page.events else {
            panic!("full page")
        };
        for event in events.iter().filter(|event| event.sequence <= high_water) {
            summary
                .fold_event(
                    &event.event_type,
                    &event.payload,
                    lash_core::FleetFormat::current(),
                )
                .expect("fold");
        }
        summary
    }
}

async fn next(subscription: &mut ProcessObservationSubscription) -> ProcessObservationItem {
    tokio::time::timeout(Duration::from_secs(5), subscription.recv())
        .await
        .expect("an item arrives")
        .expect("recv")
        .expect("the stream continues")
}

async fn quiet(subscription: &mut ProcessObservationSubscription) {
    assert!(
        tokio::time::timeout(Duration::from_millis(50), subscription.recv())
            .await
            .is_err(),
        "nothing is pending"
    );
}

fn snapshot_cursor(item: &ProcessObservationItem) -> ProcessCursor {
    match item {
        ProcessObservationItem::Snapshot { cursor, .. } => cursor.clone(),
        other => panic!("expected a snapshot, got {other:?}"),
    }
}

#[track_caller]
fn expect_gap(
    item: &ProcessObservationItem,
    expected: ProcessObservationGapReason,
) -> (ProcessCursor, ProcessObservationSnapshot) {
    let ProcessObservationItem::Gap {
        cursor,
        reason,
        snapshot,
        ..
    } = item
    else {
        panic!("expected a {expected:?} gap, got {item:?}");
    };
    assert_eq!(*reason, expected);
    (cursor.clone(), snapshot.clone())
}

#[track_caller]
fn expect_committed(item: &ProcessObservationItem, sequence: u64) -> ProcessCursor {
    let ProcessObservationItem::Committed {
        cursor,
        sequence: observed,
        ..
    } = item
    else {
        panic!("expected Committed {{ {sequence} }}, got {item:?}");
    };
    assert_eq!(*observed, sequence);
    assert_eq!(
        cursor.sequence(),
        sequence,
        "the cursor carries the durable mark"
    );
    cursor.clone()
}

fn durable_sequence(snapshot: &ProcessObservationSnapshot) -> u64 {
    match &snapshot.durable {
        ProcessDurableSnapshot::Retained { sequence, .. } => *sequence,
        other => panic!("expected a retained snapshot, got {other:?}"),
    }
}

/// L8: a cursor whose sequence the ring bridges with contiguous `Committed`
/// evidence resumes silently: no snapshot, no gap, every commit and live
/// observation after it, each carrying the advanced cursor.
#[tokio::test]
async fn l8_sequence_bridging_resumes_after_the_cursor() {
    let fixture = Fixture::new("l8-bridge", false).await;
    fixture.live(1, 0);
    let mut first = fixture.subscribe(None).await;
    let snapshot = next(&mut first).await;
    let start = snapshot_cursor(&snapshot);
    let ProcessObservationItem::Snapshot { snapshot, .. } = &snapshot else {
        unreachable!()
    };
    let high_water = fixture.high_water().await;
    assert_eq!(start.sequence(), high_water);
    assert_eq!(durable_sequence(snapshot), high_water);
    assert!(snapshot.live.graph.is_some());
    assert_eq!(
        snapshot.live.completeness,
        ProcessObservationCompleteness::Complete,
        "a publisher joined at start reports a complete live graph"
    );
    drop(first);

    let one = fixture.commit(true).await;
    fixture.live(1, 1);
    let two = fixture.commit(true).await;

    let mut resumed = fixture.subscribe(Some(&start)).await;
    let after_one = expect_committed(&next(&mut resumed).await, one.sequence);
    let ProcessObservationItem::Event { cursor, .. } = next(&mut resumed).await else {
        panic!("the live observation between the commits replays in order");
    };
    assert_eq!(cursor.sequence(), one.sequence);
    assert_eq!(cursor.position(), after_one.position() + 1);
    expect_committed(&next(&mut resumed).await, two.sequence);
    quiet(&mut resumed).await;

    // A live subscriber receives later commits as they publish.
    let three = fixture.commit(true).await;
    expect_committed(&next(&mut resumed).await, three.sequence);
}

/// L8: a commit whose publication has not reached the hub yet cannot be
/// bridged, so it answers a gap at the durable high-water mark — never a
/// silent skip — and the late publication is not delivered twice.
#[tokio::test]
async fn l8_commit_to_publication_races_never_skip_silently() {
    let fixture = Fixture::new("l8-race", false).await;
    let mut first = fixture.subscribe(None).await;
    let start = snapshot_cursor(&next(&mut first).await);
    drop(first);

    let committed = fixture.commit(false).await;
    let mut reconnect = fixture.subscribe(Some(&start)).await;
    let (cursor, snapshot) = expect_gap(
        &next(&mut reconnect).await,
        ProcessObservationGapReason::SequenceUnbridged,
    );
    assert_eq!(durable_sequence(&snapshot), committed.sequence);
    assert_eq!(cursor.sequence(), committed.sequence);

    // The late publication is already folded into the snapshot.
    fixture.hub.publish_committed(&committed);
    let later = fixture.commit(true).await;
    expect_committed(&next(&mut reconnect).await, later.sequence);

    // A live subscriber meeting a publication that jumps past a lost one
    // re-reads the durable log and continues from its new boundary.
    let lost = fixture.commit(false).await;
    let after = fixture.commit(true).await;
    let (cursor, snapshot) = expect_gap(
        &next(&mut reconnect).await,
        ProcessObservationGapReason::SequenceUnbridged,
    );
    assert!(durable_sequence(&snapshot) >= after.sequence);
    assert!(cursor.sequence() > lost.sequence);
    let last = fixture.commit(true).await;
    expect_committed(&next(&mut reconnect).await, last.sequence);
}

/// L8: an idle process whose last publication was lost answers a reconnect
/// with a gap whose snapshot is exactly the fold through the named
/// high-water mark.
#[tokio::test]
async fn l8_idle_reconnect_after_a_lost_publication_folds_through_the_high_water_mark() {
    let fixture = Fixture::new("l8-idle", true).await;
    for occurrence in 1..=3 {
        fixture.commit_outcome(occurrence, true).await;
    }
    let mut first = fixture.subscribe(None).await;
    let start = snapshot_cursor(&next(&mut first).await);
    drop(first);
    let lost = fixture.commit_outcome(4, false).await;

    let mut reconnect = fixture.subscribe(Some(&start)).await;
    let (cursor, snapshot) = expect_gap(
        &next(&mut reconnect).await,
        ProcessObservationGapReason::SequenceUnbridged,
    );
    let high_water = fixture.high_water().await;
    assert_eq!(high_water, lost.sequence);
    assert_eq!(cursor.sequence(), high_water);
    let ProcessDurableSnapshot::Retained {
        sequence,
        status,
        summary,
        completeness,
    } = &snapshot.durable
    else {
        panic!("a live process is retained");
    };
    assert_eq!(*sequence, high_water);
    assert_eq!(*status, ProcessStatus::Running);
    assert_eq!(*completeness, ProcessDurableCompleteness::Complete);
    assert_eq!(summary, &fixture.fold_through(high_water).await);
    assert_eq!(
        summary
            .node("node")
            .expect("summary node")
            .occurrences
            .len(),
        4
    );
    assert_eq!(
        snapshot.live.completeness,
        ProcessObservationCompleteness::Incomplete {
            reason: ProcessObservationGapReason::RoutingUnavailable
        },
        "absent live telemetry is reported, never a Skipped graph"
    );
    assert!(snapshot.live.graph.is_none());
}

/// L8: epoch replacement, lag and ring trim answer typed gaps with a
/// snapshot and a new cursor.
#[tokio::test]
async fn l8_epoch_replacement_lag_and_trim_are_gaps_with_snapshots() {
    let fixture = Fixture::new("l8-epoch", false).await;
    fixture.live(1, 0);
    let mut live = fixture.subscribe(None).await;
    let start = snapshot_cursor(&next(&mut live).await);
    fixture.live(2, 0);
    let (replaced, _) = expect_gap(
        &next(&mut live).await,
        ProcessObservationGapReason::PublisherReplaced,
    );
    assert_ne!(replaced.epoch(), start.epoch());
    // The subscription continues from its new boundary.
    fixture.live(2, 1);
    assert!(matches!(
        next(&mut live).await,
        ProcessObservationItem::Event { .. }
    ));
    let mut resumed = fixture.subscribe(Some(&start)).await;
    expect_gap(
        &next(&mut resumed).await,
        ProcessObservationGapReason::PublisherReplaced,
    );

    // A restarted core has no route under the old epoch.
    let restarted = Arc::new(ProcessObservationHub::default());
    let mut after_restart = restarted
        .subscribe(
            Arc::clone(&fixture.registry),
            &fixture.process_id,
            Some(&start),
        )
        .await
        .expect("subscribe");
    expect_gap(
        &next(&mut after_restart).await,
        ProcessObservationGapReason::RoutingUnavailable,
    );

    // Trim: a one-item ring loses the cursor's successor.
    let small = ProcessObservationConfig {
        capacity: 1,
        ..ProcessObservationConfig::default()
    };
    let trimmed = Fixture::with_config("l8-trim", false, small).await;
    trimmed.live(1, 0);
    let mut first = trimmed.subscribe(None).await;
    let start = snapshot_cursor(&next(&mut first).await);
    drop(first);
    trimmed.live(1, 1);
    trimmed.live(1, 2);
    let mut late = trimmed.subscribe(Some(&start)).await;
    expect_gap(
        &next(&mut late).await,
        ProcessObservationGapReason::Overflow,
    );

    // Lag: the broadcast outruns a subscriber that does not read.
    let mut lagging = trimmed.subscribe(None).await;
    snapshot_cursor(&next(&mut lagging).await);
    trimmed.live(1, 3);
    trimmed.live(1, 4);
    trimmed.live(1, 5);
    expect_gap(
        &next(&mut lagging).await,
        ProcessObservationGapReason::SubscriberLagged,
    );

    // Expiry: a zero TTL expires the cursor's successor.
    let expiring = Fixture::with_config(
        "l8-expiry",
        false,
        ProcessObservationConfig {
            capacity: 8,
            ttl: Duration::ZERO,
            ..ProcessObservationConfig::default()
        },
    )
    .await;
    expiring.live(1, 0);
    // The held subscription keeps the route alive through the idle sweep.
    let mut first = expiring.subscribe(None).await;
    let start = snapshot_cursor(&next(&mut first).await);
    expiring.live(1, 1);
    tokio::time::sleep(Duration::from_millis(2)).await;
    expiring.live(1, 2);
    let mut late = expiring.subscribe(Some(&start)).await;
    expect_gap(&next(&mut late).await, ProcessObservationGapReason::Expired);
}

/// L8: prune, a successor started for the same work, another process and an
/// unknown process are typed gaps; a process that is gone ends the stream
/// after its gap.
#[tokio::test]
async fn l8_prune_and_a_successor_are_typed_retention_gaps() {
    let fixture = Fixture::new("l8-prune", false).await;
    let mut first = fixture.subscribe(None).await;
    let start = snapshot_cursor(&next(&mut first).await);
    drop(first);
    let terminal = fixture
        .registry
        .complete_process(
            &fixture.process_id,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete");
    fixture
        .registry
        .prune_terminal_processes(
            terminal.updated_at_ms.saturating_add(1),
            None,
            lash_core::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune");
    let mut pruned = fixture.subscribe(Some(&start)).await;
    let (_, snapshot) = expect_gap(
        &next(&mut pruned).await,
        ProcessObservationGapReason::HistoryUnavailable,
    );
    assert!(matches!(
        snapshot.durable,
        ProcessDurableSnapshot::NoLongerRetained(ProcessEventHistoryRetention::Pruned { .. })
    ));
    assert!(
        pruned.recv().await.expect("recv").is_none(),
        "a pruned lifetime has nothing to follow"
    );

    // FIG-3611: an id is never reused. Registering the same process again
    // mints a new id, and the pruned id's cursor still reads as pruned — it
    // never follows the successor (ADR 0107).
    let successor = fixture
        .registry
        .register_process(registration("l8-prune", false))
        .await
        .expect("register a successor");
    assert_ne!(successor.id, fixture.process_id);
    let mut old = fixture.subscribe(Some(&start)).await;
    let (_, snapshot) = expect_gap(
        &next(&mut old).await,
        ProcessObservationGapReason::HistoryUnavailable,
    );
    assert!(matches!(
        snapshot.durable,
        ProcessDurableSnapshot::NoLongerRetained(ProcessEventHistoryRetention::Pruned { .. })
    ));
    let mut new = fixture
        .hub
        .subscribe(Arc::clone(&fixture.registry), &successor.id, Some(&start))
        .await
        .expect("subscribe the successor");
    expect_gap(
        &next(&mut new).await,
        ProcessObservationGapReason::CrossProcess,
    );

    let other = Fixture::new("l8-other", false).await;
    let mut cross = other.subscribe(Some(&start)).await;
    expect_gap(
        &next(&mut cross).await,
        ProcessObservationGapReason::CrossProcess,
    );

    let mut unknown = fixture
        .hub
        .subscribe(
            Arc::clone(&fixture.registry),
            &lash_core::mint_process_id(),
            None,
        )
        .await
        .expect("subscribe unknown");
    let (_, snapshot) = expect_gap(
        &next(&mut unknown).await,
        ProcessObservationGapReason::HistoryUnavailable,
    );
    assert_eq!(snapshot.durable, ProcessDurableSnapshot::Unknown);
}

/// L8: one cursor pages the durable history in Full or Lite — the projection
/// is a request parameter — and, at the high-water mark, resumes the live
/// stream without a gap.
#[tokio::test]
async fn l8_full_and_lite_history_page_through_one_cursor() {
    let fixture = Fixture::new("l8-pages", false).await;
    for _ in 0..5 {
        fixture.commit(true).await;
    }
    let limit = std::num::NonZeroUsize::new(2).expect("page size");
    let mut from = ProcessEventsFrom::Start(fixture.process_id.clone());
    let mut sequences = Vec::new();
    let mut mode = ProcessEventQueryMode::Lite;
    let cursor = loop {
        let read = read_events(
            &fixture.registry,
            Some(fixture.hub.as_ref()),
            from,
            limit,
            mode,
        )
        .await
        .expect("read a page");
        let ProcessEventReadOutcome::Retained(page) = read.outcome else {
            panic!("retained history");
        };
        let page_sequences = match &page.events {
            ProcessEventPageEvents::Full(events) => {
                assert_eq!(mode, ProcessEventQueryMode::Full);
                events
                    .iter()
                    .map(|event| event.sequence)
                    .collect::<Vec<_>>()
            }
            ProcessEventPageEvents::Lite(events) => {
                assert_eq!(mode, ProcessEventQueryMode::Lite);
                events.iter().map(|event| event.sequence).collect()
            }
        };
        let cursor = read.cursor.expect("a named lifetime");
        assert_eq!(Some(&cursor.sequence()), page_sequences.last());
        sequences.extend(page_sequences);
        mode = match mode {
            ProcessEventQueryMode::Lite => ProcessEventQueryMode::Full,
            ProcessEventQueryMode::Full => ProcessEventQueryMode::Lite,
        };
        match page.more {
            ProcessEventPageMore::More { .. } => from = ProcessEventsFrom::After(cursor),
            ProcessEventPageMore::Complete => break cursor,
        }
    };
    let high_water = fixture.high_water().await;
    assert_eq!(sequences, (1..=high_water).collect::<Vec<_>>());
    assert_eq!(cursor.sequence(), high_water);

    let mut live = fixture.subscribe(Some(&cursor)).await;
    quiet(&mut live).await;
    let next_commit = fixture.commit(true).await;
    expect_committed(&next(&mut live).await, next_commit.sequence);

    // A retired cursor version is refused by name.
    assert_eq!(
        ProcessCursor::parse("lashpc1:epoch:1:1:l8-pages"),
        Err(ProcessCursorError::RetiredVersion {
            found: "lashpc1".to_string()
        })
    );
}

/// The snapshot's durable acquisition is bounded; running out of budget is
/// reported as durable incompleteness, apart from the live graph.
#[tokio::test]
async fn snapshot_acquisition_is_bounded_and_says_so() {
    let fixture = Fixture::with_config(
        "l8-budget",
        true,
        ProcessObservationConfig {
            snapshot_page_budget: 1,
            snapshot_page_size: std::num::NonZeroUsize::new(2).expect("page size"),
            ..ProcessObservationConfig::default()
        },
    )
    .await;
    for occurrence in 1..=4 {
        fixture.commit_outcome(occurrence, true).await;
    }
    let mut subscription = fixture.subscribe(None).await;
    let ProcessObservationItem::Snapshot { snapshot, .. } = next(&mut subscription).await else {
        panic!("snapshot");
    };
    let ProcessDurableSnapshot::Retained {
        sequence,
        completeness,
        ..
    } = snapshot.durable
    else {
        panic!("retained");
    };
    assert_eq!(sequence, fixture.high_water().await);
    assert_eq!(
        completeness,
        ProcessDurableCompleteness::Incomplete {
            reason: ProcessDurableGapReason::AcquisitionBudgetExhausted
        }
    );
}

#[tokio::test]
async fn publisher_joined_mid_run_never_claims_a_complete_live_graph() {
    let fixture = Fixture::new("l8-joined-late", false).await;
    fixture.live(1, 1);
    let mut subscription = fixture.subscribe(None).await;
    let ProcessObservationItem::Snapshot { snapshot, .. } = next(&mut subscription).await else {
        panic!("snapshot");
    };
    assert_eq!(
        snapshot.live.completeness,
        ProcessObservationCompleteness::Incomplete {
            reason: ProcessObservationGapReason::PublisherJoinedMidRun
        }
    );
    assert_eq!(
        snapshot.live.graph.expect("graph").completeness,
        TraceLashlangGraphCompleteness::IncompleteMap
    );
}

#[tokio::test]
async fn finished_processes_release_their_hub_entries_without_subscribers() {
    let hub = Arc::new(ProcessObservationHub::default());
    for index in 0..16 {
        let id = ProcessId::fixture(&format!("process:finished:{index}"));
        hub.append(&record(&id, 1, 0)).expect("start");
        hub.append(&finished_record(&id)).expect("finish");
    }
    assert_eq!(hub.states.lock_recover().len(), 0);
}

#[tokio::test]
async fn idle_unfinished_processes_are_released_after_the_ttl() {
    let hub = Arc::new(ProcessObservationHub::new(ProcessObservationConfig {
        capacity: 8,
        ttl: Duration::from_millis(1),
        ..ProcessObservationConfig::default()
    }));
    for index in 0..8 {
        hub.append(&record(
            &ProcessId::fixture(&format!("process:suspended:{index}")),
            1,
            0,
        ))
        .expect("publish");
    }
    tokio::time::sleep(Duration::from_millis(5)).await;
    hub.append(&record(&ProcessId::fixture("process:live"), 1, 0))
        .expect("publish");
    let remaining = hub
        .states
        .lock_recover()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(remaining, [ProcessId::fixture("process:live")]);
}

fn assert_remote_round_trip(item: ProcessObservationItem, process_id: &ProcessId) {
    let remote = item.into_remote(process_id.clone());
    let wire = remote.encode_json().expect("encode remote item");
    assert_eq!(
        lash_remote_protocol::RemoteProcessObservationItem::decode_json(&wire)
            .expect("decode remote item"),
        remote
    );
}

/// The facade wiring: durable commits through the core's watched registry
/// reach the hub through the ADR 0017 sink, `Processes::events` mints the
/// cursor, and the remote request and items round-trip.
#[tokio::test]
async fn the_facade_routes_commits_to_the_hub_and_pages_by_cursor() {
    let dir = tempfile::tempdir().expect("facade tempdir");
    let backend = Arc::new(
        lash_sqlite_store::SqliteBackend::open(dir.path())
            .await
            .expect("open the file backend"),
    );
    let core = crate::tests::standard_core_over(backend.into());
    let watched = core.process_registry();
    let process_id = watched
        .register_process(registration("l8-facade", false))
        .await
        .expect("register")
        .id;

    let read = core
        .processes()
        .events(
            ProcessEventsFrom::Start(process_id.clone()),
            std::num::NonZeroUsize::new(64).expect("page size"),
            ProcessEventQueryMode::Lite,
        )
        .await
        .expect("read history");
    let cursor = read.cursor.clone().expect("cursor");
    let mut subscription = core
        .processes()
        .subscribe_observation(&process_id, Some(&cursor))
        .await
        .expect("subscribe");
    quiet(&mut subscription).await;

    let committed = watched
        .append_event(
            &process_id,
            lash_core::ProcessEventAppendRequest::new(TICK, serde_json::json!({})),
        )
        .await
        .expect("commit through the watched registry")
        .event;
    let item = next(&mut subscription).await;
    expect_committed(&item, committed.sequence);
    assert_remote_round_trip(item, &process_id);

    let request = lash_remote_protocol::RemoteProcessObservationRequest {
        process_id: process_id.clone(),
        cursor: Some(cursor.clone()),
    };
    let request = lash_remote_protocol::RemoteProcessObservationRequest::decode_json(
        &request.encode_json().expect("encode request"),
    )
    .expect("decode request");
    let mut remote = core
        .processes()
        .subscribe_observation_remote(&request)
        .await
        .expect("remote route");
    let item = remote.recv_remote().await.expect("recv").expect("item");
    assert!(
        matches!(
            &item,
            lash_remote_protocol::RemoteProcessObservationItem::Committed { sequence, .. }
                if *sequence == committed.sequence
        ),
        "the remote route resumes from the cursor: {item:?}"
    );

    let mut initial = core
        .processes()
        .subscribe_observation(&process_id, None)
        .await
        .expect("subscribe");
    let snapshot = next(&mut initial).await;
    assert_eq!(snapshot.cursor().sequence(), committed.sequence);
    assert_remote_round_trip(snapshot, &process_id);

    let events = core
        .processes()
        .events_remote(&lash_remote_protocol::RemoteProcessEventsRequest {
            process_id: process_id.clone(),
            limit: std::num::NonZeroUsize::new(64).expect("page size"),
            mode: ProcessEventQueryMode::Full,
            cursor: Some(cursor.clone()),
        })
        .await
        .expect("remote events");
    assert_eq!(events.cursor.sequence(), committed.sequence);
    assert_eq!(events.cursor.epoch(), cursor.epoch());
    let lash_core::ProcessEventReadOutcome::Retained(page) = events.outcome else {
        panic!("retained");
    };
    assert_eq!(
        page.last_sequence(|event| event.sequence, |event| event.sequence),
        Some(committed.sequence)
    );
}
