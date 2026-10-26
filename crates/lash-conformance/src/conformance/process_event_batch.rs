//! Cross-backend law L10 (FIG-3571): a terminal completion commits its
//! prelude in its own transaction.
//!
//! A run commits its effect summary at boundaries, as the prelude of the
//! boundary's own write (ADR 0100 R4). The terminal completion's prelude
//! folds its events exactly as appending them one at a time would (ADR 0046),
//! rewrites the process record once and advances the change clock once, so a
//! change-feed consumer sees one committed batch for the boundary.

use super::*;
use crate::ProcessEventLogTestSupport as _;
use lash_vocabulary::identity::ProcessId;
use pretty_assertions::assert_eq;

fn batch_registration() -> ProcessRegistration {
    ProcessRegistration::new(
        ProcessInput::Engine {
            kind: "test-engine".to_string(),
            payload: serde_json::Value::Null,
            launch: crate::EngineLaunch::new(
                lash_execution_contracts::testing::process_execution_env_fixture_ref(),
                None,
            ),
        },
        ProcessProvenance::host(),
        lash_execution_contracts::process::Lifetime::Detached,
    )
}

/// Register a process for the law.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn registered(registry: &Arc<dyn ProcessRegistry>) -> ProcessId {
    registry
        .register_process(batch_registration())
        .await
        .expect("register the batch process")
        .id
}

fn occurrence(node: &str, occurrence: u64, replay_key: &str) -> ProcessEventAppendRequest {
    lash_execution_contracts::process::ProcessEffectOccurrence::new(
        lash_kernel_value::sites::EffectLocation::authored(
            lash_protocol_types::effect_identity_fixture(node, occurrence),
        ),
        "tool:batch_law",
        lash_execution_contracts::process::ProcessEffectOutcomeClass::Success,
        None,
        replay_key,
        lash_core::FleetFormat::current(),
    )
    .append_request()
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
    (crate::ProcessStatus, u64, Vec<crate::WaitState>),
) {
    let events = registry
        .full_event_window(id, 0)
        .await
        .expect("read the process log")
        .into_iter()
        .map(|event| {
            (
                event.fact.event_type().to_owned(),
                event.sequence,
                event.fact.payload(),
            )
        })
        .collect();
    let record = registry
        .get_process(id)
        .await
        .expect("read the process")
        .expect("the process is retained");
    (
        events,
        (
            record.status(),
            record.last_event_sequence,
            record
                .waits()
                .iter()
                .map(|wait| {
                    // The since instant is the writer's clock; the law
                    // compares what the fold derived.
                    crate::WaitState {
                        since_ms: 0,
                        ..wait.clone()
                    }
                })
                .collect(),
        ),
    )
}

fn event_types(events: &[(String, u64, serde_json::Value)]) -> Vec<&str> {
    events
        .iter()
        .map(|(event_type, _, _)| event_type.as_str())
        .collect()
}

/// L10: the terminal completion commits the run's pending prelude ahead of
/// its own event in its one transaction, with one change-clock tick.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_boundary_commits_its_prelude_in_its_own_transaction(
    registry: Arc<dyn ProcessRegistry>,
) {
    let id = registered(&registry).await;

    // The terminal batch: the last occurrence, the omission record, the
    // terminal, one commit.
    let clock = change_clock(&registry).await;
    let omissions = lash_execution_contracts::process::ProcessEffectOmissions::new(
        [(
            "node:a".to_string(),
            lash_execution_contracts::process::ProcessEffectOmittedCounts {
                success: 0,
                failure: 1,
                cancelled: 0,
            },
        )]
        .into(),
        8,
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
            lash_execution_contracts::process::ProcessCompletionAuthority::workflow_key(
                id.to_string(),
            ),
        )
        .await
        .expect("complete with the terminal batch");
    assert!(
        matches!(
            completion,
            lash_execution_contracts::process::ProcessCompletionOutcome::Committed(ref record) if record.is_terminal()
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
            lash_execution_contracts::process::PROCESS_EFFECT_OUTCOME_EVENT_TYPE,
            lash_execution_contracts::process::PROCESS_EFFECT_OMISSIONS_EVENT_TYPE,
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
            lash_execution_contracts::process::ProcessCompletionAuthority::workflow_key(
                id.to_string(),
            ),
        )
        .await
        .expect("replay the completion");
    assert!(
        matches!(
            replayed,
            lash_execution_contracts::process::ProcessCompletionOutcome::AlreadyApplied { .. }
        ),
        "{replayed:?}"
    );
    assert_eq!(change_clock(&registry).await, clock);
    assert_eq!(folded(&registry, &id).await.0, events);
}
