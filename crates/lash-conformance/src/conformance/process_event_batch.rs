//! Cross-backend law L10 (FIG-3571): a process-event batch is one commit.
//!
//! A run commits its effect summary at boundaries, as the prelude of the
//! boundary's own write (ADR 0100 R4). Every such write — a bare batch
//! ([`ProcessEventLog::append_events`](crate::ProcessEventLog::append_events)),
//! a wait's enter or clear, a terminal completion — is one transaction that
//! folds its events exactly as appending them one at a time would (ADR 0046),
//! rewrites the process record once and advances the change clock once, so a
//! change-feed consumer sees one committed batch per boundary. A refusal of
//! any event commits none of them.

use super::*;
use crate::ProcessEventLogTestSupport as _;
use crate::{ProcessExecutionWriteAuthority, ProcessLease};
use lash_sansio::ProcessId;
use pretty_assertions::assert_eq;

fn batch_registration() -> ProcessRegistration {
    ProcessRegistration::new(
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        RecoveryContract::Rerunnable,
        ProcessProvenance::host(),
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon,
        ),
    )
}

/// Register a process and claim and start its first attempt, returning its
/// id and the lease that attempt writes under.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn started(registry: &Arc<dyn ProcessRegistry>, label: &str) -> (ProcessId, ProcessLease) {
    let id = registry
        .register_process(batch_registration())
        .await
        .expect("register the batch process")
        .id;
    let owner = crate::LeaseOwnerIdentity::opaque(label, format!("{label}:i"));
    let lease = registry
        .claim_process_lease(&id, &owner, 60_000)
        .await
        .expect("claim the attempt's lease")
        .acquired()
        .expect("the attempt's lease is free");
    registry
        .record_first_started_with_authority(
            &id,
            crate::ProcessStarted {
                owner,
                fencing_token: lease.fencing_token,
                attempt: 1,
                started_at_ms: lease.claimed_at_epoch_ms,
                generation: None,
            },
            &ProcessExecutionWriteAuthority::lease(lease.clone()),
        )
        .await
        .expect("record the attempt's start");
    (id, lease)
}

fn occurrence(node: &str, occurrence: u64, replay_key: &str) -> ProcessEventAppendRequest {
    lash_core::ProcessEffectSummaryOccurrence::new(
        node,
        occurrence,
        "tool:batch_law",
        lash_core::ProcessEffectOutcomeClass::Success,
        None,
        replay_key,
        lash_core::FleetFormat::current(),
    )
    .append_request()
}

/// The run's summary so far: two occurrences of one node, one of another,
/// then the omission record a terminal batch closes with.
fn summary() -> Vec<ProcessEventAppendRequest> {
    vec![
        occurrence("node:a", 1, "batch-law:a:1"),
        occurrence("node:a", 2, "batch-law:a:2"),
        occurrence("node:b", 1, "batch-law:b:1"),
        lash_core::ProcessEffectOmissions::new(
            [(
                "node:a".to_string(),
                lash_core::ProcessEffectOmittedCounts {
                    success: 3,
                    failure: 0,
                    cancelled: 0,
                },
            )]
            .into(),
            lash_core::FleetFormat::current(),
        )
        .append_request("batch-law:omissions"),
    ]
}

fn signal_wait(id: &ProcessId) -> crate::WaitState {
    crate::WaitState {
        since_ms: 1,
        kind: crate::WaitKind::Signal {
            name: "ready".to_string(),
            event_type: "signal.ready".to_string(),
            key: format!("{id}:signal.ready:1"),
            ordinal: 1,
        },
    }
}

/// The change clock's current position: the change feed drained to its end.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn change_clock(registry: &Arc<dyn ProcessRegistry>) -> u64 {
    let mut cursor = crate::ProcessChangeCursor::initial();
    loop {
        let (changes, next) = registry
            .processes_changed_since(cursor, 256)
            .await
            .expect("read the change feed");
        cursor = next;
        if changes.is_empty() {
            return cursor.store_sequence();
        }
    }
}

/// The log of `id`, as (type, sequence, payload), and its folded record's
/// lifecycle projection.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn folded(
    registry: &Arc<dyn ProcessRegistry>,
    id: &ProcessId,
) -> (
    Vec<(String, u64, serde_json::Value)>,
    (crate::ProcessStatus, u64, Option<crate::WaitState>),
) {
    let events = registry
        .full_event_window(id, 0)
        .await
        .expect("read the process log")
        .into_iter()
        .map(|event| (event.event_type, event.sequence, event.payload))
        .collect();
    let record = registry
        .get_process(id)
        .await
        .expect("read the process")
        .expect("the process is retained");
    (
        events,
        (
            record.status,
            record.last_event_sequence,
            record.wait.map(|wait| {
                // The since instant is the writer's clock; the law compares
                // what the fold derived.
                crate::WaitState {
                    since_ms: 0,
                    ..wait
                }
            }),
        ),
    )
}

fn event_types(events: &[(String, u64, serde_json::Value)]) -> Vec<&str> {
    events
        .iter()
        .map(|(event_type, _, _)| event_type.as_str())
        .collect()
}

/// L10: a batch folds exactly as its events appended singly do, in one
/// commit — one record rewrite, one change-clock tick — and a refusal of any
/// event commits none of them.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_process_event_batch_is_one_commit(registry: Arc<dyn ProcessRegistry>) {
    let (single, single_lease) = started(&registry, "batch-law-single").await;
    let single_authority = ProcessExecutionWriteAuthority::lease(single_lease);
    let (batched, batched_lease) = started(&registry, "batch-law-batched").await;
    let batched_authority = ProcessExecutionWriteAuthority::lease(batched_lease);

    let before = change_clock(&registry).await;
    for request in summary() {
        registry
            .append_event_with_authority(&single, request, &single_authority)
            .await
            .expect("append one event");
    }
    let after_singles = change_clock(&registry).await;
    assert_eq!(
        after_singles - before,
        summary().len() as u64,
        "each single append is its own commit"
    );

    let receipts = registry
        .append_events(&batched, summary(), &batched_authority)
        .await
        .expect("append the batch");
    let after_batch = change_clock(&registry).await;
    assert_eq!(
        after_batch - after_singles,
        1,
        "the batch rewrites the record once and ticks the change clock once"
    );
    let (single_events, single_record) = folded(&registry, &single).await;
    let (batched_events, batched_record) = folded(&registry, &batched).await;
    // Each process's start names its own lease owner; what follows it is the
    // summary under test.
    assert_eq!(
        batched_events[1..],
        single_events[1..],
        "the batch appends the events the singles do, in order, at the same sequences"
    );
    assert_eq!(
        batched_record, single_record,
        "the batch folds to the record the singles fold to"
    );
    assert_eq!(
        receipts
            .iter()
            .map(|receipt| (receipt.event.sequence, receipt.realization))
            .collect::<Vec<_>>(),
        batched_events[1..]
            .iter()
            .map(|(_, sequence, _)| (*sequence, lash_core::StoreRealization::Realized))
            .collect::<Vec<_>>(),
        "the receipts answer the requests in order"
    );

    // A re-committed batch is a replay-key no-op: nothing is written.
    let replayed = registry
        .append_events(&batched, summary(), &batched_authority)
        .await
        .expect("re-commit the batch");
    assert!(
        replayed
            .iter()
            .all(|receipt| receipt.realization == lash_core::StoreRealization::Coalesced),
        "every re-committed event coalesces onto its first write"
    );
    assert_eq!(change_clock(&registry).await, after_batch);
    assert_eq!(folded(&registry, &batched).await.0, batched_events);

    // An empty batch writes nothing.
    assert!(
        registry
            .append_events(&batched, Vec::new(), &batched_authority)
            .await
            .expect("an empty batch")
            .is_empty()
    );
    assert_eq!(change_clock(&registry).await, after_batch);

    // A refusal mid-batch commits nothing: a fresh occurrence ahead of a
    // payload that conflicts with the one already under its key.
    let mut conflicting = occurrence("node:b", 1, "batch-law:b:1");
    conflicting.payload["outcome_class"] = serde_json::json!("cancelled");
    let refused = registry
        .append_events(
            &batched,
            vec![occurrence("node:c", 1, "batch-law:c:1"), conflicting],
            &batched_authority,
        )
        .await
        .expect_err("a conflicting payload under a recorded key is refused");
    assert!(
        refused
            .to_string()
            .contains("conflicts with an existing event"),
        "the refusal is the replay-conflict rule: {refused}"
    );
    assert_eq!(
        folded(&registry, &batched).await,
        (batched_events, batched_record),
        "the refused batch appended nothing, not even the event ahead of the conflict"
    );
    assert_eq!(change_clock(&registry).await, after_batch);
}

/// L10: a run boundary's write — a wait's enter, its clear, the terminal
/// completion — commits the run's pending prelude ahead of its own event in
/// its one transaction, with one change-clock tick.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_boundary_commits_its_prelude_in_its_own_transaction(
    registry: Arc<dyn ProcessRegistry>,
) {
    let (id, lease) = started(&registry, "batch-law-boundary").await;
    let authority = ProcessExecutionWriteAuthority::lease(lease);

    let clock = change_clock(&registry).await;
    let waiting = registry
        .set_process_wait_with_authority(
            &id,
            signal_wait(&id),
            vec![
                occurrence("node:a", 1, "boundary-law:a:1"),
                occurrence("node:a", 2, "boundary-law:a:2"),
            ],
            &authority,
        )
        .await
        .expect("enter the wait with its prelude");
    assert!(waiting.wait.is_some(), "the process waits");
    assert_eq!(
        change_clock(&registry).await - clock,
        1,
        "the enter is one commit"
    );
    let (events, _) = folded(&registry, &id).await;
    assert_eq!(
        event_types(&events)[events.len() - 3..],
        [
            lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE,
            lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE,
            "process.waiting",
        ],
        "the prelude precedes the wait it rode"
    );

    // A replayed enter re-commits its prelude as a no-op and leaves the wait.
    let clock = change_clock(&registry).await;
    registry
        .set_process_wait_with_authority(
            &id,
            signal_wait(&id),
            vec![occurrence("node:a", 1, "boundary-law:a:1")],
            &authority,
        )
        .await
        .expect("replay the enter");
    assert_eq!(
        change_clock(&registry).await,
        clock,
        "a replayed enter writes nothing"
    );

    let clock = change_clock(&registry).await;
    let resumed = registry
        .clear_process_wait_with_authority(
            &id,
            vec![occurrence("node:b", 1, "boundary-law:b:1")],
            &authority,
        )
        .await
        .expect("clear the wait with its prelude");
    assert!(resumed.wait.is_none(), "the process resumed");
    assert_eq!(
        change_clock(&registry).await - clock,
        1,
        "the clear is one commit"
    );
    let (events, _) = folded(&registry, &id).await;
    assert_eq!(
        event_types(&events)[events.len() - 2..],
        [
            lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE,
            "process.resumed"
        ],
        "the prelude precedes the resume it rode"
    );

    // A refused prelude refuses its boundary: neither is written.
    let mut conflicting = occurrence("node:b", 1, "boundary-law:b:1");
    conflicting.payload["outcome_class"] = serde_json::json!("failure");
    let before = folded(&registry, &id).await;
    let clock = change_clock(&registry).await;
    registry
        .set_process_wait_with_authority(&id, signal_wait(&id), vec![conflicting], &authority)
        .await
        .expect_err("the conflicting prelude refuses the enter");
    assert_eq!(
        folded(&registry, &id).await,
        before,
        "neither the prelude nor the wait"
    );
    assert_eq!(change_clock(&registry).await, clock);

    // The terminal batch: the last occurrence, the omission record, the
    // terminal, one commit.
    let clock = change_clock(&registry).await;
    let omissions = lash_core::ProcessEffectOmissions::new(
        [(
            "node:a".to_string(),
            lash_core::ProcessEffectOmittedCounts {
                success: 0,
                failure: 1,
                cancelled: 0,
            },
        )]
        .into(),
        lash_core::FleetFormat::current(),
    )
    .append_request("boundary-law:omissions");
    let completion = registry
        .complete_process_with_prelude(
            &id,
            ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::json!({ "done": true }),
            )),
            vec![occurrence("node:c", 1, "boundary-law:c:1"), omissions],
            lash_core::ProcessCompletionAuthority::workflow_key(id.to_string()),
        )
        .await
        .expect("complete with the terminal batch");
    assert!(
        matches!(
            completion,
            lash_core::ProcessCompletionOutcome::Committed(ref record) if record.is_terminal()
        ),
        "the terminal committed: {completion:?}"
    );
    assert_eq!(
        change_clock(&registry).await - clock,
        1,
        "the terminal batch is one commit"
    );
    let (events, _) = folded(&registry, &id).await;
    assert_eq!(
        event_types(&events)[events.len() - 3..],
        [
            lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE,
            lash_core::PROCESS_EFFECT_OMISSIONS_EVENT_TYPE,
            "process.completed",
        ],
        "the omission record is the terminal batch's penultimate event"
    );

    // A replayed completion answers the stored terminal and writes nothing.
    let clock = change_clock(&registry).await;
    let replayed = registry
        .complete_process_with_prelude(
            &id,
            ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::json!({ "done": true }),
            )),
            vec![occurrence("node:c", 1, "boundary-law:c:1")],
            lash_core::ProcessCompletionAuthority::workflow_key(id.to_string()),
        )
        .await
        .expect("replay the completion");
    assert!(
        matches!(
            replayed,
            lash_core::ProcessCompletionOutcome::AlreadyApplied { .. }
        ),
        "{replayed:?}"
    );
    assert_eq!(change_clock(&registry).await, clock);
    assert_eq!(folded(&registry, &id).await.0, events);
}
