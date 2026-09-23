//! Process-wake identities and observations the runtime boundaries record.

use lash_core::RuntimeInvocation;
use lash_core::runtime::{RuntimeAttribution, RuntimeReplay, RuntimeSubject};
use lash_sansio::{ProcessId, SessionId};
use serde_json::{Value, json};

use super::RuntimeBoundaryError;

/// The observation of a wake redelivery the receiver refused at its
/// allocation floor: the first delivery's settlement raised the floor, so the
/// store refuses the redelivery durably and nothing is enqueued or claimed
/// (FIG-3545).
pub(super) fn refused_redelivery_observation(
    session: &str,
    process_id: &str,
    source_key: &str,
    wake: lash_core::ProcessWakeDelivery,
    allocation_floor: u64,
) -> Value {
    json!({
        "session": session,
        "process_wake": true,
        "process_id": process_id,
        "sequence": wake.sequence,
        "wake_id": wake.wake_id,
        "claimed_once": false,
        "runtime_process_wake": wake,
        "runtime_queued_work": {
            "source_key": source_key,
            "work_class": "TurnWork",
            "enqueued": false,
            "claimed": false,
            "claimed_batch_count": 0,
            "claim_fencing_token": Value::Null,
            "batch_id_present": false,
            "claim_id_present": false,
            "runtime_turn_id": Value::Null,
            "receiver_floor_refused": true,
            "receiver_allocation_floor": allocation_floor,
        },
    })
}

/// The unit of worker-owned work one worker-contention boundary claims.
///
/// Each boundary owns a distinct wake identity: settling the work raises the
/// session's receiver floor for that wake (FIG-3545), so a second
/// worker-contention boundary on the same session cannot reuse it. The
/// scheduler boundary id is stable across backend replays and unique per
/// occurrence, as it is for the process-completion contention.
pub(super) fn worker_failover_work(
    session: &str,
    boundary_id: &str,
    occurred_at_ms: u64,
) -> Result<lash_core::ProcessWakeDelivery, RuntimeBoundaryError> {
    let process_id = ProcessId::from(format!("sim-worker-{session}-{boundary_id}"));
    lash_core::facade_support::process_wake_delivery(
        lash_core::facade_support::ProcessWakeDeliveryRequest {
            target_session_id: SessionId::from(session.to_string()),
            process_id: process_id.clone(),
            process_incarnation: lash_core::ProcessIncarnation::from_registration_sequence(1),
            sequence: 1,
            event_type: "process.wake".to_string(),
            event_invocation: RuntimeInvocation {
                attribution: RuntimeAttribution::for_session(session.to_string()),
                subject: RuntimeSubject::ProcessEvent {
                    process_id,
                    sequence: 1,
                    event_type: "process.wake".to_string(),
                },
                caused_by: None,
                replay: Some(RuntimeReplay {
                    attribution: None,
                    key: format!("worker-failover:{session}:{boundary_id}:work"),
                }),
            },
            process_caused_by: None,
            authority: lash_core::QueuedWorkAuthority::default(),
            wake: lash_core::facade_support::ProcessWake {
                input: format!("worker-owned work for {session}"),
            },
            occurred_at_ms,
        },
    )
    .map_err(|err| RuntimeBoundaryError::new(format!("build worker-owned work failed: {err}")))
}
