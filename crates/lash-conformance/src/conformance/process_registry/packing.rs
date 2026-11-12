//! A finished process keeps its terminal event and its parent-end plan on its
//! own row (FIG-7701). Nothing a host observes may tell the packed form from
//! the event and plan rows: these laws pin the parts a review found unproved
//! (r57 store review, finding 3), each with its expected values derived from
//! the steps the law takes.

use super::*;
use lash_core::CancelOrigin;
use pretty_assertions::assert_eq;

/// Backend observation and fault seam for the packed-row laws.
///
/// Integrator class (ADR 0051): **conformance-suite embedders** implement this
/// probe so the shared laws can read a process row's state revision and fault
/// the write that moves a packed plan into the parent-end table.
#[async_trait::async_trait]
pub trait ProcessRowProbe: Send + Sync {
    /// Process `process_id`'s stored state revision.
    async fn state_rev(&self, process_id: &ProcessId) -> i64;

    /// Make every later write into the parent-end plan table fail, so a
    /// prune that moves a packed plan there fails at that write.
    async fn fail_parent_end_plan_writes(&self);

    /// Remove the fault [`Self::fail_parent_end_plan_writes`] installed.
    async fn clear_injected_failure(&self);
}

fn session_registration(originator: &SessionScope) -> ProcessRegistration {
    lash_execution_contracts::testing::held_engine_registration(
        serde_json::Value::Null,
        ProcessProvenance::session(originator.clone()),
        lash_execution_contracts::process::Lifetime::Detached,
    )
}

fn session_filter(originator: &SessionScope) -> ProcessListFilter {
    ProcessListFilter {
        status: ProcessStatusFilter::Any,
        originator: Some(ProcessOriginatorFilter::session(
            originator.session_id.clone(),
        )),
        ..ProcessListFilter::default()
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn prune(
    registry: &Arc<dyn ProcessRegistry>,
    filter: ProcessListFilter,
) -> crate::ProcessPruneReport {
    registry
        .prune_terminal_processes(
            u64::MAX,
            Some(filter),
            crate::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune terminal processes")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn complete(registry: &Arc<dyn ProcessRegistry>, process_id: &ProcessId) {
    registry
        .complete_process(
            process_id,
            settled_success(serde_json::json!({ "done": true })),
            crate::ProcessCompletionAuthority::workflow_key(process_id),
        )
        .await
        .expect("complete the process");
}

/// Register a late `Cancel` child of `scope`: its id when admitted, `None`
/// when refused as `ParentEnded`; any other answer fails the law.
async fn late_child(
    registry: &Arc<dyn ProcessRegistry>,
    originator: &SessionScope,
    scope: &lash_core::ScopeId,
) -> Option<ProcessId> {
    match registry
        .register_process(crate::started_until_starter(
            session_registration(originator),
            scope.clone(),
        ))
        .await
    {
        Ok(record) => Some(record.id),
        Err(PluginError::ParentEnded { .. }) => None,
        Err(error) => panic!("a late child is admitted or refused as ParentEnded: {error:?}"),
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn plan(
    registry: &Arc<dyn ProcessRegistry>,
    scope: &lash_core::ScopeId,
) -> Option<crate::ParentEndPlan> {
    registry
        .get_parent_end_plan(scope)
        .await
        .expect("read the parent-end plan")
}

/// Every feed change after `cursor`, and the cursor after the last one.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn feed_after(
    registry: &Arc<dyn ProcessRegistry>,
    mut cursor: ProcessChangeCursor,
) -> (Vec<crate::ProcessChange>, ProcessChangeCursor) {
    let mut changes = Vec::new();
    loop {
        let (page, next) = registry
            .processes_changed_since(cursor, 64)
            .await
            .expect("read the process change feed");
        if page.is_empty() {
            return (changes, cursor);
        }
        cursor = next;
        changes.extend(page);
    }
}

fn changes_of(changes: &[crate::ProcessChange], process_id: &ProcessId) -> Vec<ProcessRecord> {
    changes
        .iter()
        .filter_map(|change| match &change.kind {
            crate::ProcessChangeKind::Upsert { record } if record.id == *process_id => {
                Some((**record).clone())
            }
            _ => None,
        })
        .collect()
}

/// A start that fails before any owner holds it ends in its own
/// `CancelRequested` event (item 12). Packed on the row, that event keeps its
/// kind: the lite and full pages and its replay key read it as that cancel, a
/// release counts it and leaves the record and the already-terminal refusal
/// as they were, and a prune counts it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_start_failed_cancel_keeps_its_kind_on_the_row(
    registry: Arc<dyn ProcessRegistry>,
    _probe: Arc<dyn ProcessRowProbe>,
) {
    use lash_execution_contracts::process::ProcessEventKind;

    let label = "packed-start-failed";
    let process_id = registry
        .register_process(registration(label))
        .await
        .expect("register a start that fails")
        .id;
    let cancel = || {
        registry.request_process_cancel(
            &process_id,
            CancelOrigin::StartFailed,
            "start:failed".to_owned(),
            None,
        )
    };
    let failed = cancel().await.expect("fold the failed start");
    assert_eq!(
        (failed.status(), failed.last_event_sequence),
        (ProcessStatus::Cancelled, 1),
        "the failed start's cancel is its one event and ends it"
    );
    let limit = std::num::NonZeroUsize::new(16).unwrap_or(std::num::NonZeroUsize::MIN);
    let lite = registry
        .event_page_after(&process_id, 0, limit, crate::ProcessEventQueryMode::Lite)
        .await
        .expect("read the lite page");
    let crate::ProcessEventReadOutcome::Retained(crate::ProcessEventPage {
        events: crate::ProcessEventPageEvents::Lite(lite),
        ..
    }) = lite
    else {
        panic!("expected a retained lite page, got {lite:?}");
    };
    assert_eq!(
        lite.iter()
            .map(|event| (event.sequence, event.kind))
            .collect::<Vec<_>>(),
        vec![(1, ProcessEventKind::CancelRequested)],
        "the packed terminal keeps the cancel's kind"
    );
    let full = registry
        .event_page_after(&process_id, 0, limit, crate::ProcessEventQueryMode::Full)
        .await
        .expect("read the full page");
    let crate::ProcessEventReadOutcome::Retained(crate::ProcessEventPage {
        events: crate::ProcessEventPageEvents::Full(full),
        ..
    }) = full
    else {
        panic!("expected a retained full page, got {full:?}");
    };
    let [cancelled] = &full[..] else {
        panic!("the failed start has one event, got {full:?}");
    };
    assert!(
        matches!(&cancelled.fact, crate::ProcessLifecycleFact::CancelRequested(request)
            if request.origin == CancelOrigin::StartFailed),
        "the packed event is the failed start's own cancel: {:?}",
        cancelled.fact
    );
    let key = cancelled
        .invocation
        .effect_replay_key()
        .expect("the cancel is replay-keyed")
        .to_owned();
    assert_eq!(
        registry
            .event_by_replay_key(&process_id, &key, 1)
            .await
            .expect("read the cancel by its replay key"),
        crate::ProcessEventReadOutcome::Retained(Some(cancelled.clone())),
        "the cancel is found by its replay key"
    );
    let refused = |answer: Result<ProcessRecord, PluginError>| {
        matches!(
            answer,
            Err(PluginError::ProcessAlreadyTerminal {
                status: ProcessStatus::Cancelled,
                ..
            })
        )
    };
    assert!(
        refused(cancel().await),
        "another cancel of the cancelled start is refused as already terminal"
    );

    assert_eq!(
        registry
            .release_process_events(&process_id, 1)
            .await
            .expect("release through the cancel"),
        crate::ProcessEventRelease {
            released_through: 1,
            released_events: 1,
        },
        "a release through the packed cancel counts it"
    );
    let full = registry
        .event_page_after(&process_id, 1, limit, crate::ProcessEventQueryMode::Full)
        .await
        .expect("read after the horizon");
    assert!(
        matches!(
            full,
            crate::ProcessEventReadOutcome::Retained(crate::ProcessEventPage {
                events: crate::ProcessEventPageEvents::Full(ref events),
                ..
            }) if events.is_empty()
        ),
        "nothing follows the released cancel, got {full:?}"
    );
    assert!(
        refused(cancel().await),
        "after the release another cancel is still refused as already terminal"
    );
    assert_eq!(
        registry
            .get_process(&process_id)
            .await
            .expect("read the released process"),
        Some(failed),
        "the released process reads as before the release"
    );

    let report = prune(
        &registry,
        ProcessListFilter {
            status: ProcessStatusFilter::Any,
            identity_label: Some(label.to_owned()),
            ..ProcessListFilter::default()
        },
    )
    .await;
    assert_eq!(
        (report.pruned_processes, report.pruned_events),
        (1, 1),
        "a prune counts the packed cancel"
    );
}

/// Packing and reclaiming are physical (item 3): the completion that packs
/// the terminal event and plan is one feed change and moves no state
/// revision, and a reclaim that drops a packed plan from a retained row adds
/// no feed change, changes no record and moves no state revision. Registry
/// writes never move a state revision, so it stays at its registered value.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn packing_and_reclaim_leave_the_feed_and_state_revision(
    registry: Arc<dyn ProcessRegistry>,
    probe: Arc<dyn ProcessRowProbe>,
) {
    let originator = SessionScope::new("packed-feed-session");
    let process_id = registry
        .register_process(session_registration(&originator))
        .await
        .expect("register the process whose row packs")
        .id;
    let scope = lash_core::ScopeId::process(process_id.clone());
    let registered_rev = probe.state_rev(&process_id).await;
    let (_, before) = feed_after(&registry, ProcessChangeCursor::default()).await;

    complete(&registry, &process_id).await;
    let finished = registry
        .get_process(&process_id)
        .await
        .expect("read the finished process")
        .expect("the finished process is retained");
    let (changes, after_completion) = feed_after(&registry, before).await;
    assert_eq!(
        changes_of(&changes, &process_id),
        vec![finished.clone()],
        "the packing completion is one feed change, the finished record"
    );
    assert_eq!(
        probe.state_rev(&process_id).await,
        registered_rev,
        "packing moves no state revision"
    );
    assert!(
        plan(&registry, &scope).await.is_some(),
        "the completion records the plan"
    );

    // A prune whose filter keeps the row still reclaims its childless plan.
    let report = prune(
        &registry,
        ProcessListFilter {
            status: ProcessStatusFilter::Any,
            identity_label: Some("packed-feed-no-such-process".to_owned()),
            ..ProcessListFilter::default()
        },
    )
    .await;
    assert_eq!(
        (report.pruned_processes, report.pruned_events),
        (0, 0),
        "the filter keeps every row"
    );
    assert_eq!(
        plan(&registry, &scope).await,
        None,
        "retention reclaims the packed plan of a kept row"
    );
    let (changes, _) = feed_after(&registry, after_completion).await;
    assert_eq!(
        changes_of(&changes, &process_id),
        Vec::<ProcessRecord>::new(),
        "the reclaim adds no feed change"
    );
    assert_eq!(
        registry
            .get_process(&process_id)
            .await
            .expect("read the kept process"),
        Some(finished),
        "the reclaim changes no record"
    );
    assert_eq!(
        probe.state_rev(&process_id).await,
        registered_rev,
        "the reclaim moves no state revision"
    );
}

/// A late `Cancel` child racing its parent's close, prunes and reclaims
/// (items 4 and 5). While a live child names the ended scope, its plan cannot
/// lawfully expire: once the close has committed, every late child is refused
/// `ParentEnded`, whether the plan is packed on the row or moved to the table
/// by a racing prune, and the first stamp stands. Once no live child names
/// the scope a prune reclaims the plan, a late child is admitted, and no
/// admitted child ever coexists with a standing plan. Every process here has
/// exactly one event, its terminal, so each prune's event count equals its
/// process count.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_late_child_racing_close_prune_and_reclaim_is_refused_until_the_plan_expires(
    registry: Arc<dyn ProcessRegistry>,
    _probe: Arc<dyn ProcessRowProbe>,
) {
    const LATE: usize = 4;
    let sessions = [
        "packed-race-session-0",
        "packed-race-session-1",
        "packed-race-session-2",
        "packed-race-session-3",
    ];
    for (round, session) in sessions.into_iter().enumerate() {
        let originator = SessionScope::new(session);
        let filter = session_filter(&originator);
        let parent = registry
            .register_process(session_registration(&originator))
            .await
            .expect("register the parent")
            .id;
        let scope = lash_core::ScopeId::process(parent.clone());
        let child = registry
            .register_process(crate::started_until_starter(
                session_registration(&originator),
                scope.clone(),
            ))
            .await
            .expect("register the live child that keeps the plan")
            .id;
        let mut pruned = (0_usize, 0_usize);
        let tally = |pruned: &mut (usize, usize), report: crate::ProcessPruneReport| {
            pruned.0 += report.pruned_processes;
            pruned.1 += report.pruned_events;
        };

        // The close races reclaim-only prunes (a filter no row matches) and
        // late children; a child admitted before the close commits is
        // lawful, and lives on.
        let nothing = ProcessListFilter {
            status: ProcessStatusFilter::Any,
            identity_label: Some("packed-race-no-such-process".to_owned()),
            ..ProcessListFilter::default()
        };
        let (_, reclaims, early) = tokio::join!(
            complete(&registry, &parent),
            futures_util::future::join_all((0..2).map(|_| prune(&registry, nothing.clone()))),
            futures_util::future::join_all((0..LATE).map(|_| late_child(
                &registry,
                &originator,
                &scope
            ))),
        );
        for report in reclaims {
            tally(&mut pruned, report);
        }
        let mut live: Vec<ProcessId> = early.into_iter().flatten().collect();
        live.push(child);
        let stamp = plan(&registry, &scope)
            .await
            .expect("the committed close leaves its plan");

        // Close committed, a child still live: the packed plan refuses.
        assert_eq!(
            late_child(&registry, &originator, &scope).await,
            None,
            "round {round}: a late child under the packed plan is refused"
        );
        let (prunes, late) = tokio::join!(
            futures_util::future::join_all((0..2).map(|_| prune(&registry, filter.clone()))),
            futures_util::future::join_all((0..LATE).map(|_| late_child(
                &registry,
                &originator,
                &scope
            ))),
        );
        for report in prunes {
            tally(&mut pruned, report);
        }
        assert_eq!(
            late,
            vec![None; LATE],
            "round {round}: a late child racing the prune that moves the plan is refused"
        );
        assert_eq!(
            plan(&registry, &scope).await,
            Some(stamp.clone()),
            "round {round}: the first stamp stands through the prunes"
        );
        assert_eq!(
            late_child(&registry, &originator, &scope).await,
            None,
            "round {round}: a late child under the moved plan is refused"
        );

        // No live child: the plan may expire, and admission reopens.
        let ended = live.len();
        for id in &live {
            complete(&registry, id).await;
        }
        let (prunes, late) = tokio::join!(
            futures_util::future::join_all((0..2).map(|_| prune(&registry, filter.clone()))),
            futures_util::future::join_all((0..LATE).map(|_| late_child(
                &registry,
                &originator,
                &scope
            ))),
        );
        for report in prunes {
            tally(&mut pruned, report);
        }
        let admitted: Vec<ProcessId> = late.into_iter().flatten().collect();
        if !admitted.is_empty() {
            assert_eq!(
                plan(&registry, &scope).await,
                None,
                "round {round}: a child is admitted only once the plan has expired"
            );
        }
        tally(&mut pruned, prune(&registry, filter.clone()).await);
        assert_eq!(
            plan(&registry, &scope).await,
            None,
            "round {round}: retention reclaims the plan once no live child names the scope"
        );
        assert!(
            late_child(&registry, &originator, &scope).await.is_some(),
            "round {round}: a late child is admitted once the plan has expired"
        );
        assert_eq!(
            pruned,
            (1 + ended, 1 + ended),
            "round {round}: the prunes removed the parent and each ended child, one event each"
        );
    }
}

/// A prune that fails at the write moving a packed plan to the table (item
/// 5) keeps the row and its plan: the plan is on the row or in the table,
/// never neither, and a late child is refused either way. The retried prune
/// moves it and removes the row with its one event.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_prune_failing_at_plan_write_back_keeps_the_plan(
    registry: Arc<dyn ProcessRegistry>,
    probe: Arc<dyn ProcessRowProbe>,
) {
    let originator = SessionScope::new("packed-write-back-session");
    let filter = session_filter(&originator);
    let parent = registry
        .register_process(session_registration(&originator))
        .await
        .expect("register the parent")
        .id;
    let scope = lash_core::ScopeId::process(parent.clone());
    registry
        .register_process(crate::started_until_starter(
            session_registration(&originator),
            scope.clone(),
        ))
        .await
        .expect("register the live child that keeps the plan");
    complete(&registry, &parent).await;
    let stamp = plan(&registry, &scope)
        .await
        .expect("the close records its plan");

    probe.fail_parent_end_plan_writes().await;
    let failed = registry
        .prune_terminal_processes(
            u64::MAX,
            Some(filter.clone()),
            crate::ProjectionWatermark::NoProjector,
        )
        .await;
    probe.clear_injected_failure().await;
    assert!(
        failed.is_err(),
        "the prune fails at the plan's write-back, got {failed:?}"
    );
    assert!(
        registry
            .get_process(&parent)
            .await
            .expect("read the parent after the failed prune")
            .is_some(),
        "the failed prune keeps the row"
    );
    assert_eq!(
        plan(&registry, &scope).await,
        Some(stamp.clone()),
        "the failed prune keeps the plan"
    );
    assert_eq!(
        late_child(&registry, &originator, &scope).await,
        None,
        "a late child is refused after the failed prune"
    );

    let report = prune(&registry, filter).await;
    assert_eq!(
        (report.pruned_processes, report.pruned_events),
        (1, 1),
        "the retried prune removes the parent and its one event"
    );
    assert_eq!(
        plan(&registry, &scope).await,
        Some(stamp),
        "the retried prune moves the plan, first stamp and all"
    );
    assert_eq!(
        late_child(&registry, &originator, &scope).await,
        None,
        "a late child is refused under the moved plan"
    );
}
