//! Shared fixtures for the conformance suites: paired handles opened
//! against the same durable backing store, used by the `*_reopenable`
//! suite variants.

use super::*;
use lash_core::PROCESS_WAKE_DELIVERY_FORMAT_VERSION;

pub(crate) fn assert_fresh_instances<T: ?Sized>(left: &Arc<T>, right: &Arc<T>, suite: &str) {
    assert!(
        !Arc::ptr_eq(left, right),
        "{suite} factory reused one Arc across conformance roles"
    );
}

/// Admit a scope for a host entry point: conformance suites mint scopes
/// directly, so this stands in for the admission authority's answer — a
/// process scope pins the fabricated first-registration incarnation the
/// fixture fabricates for it.
pub(crate) fn admit(scope: crate::ExecutionScope) -> crate::AdmittedScope {
    match &scope {
        crate::ExecutionScope::Process { process_id } => {
            crate::AdmittedScope::process(process_id.clone())
        }
        _ => crate::AdmittedScope::new(scope),
    }
}

/// Record one completed attachment write: acquire the write fence, then stamp
/// the upload evidence. This is the only way a manifest row comes into being,
/// and the stamp is the only thing that makes a digest adoptable.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(crate) async fn record_completed_attachment_write(
    store: &Arc<dyn crate::RuntimePersistence>,
    intent: crate::AttachmentIntent,
) {
    let crate::AttachmentWriteFence::Granted(permit) = store
        .begin_attachment_write(intent.clone())
        .await
        .expect("begin attachment write")
    else {
        panic!(
            "expected a granted write fence for `{}`",
            intent.attachment_id
        );
    };
    store
        .complete_attachment_write(&intent, permit)
        .await
        .expect("stamp attachment upload evidence");
}

/// Model the artifact-cleanup worker before asserting that a projected process
/// tombstone is eligible for physical compaction.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(crate) async fn acknowledge_pending_process_artifact_cleanup(
    registry: &dyn crate::ProcessRegistry,
) {
    for cleanup in registry
        .pending_process_artifact_cleanup()
        .await
        .expect("list pending process artifact cleanup")
    {
        let acknowledgement = registry
            .complete_process_artifact_cleanup(&cleanup.process_id)
            .await
            .expect("acknowledge process artifact cleanup");
        assert_eq!(
            acknowledgement,
            crate::ProcessArtifactCleanupAck::Acknowledged {
                process_id: cleanup.process_id,
            },
            "cleanup acknowledges its exact process"
        );
    }
}

/// A pair of [`ProcessRegistry`] handles opened against the same durable
/// backing store.
pub struct ReopenableProcessRegistry {
    pub open: Arc<dyn crate::ConformanceProcessRegistry>,
    pub reopen: Arc<dyn crate::ConformanceProcessRegistry>,
}

/// A pair of [`RuntimePersistence`] handles opened against the same durable
/// backing store, and the effect host of the same substrate: the owner of the
/// turn-control promises a closure authorization names.
pub struct ReopenableRuntimePersistence {
    pub open: Arc<dyn RuntimePersistence>,
    pub reopen: Arc<dyn RuntimePersistence>,
    pub effect_host: Arc<dyn crate::EffectHost>,
}

/// A pair of [`AttachmentStore`](crate::AttachmentStore) handles opened against
/// the same durable backing store.
pub struct ReopenableAttachmentStore {
    pub open: Arc<dyn crate::AttachmentStore>,
    pub reopen: Arc<dyn crate::AttachmentStore>,
}

/// A pair of [`TriggerStore`](crate::TriggerStore) handles opened against
/// the same durable backing store.
pub struct ReopenableTriggerStore {
    pub open: Arc<dyn crate::TriggerStore>,
    pub reopen: Arc<dyn crate::TriggerStore>,
}

/// Push an unpersisted event node onto `state`'s active path and make it the
/// resident leaf. Pair with [`commit_conformance_state`] to advance a session's
/// durable head from outside any runtime.
pub(crate) use lash_core::testing::store_fixtures::{
    append_conformance_event_node, bind_conformance_session, commit_conformance_state,
    durable_turn_address, durable_turn_scope,
};

/// Queued turn work carrying `text`: one process wake of `process` at
/// `sequence`. A process wake is the one turn-work payload; a frame handoff is
/// the head's pending follow-on, never a queue row (ADR 0101 §3). The source
/// key is the wake's own, so the same `(process, sequence)` names the same row.
pub(crate) fn process_wake_work(
    session_id: &crate::SessionId,
    process: &str,
    sequence: u64,
    text: &str,
    delivery_policy: crate::DeliveryPolicy,
) -> crate::QueuedWorkBatchDraft {
    let process_id = crate::ProcessId::fixture(process);
    let wake = crate::ProcessWakeDelivery {
        version: crate::FleetFormat::current().writer_version(lash_core::surface_format!(
            PROCESS_WAKE_DELIVERY_FORMAT_VERSION
        )),
        wake_id: format!("wake:{session_id}:{process}:{sequence}"),
        target_session_id: session_id.clone(),
        process_id: process_id.clone(),
        sequence,
        event_type: "process.wake".to_string(),
        event_invocation: crate::RuntimeInvocation {
            attribution: crate::RuntimeAttribution::for_session(session_id),
            subject: crate::RuntimeSubject::ProcessEvent {
                process_id: process_id.clone(),
                sequence,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        authority: crate::QueuedWorkAuthority::default(),
        input: text.to_string(),
        created_at_ms: 1,
    };
    crate::QueuedWorkBatchDraft::new(
        session_id,
        delivery_policy,
        crate::TurnWorkPayload::process_wake(wake),
    )
    .with_source_key(crate::process_wake_source_key(&process_id, sequence))
    .with_process_wake_source(process_id, sequence)
}
