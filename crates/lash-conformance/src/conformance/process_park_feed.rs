//! Cross-backend laws for parked processes (FIG-3659 NOW-B).
//!
//! A process parks when its body refuses to replay its journal. The park is
//! a fact on the process record — non-terminal, no terminal evidence — kept
//! across every rerun that refuses again, and every transition it takes is
//! appended to the process park feed in the transaction that moved it. These
//! laws hold every process registry to that shape: the parked list and its
//! summary, the re-park that keeps the park, the fact that ends it, the
//! attempt budget a refusing park exempts, and the feed's compaction horizon.

use super::*;
use crate::store::{
    ParkCancelCause, ParkEventKind, ParkFeedCursor, ParkReason, ParkReasonCode, ProcessParkQuery,
    UnparkCause,
};
use crate::{PluginError, ProcessExecutionWriteAuthority, ProcessRecord};
use lash_sansio::ProcessId;
use pretty_assertions::assert_eq;
use std::collections::BTreeSet;
use std::num::NonZeroUsize;

fn limit(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap_or(NonZeroUsize::MIN)
}

fn divergence(effect_kind: &str) -> ParkReason {
    ParkReason::EffectReplayDivergence {
        effect_kind: effect_kind.to_string(),
        message: format!("the recorded {effect_kind} envelope diverged"),
    }
}

fn cell_divergence() -> ParkReason {
    ParkReason::ReplayDivergence {
        message: "the cell issued a command its journal does not hold".to_string(),
    }
}

/// A process lash executes, so its runs can start and park.
fn parkable() -> ProcessRegistration {
    ProcessRegistration::new(
        ProcessInput::Engine {
            kind: "park-feed-conformance".to_string(),
            payload: serde_json::Value::Null,
        },
        ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
    .with_execution_env_ref(Some(crate::ProcessExecutionEnvRef::new(
        "process-env:park-feed",
    )))
}

/// Record engine invocation `attempt`, returning its write authority and start
/// outcome.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn start_attempt(
    registry: &Arc<dyn ProcessRegistry>,
    id: &ProcessId,
    attempt: u32,
) -> (ProcessExecutionWriteAuthority, crate::ProcessStartOutcome) {
    let authority =
        ProcessExecutionWriteAuthority::invocation(id.clone(), format!("park-owner-{attempt}"))
            .bind_attempt(attempt);
    let outcome = registry
        .record_first_started_with_authority(
            id,
            authority
                .invocation_started()
                .expect("the authority is bound to an attempt"),
            &authority,
        )
        .await
        .expect("record the attempt's start");
    (authority, outcome)
}

/// Register a parkable process, returning its minted id.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn register(registry: &Arc<dyn ProcessRegistry>) -> ProcessId {
    registry
        .register_process(parkable())
        .await
        .expect("register the parkable process")
        .id
}

/// Register a process and park it once under a first attempt, returning its
/// id and the authority that attempt writes under.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn parked(
    registry: &Arc<dyn ProcessRegistry>,
    reason: ParkReason,
) -> (ProcessId, ProcessExecutionWriteAuthority, ProcessRecord) {
    let id = register(registry).await;
    let (authority, _) = start_attempt(registry, &id, 1).await;
    let record = registry
        .park_process_with_authority(&id, reason.into(), &authority)
        .await
        .expect("park the process");
    (id, authority, record)
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn transitions_of(
    registry: &Arc<dyn ProcessRegistry>,
    id: &ProcessId,
) -> Vec<(crate::store::ParkId, ParkEventKind)> {
    registry
        .process_park_feed(ParkFeedCursor::initial(), limit(256))
        .await
        .expect("read the process park feed")
        .events
        .into_iter()
        .filter(|event| event.target == *id)
        .map(|event| (event.park_id, event.kind))
        .collect()
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn list(registry: &Arc<dyn ProcessRegistry>, query: ProcessParkQuery) -> Vec<ProcessId> {
    registry
        .list_parked_processes(&query)
        .await
        .expect("list parked processes")
        .into_iter()
        .map(|record| record.id)
        .collect()
}

fn query(n: usize) -> ProcessParkQuery {
    ProcessParkQuery {
        reasons: None,
        parked_at_or_before_ms: None,
        after: None,
        limit: limit(n),
    }
}

/// P1: parked processes list in `(since_ms, process)` order — and only
/// parked ones — filter by reason and age, page by keyset visiting each park
/// once, and agree with the park summary.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn parked_processes_list_by_since_with_filters_and_keyset_pages(
    registry: Arc<dyn ProcessRegistry>,
) {
    let (a, _, park_a) = parked(&registry, cell_divergence()).await;
    let (b, _, park_b) = parked(&registry, divergence("llm_call")).await;
    let (c, _, park_c) = parked(&registry, divergence("tool_call")).await;
    register(&registry).await;

    let since = |record: &ProcessRecord| {
        record
            .park
            .as_deref()
            .map(|park| park.since_ms)
            .expect("the process is parked")
    };
    let mut expected = [
        (since(&park_a), a.clone()),
        (since(&park_b), b.clone()),
        (since(&park_c), c.clone()),
    ];
    expected.sort();
    let expected_ids = expected
        .iter()
        .map(|(_, id)| id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        list(&registry, query(10)).await,
        expected_ids,
        "every parked process, oldest first, and no unparked one"
    );

    let mut effect_only = query(10);
    effect_only.reasons = Some(BTreeSet::from([ParkReasonCode::EffectReplayDivergence]));
    assert_eq!(
        list(&registry, effect_only).await,
        expected_ids
            .iter()
            .filter(|id| **id != a)
            .cloned()
            .collect::<Vec<_>>(),
        "a reason filter keeps only its reason's parks"
    );

    let oldest = expected
        .first()
        .map(|(since, _)| *since)
        .unwrap_or_default();
    let mut too_young = query(10);
    too_young.parked_at_or_before_ms = Some(oldest.saturating_sub(1));
    assert!(
        list(&registry, too_young).await.is_empty(),
        "an age filter older than every park lists nothing"
    );

    let mut visited = Vec::new();
    let mut after = None;
    loop {
        let mut page = query(1);
        page.after = after.clone();
        let records = registry
            .list_parked_processes(&page)
            .await
            .expect("page parked processes");
        let Some(record) = records.first() else { break };
        assert_eq!(records.len(), 1, "a page never exceeds its limit");
        visited.push(record.id.clone());
        after = Some((since(record), record.park_key()));
    }
    assert_eq!(
        visited, expected_ids,
        "a limit-1 keyset walk visits each park once, in order"
    );

    let summary = registry
        .summarize_parked_processes()
        .await
        .expect("summarize parked processes");
    assert_eq!(summary.total(), 3);
    assert_eq!(
        summary
            .by_reason
            .get(&ParkReasonCode::EffectReplayDivergence),
        Some(&2)
    );
    assert_eq!(
        summary.by_reason.get(&ParkReasonCode::ReplayDivergence),
        Some(&1)
    );
    assert_eq!(summary.oldest_since_ms, Some(oldest));
}

/// P2: a rerun's refusal re-parks the same park — `since_ms` and `park_id`
/// kept, `attempts` counted, the reason refreshed — and neither the re-park
/// nor the rerun's start writes a feed event; a park whose latest run
/// already refused is unchanged by another park write.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_process_re_park_keeps_its_park_and_counts_attempts(
    registry: Arc<dyn ProcessRegistry>,
) {
    // S8: a park whose writer knows the checkpoint's build generation records
    // it on the park and stamps the `Parked` feed event it opens.
    let id = register(&registry).await;
    let (authority, _) = start_attempt(&registry, &id, 1).await;
    let checkpoint_generation = lash_core::engine::BuildGeneration::for_test("f3795c");
    let first = registry
        .park_process_with_authority(
            &id,
            crate::store::ProcessParkWrite {
                reason: cell_divergence(),
                engine: None,
                build_generation: Some(checkpoint_generation.clone()),
            },
            &authority,
        )
        .await
        .expect("park the process");
    let opened = first
        .park
        .as_deref()
        .cloned()
        .expect("the first refusal parks");
    assert_eq!(opened.attempts, 1);
    assert!(opened.refusing);
    assert!(!first.is_terminal(), "a park is never terminal");
    assert_eq!(first.outcome, None, "a park writes no terminal evidence");
    assert_eq!(
        opened.build_generation,
        Some(checkpoint_generation.clone()),
        "the park keeps the checkpoint's recorded build generation"
    );
    assert_eq!(
        transitions_of(&registry, &id).await,
        vec![(
            opened.park_id,
            ParkEventKind::Parked {
                reason: cell_divergence()
            }
        )],
        "the first refusal opens the park in the feed"
    );
    let feed = registry
        .process_park_feed(ParkFeedCursor::initial(), limit(256))
        .await
        .expect("read the process park feed");
    assert_eq!(
        feed.events
            .iter()
            .find(|event| event.target == id)
            .map(|event| event.build_generation.clone()),
        Some(Some(checkpoint_generation)),
        "the `Parked` event carries the checkpoint's build generation"
    );

    let repeated = registry
        .park_process_with_authority(&id, divergence("llm_call").into(), &authority)
        .await
        .expect("repeat the park write");
    assert_eq!(
        repeated.park.as_deref(),
        Some(&opened),
        "a refusing park is unchanged by a repeated write"
    );

    let rerun = registry
        .begin_parked_rerun_with_authority(&id, &authority)
        .await
        .expect("begin a rerun");
    let rerunning = rerun.park.as_deref().expect("a rerun keeps the park");
    assert!(!rerunning.refusing, "a rerun stops the park refusing");
    assert_eq!(rerunning.park_id, opened.park_id);
    let again = registry
        .begin_parked_rerun_with_authority(&id, &authority)
        .await
        .expect("begin the rerun again");
    assert_eq!(again.park, rerun.park, "a second rerun start is unchanged");

    let reparked = registry
        .park_process_with_authority(&id, divergence("llm_call").into(), &authority)
        .await
        .expect("the rerun refuses again");
    let park = reparked.park.as_deref().expect("the process stays parked");
    assert_eq!(park.park_id, opened.park_id, "a re-park keeps its park id");
    assert_eq!(park.since_ms, opened.since_ms, "a re-park keeps its since");
    assert_eq!(park.attempts, 2, "a re-park counts the refusal");
    assert!(park.refusing);
    assert!(park.last_refused_ms >= opened.last_refused_ms);
    assert_eq!(park.reason, divergence("llm_call"), "the reason refreshes");
    assert_eq!(
        transitions_of(&registry, &id).await.len(),
        1,
        "neither a rerun's start nor a re-park writes a feed event"
    );
    assert_eq!(list(&registry, query(10)).await, vec![id.clone()]);
}

/// P3: the first fact of the process's own execution past a refusal ends
/// the park with exactly one `Unparked{ProcessProgressed}`; a later refusal
/// opens a new park.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn progress_after_a_rerun_clears_the_park_once(registry: Arc<dyn ProcessRegistry>) {
    let (id, authority, first) = parked(&registry, cell_divergence()).await;
    let opened = first.park.as_deref().cloned().expect("the refusal parks");
    registry
        .begin_parked_rerun_with_authority(&id, &authority)
        .await
        .expect("begin a rerun");
    let progressed = registry
        .set_process_wait_with_authority(
            &id,
            crate::WaitState {
                since_ms: 1,
                kind: crate::WaitKind::Signal {
                    name: "ready".to_string(),
                    event_type: "signal.ready".to_string(),
                    key: format!("{id}:signal.ready:1"),
                    ordinal: 1,
                },
            },
            Vec::new(),
            &authority,
        )
        .await
        .expect("the rerun got past replay and waits");
    assert_eq!(progressed.park, None, "progress ends the park");
    assert_eq!(
        transitions_of(&registry, &id).await,
        vec![
            (
                opened.park_id,
                ParkEventKind::Parked {
                    reason: cell_divergence()
                }
            ),
            (
                opened.park_id,
                ParkEventKind::Unparked {
                    cause: UnparkCause::ProcessProgressed
                }
            ),
        ]
    );
    assert!(list(&registry, query(10)).await.is_empty());
    assert_eq!(
        registry
            .summarize_parked_processes()
            .await
            .expect("summarize")
            .total(),
        0
    );

    let reparked = registry
        .park_process_with_authority(&id, divergence("llm_call").into(), &authority)
        .await
        .expect("a later refusal parks again");
    let park = reparked.park.as_deref().expect("a new park");
    assert_ne!(park.park_id, opened.park_id, "a new park has a new id");
    assert_eq!(park.attempts, 1);
    assert_eq!(transitions_of(&registry, &id).await.len(), 3);
}

/// P4: a parked process's terminal closes its park by how it ended —
/// `Cancelled{ProcessCancelled}` naming the cancel request's origin, or
/// `Unparked{ProcessTerminal}` naming any other terminal status — and a
/// cancel request alone leaves it parked.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_parked_process_that_ends_closes_its_park_by_how_it_ended(
    registry: Arc<dyn ProcessRegistry>,
) {
    let (failed, _authority, record) = parked(&registry, cell_divergence()).await;
    let park_id = record.park.as_deref().expect("parked").park_id;
    let completed = registry
        .complete_process(
            &failed,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::failure(
                crate::ToolFailure::runtime(
                    crate::ToolFailureClass::Execution,
                    "gave_up",
                    "the operator gave up on it",
                ),
            )),
            crate::ProcessCompletionAuthority::workflow_key(failed.to_string()),
        )
        .await
        .expect("fail the parked process");
    let _ = completed;
    let terminal = registry
        .get_process(&failed)
        .await
        .expect("read the failed process")
        .expect("the failed process is retained");
    assert_eq!(terminal.park, None, "a terminal process is not parked");
    assert_eq!(
        transitions_of(&registry, &failed).await.last(),
        Some(&(
            park_id,
            ParkEventKind::Unparked {
                cause: UnparkCause::ProcessTerminal {
                    status: crate::ProcessStatus::Failed
                }
            }
        ))
    );

    let (cancelled, _authority, record) = parked(&registry, cell_divergence()).await;
    let park_id = record.park.as_deref().expect("parked").park_id;
    let requested = registry
        .request_process_cancel(
            &record.id,
            crate::CancelOrigin::OperatorRequested,
            "operator:park".to_string(),
            None,
        )
        .await
        .expect("request the parked process's cancel");
    assert!(
        requested.park.is_some(),
        "a cancel request alone does not end the park"
    );
    registry
        .complete_process(
            &cancelled,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::cancelled(
                crate::ToolCancellation::runtime("the operator cancelled it"),
            )),
            crate::ProcessCompletionAuthority::workflow_key(cancelled.to_string()),
        )
        .await
        .expect("cancel the parked process");
    assert_eq!(
        transitions_of(&registry, &cancelled).await.last(),
        Some(&(
            park_id,
            ParkEventKind::Cancelled {
                cause: ParkCancelCause::ProcessCancelled {
                    origin: Some(crate::CancelOrigin::OperatorRequested)
                }
            }
        ))
    );
    assert!(list(&registry, query(10)).await.is_empty());
}

/// P6: compacting the process park feed raises its horizon; a read from a
/// cursor below it is refused typed, and a read from the horizon returns
/// exactly the retained suffix.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_compacted_process_park_feed_cursor_is_refused_typed(
    registry: Arc<dyn ProcessRegistry>,
) {
    parked(&registry, cell_divergence()).await;
    parked(&registry, cell_divergence()).await;
    let feed = registry
        .process_park_feed(ParkFeedCursor::initial(), limit(16))
        .await
        .expect("read the feed");
    assert_eq!(feed.events.len(), 2);
    let first = ParkFeedCursor::from_store_sequence(feed.events[0].seq);
    registry
        .compact_process_park_feed(first)
        .await
        .expect("compact through the first event");
    match registry
        .process_park_feed(ParkFeedCursor::initial(), limit(16))
        .await
    {
        Err(PluginError::ProcessParkFeedCursorCompacted { horizon }) => {
            assert_eq!(horizon, first, "the refusal names the horizon");
        }
        other => panic!("a compacted cursor must be refused typed, got {other:?}"),
    }
    let suffix = registry
        .process_park_feed(first, limit(16))
        .await
        .expect("read from the horizon");
    assert_eq!(
        suffix
            .events
            .iter()
            .map(|event| event.seq)
            .collect::<Vec<_>>(),
        vec![feed.events[1].seq],
        "the horizon read returns exactly the retained suffix"
    );
}
