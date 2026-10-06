use crate::support::prelude::*;
use std::sync::Arc;

use crate::runtime::process::{
    ProcessAwaitOutput, ProcessCompletionAuthority, ProcessEventAppendRequest,
    ProcessEventSemanticsSpec, ProcessEventType, ProcessExecutionEnvRef, ProcessExecutionEnvSpec,
    ProcessInput, ProcessProvenance, ProcessRegistration, ProcessValueSelector, ProcessWakeSpec,
    ProjectionWatermark,
};
use crate::{Lifetime, ProcessRegistry, SessionId, StoreSet as _};

use crate::support::sqlite_memory_store_set;

async fn memory_registry() -> Arc<dyn ProcessRegistry> {
    sqlite_memory_store_set().await.process_registry()
}

fn registration(_id: &str) -> ProcessRegistration {
    crate::testing::held_engine_registration(
        serde_json::Value::Null,
        ProcessProvenance::host(),
        crate::Lifetime::Detached,
    )
}

/// FIG-3123. The three halves of "an announcement is not a wake", written
/// together because each is only meaningful against the other two: the same
/// process, the same event type, the same declared wake and the same target
/// session — and only the delivery differs.
#[tokio::test]
async fn a_suppressed_append_is_journaled_in_full_and_wakes_nobody() {
    let registry = memory_registry().await;
    let process_id = crate::ProcessId::fixture("announcement-suppression");
    let target_session_id = SessionId::from("announcing-session");
    let announcement_suppression_record = registry
        .register_process(wake_registration(process_id.as_str(), &target_session_id))
        .await
        .expect("register a process whose wakes have a target session");
    let process_id = announcement_suppression_record.id.clone();

    // (a) The runtime's own announcement of a park. The session it would reach
    // is the session parked on the announced call, so it gets no work.
    let announced = registry
        .append_event(
            &process_id,
            ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "input request opened"}),
            )
            .with_replay_key("announcement:park")
            .without_wake(),
        )
        .await
        .expect("append the park announcement");
    assert!(
        announced.wake_delivery.is_none(),
        "a park announcement must not deliver a wake: {:?}",
        announced.wake_delivery
    );
    assert!(
        registry
            .claim_pending_wake_deliveries(8)
            .await
            .expect("claim wake deliveries after the announcement")
            .is_empty(),
        "the announcing session must observe no queued work from its own park"
    );

    // (c) ...and yet nothing reading the journal can tell: the event is the
    // same event, semantics included.
    let journal = registry
        .full_event_window(&process_id, 0)
        .await
        .expect("read the process journal");
    let appended = journal
        .iter()
        .find(|event| event.sequence == announced.event.sequence)
        .expect("the announcement is in the journal");
    assert_eq!(appended.event_type, "producer.wake");
    assert_eq!(
        appended.payload,
        serde_json::json!({"wake_input": "input request opened"})
    );
    assert!(
        appended.semantics.wake.is_some(),
        "the event keeps the wake its type declares; only the delivery is withheld: {appended:?}"
    );

    // (b) A progress emission from that same process still says its piece.
    let emitted = registry
        .append_event(
            &process_id,
            ProcessEventAppendRequest::new(
                "producer.wake",
                serde_json::json!({"wake_input": "deploy complete"}),
            )
            .with_replay_key("announcement:emit"),
        )
        .await
        .expect("append the progress emission");
    assert!(
        emitted.wake_delivery.is_some(),
        "an unsuppressed append of the same event type still delivers its wake"
    );
    let claimed = registry
        .claim_pending_wake_deliveries(8)
        .await
        .expect("claim wake deliveries after the emission");
    assert_eq!(
        claimed.len(),
        1,
        "exactly the emission reaches the declaring session: {claimed:?}"
    );
}

fn wake_registration(id: &str, target_session_id: &SessionId) -> ProcessRegistration {
    registration(id)
        .with_wake_session_id(Some(target_session_id.clone()))
        .with_extra_event_types([ProcessEventType {
            name: "producer.wake".to_string(),
            payload_schema: crate::JsonSchema::any(),
            semantics: ProcessEventSemanticsSpec {
                wake: Some(ProcessWakeSpec {
                    when: Some(ProcessValueSelector::Present("/wake_input".to_string())),
                    input: ProcessValueSelector::Pointer("/wake_input".to_string()),
                }),
                ..ProcessEventSemanticsSpec::default()
            },
        }])
}

#[tokio::test]
async fn prune_retains_exact_artifact_cleanup_until_acknowledged() {
    let backend = sqlite_memory_store_set().await;
    let registry = backend.process_registry();
    let cleanup_ledger = backend.artifact_cleanup();
    let registration = ProcessRegistration::new(
        ProcessInput::Engine {
            kind: "test-engine".to_string(),
            payload: serde_json::json!({"module_ref": "module-1"}),
        },
        ProcessProvenance::host(),
        Lifetime::Detached,
    )
    .with_execution_env_ref(Some(ProcessExecutionEnvRef::new("process-env:cleanup")));
    let registered = registry
        .register_process(registration)
        .await
        .expect("register process with exact artifact inputs");
    registry
        .complete_process(
            &registered.id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            ProcessCompletionAuthority::workflow_key("artifact-cleanup-process"),
        )
        .await
        .expect("complete process before prune");

    registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .expect("prune process row and persist cleanup evidence atomically");
    let pending = cleanup_ledger
        .claim_due(
            i64::MAX as u64 / 2,
            1000,
            std::num::NonZeroUsize::new(1).expect("nonzero"),
        )
        .await
        .expect("claim pending artifact cleanup");
    assert_eq!(pending.len(), 1);
    let cleanup = cleanup_ledger
        .load_cleanup(&pending[0].id)
        .await
        .expect("read cleanup")
        .expect("cleanup exists");
    assert_eq!(
        cleanup,
        crate::ArtifactCleanup::ended(
            crate::ArtifactReferrer::ProcessRecord(registered.id.clone()),
            Vec::new(),
            None,
        ),
        "row deletion must leave the process referrer cleanup"
    );
    assert_eq!(
        registry
            .compact_process_tombstones(u64::MAX, ProjectionWatermark::NoProjector, None)
            .await
            .expect("compact while cleanup is pending"),
        1,
        "the self-contained cleanup obligation survives tombstone compaction"
    );

    let acknowledgement = cleanup_ledger
        .settle(
            &pending[0].id,
            &pending[0].token,
            crate::store::ObligationSettlement::Delivered,
            i64::MAX as u64 / 2,
        )
        .await
        .expect("acknowledge artifact cleanup");
    assert_eq!(acknowledgement, crate::store::SettleOutcome::Applied);
    assert!(
        cleanup_ledger
            .load_cleanup(&pending[0].id)
            .await
            .expect("read cleanup after acknowledgement")
            .is_none()
    );
    assert_eq!(
        registry
            .compact_process_tombstones(u64::MAX, ProjectionWatermark::NoProjector, None)
            .await
            .expect("compact after cleanup acknowledgement"),
        0
    );
}

/// The store's refusals carry the typed reasons, so every caller downstream of
/// `ProcessExecutionEnvStore` classifies by code.
#[tokio::test]
async fn env_store_reports_typed_referrer_fences_and_carry_refusals() {
    let backend = sqlite_memory_store_set().await;
    let store = backend.process_env_store();
    let spec = ProcessExecutionEnvSpec::new(
        crate::AdmittedPluginConfig::default(),
        crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024)),
    );
    let env_ref = spec.stable_ref().expect("stable env ref");
    let bytes = spec.to_store_bytes().expect("encode env spec");
    let staged = crate::ArtifactReferrer::HostPin(crate::HostArtifactPin::mint());
    let staged_claim = crate::ReferrerClaim::unguarded(staged.clone()).expect("staged claim");
    let retired_destination = crate::ArtifactReferrer::HostPin(crate::HostArtifactPin::mint());

    store
        .end_process_env_referrer(&crate::ResolvedArtifactCleanup {
            referrer: retired_destination.clone(),
            carries: Vec::new(),
        })
        .await
        .expect("retire destination owner");
    store
        .publish_process_execution_env(&staged_claim, &env_ref, &bytes)
        .await
        .expect("stage env");

    store
        .end_process_env_referrer(&crate::ResolvedArtifactCleanup {
            referrer: staged.clone(),
            carries: vec![crate::ArtifactCarry {
                artifact: crate::ArtifactName {
                    store: crate::ArtifactStoreId::ProcessEnv,
                    artifact_ref: env_ref.as_str().to_owned(),
                },
                to: retired_destination.clone(),
            }],
        })
        .await
        .expect("a carry into an ended destination does not revive its edge");
    assert_eq!(
        store
            .get_process_execution_env(&env_ref)
            .await
            .expect("read env"),
        None,
    );
    let destination_claim =
        crate::ReferrerClaim::unguarded(retired_destination.clone()).expect("destination claim");
    let destination_error = store
        .publish_process_execution_env(&destination_claim, &env_ref, &bytes)
        .await
        .expect_err("the destination remains fenced");
    assert!(
        matches!(destination_error, crate::ArtifactStoreError::ReferrerEnded { ref referrer } if *referrer == retired_destination),
        "destination fence classifies by referrer: {destination_error}"
    );

    let absent = crate::ArtifactReferrer::HostPin(crate::HostArtifactPin::mint());
    let missing_edge = store
        .end_process_env_referrer(&crate::ResolvedArtifactCleanup {
            referrer: absent,
            carries: vec![crate::ArtifactCarry {
                artifact: crate::ArtifactName {
                    store: crate::ArtifactStoreId::ProcessEnv,
                    artifact_ref: "process-env:missing".to_owned(),
                },
                to: crate::ArtifactReferrer::HostPin(crate::HostArtifactPin::mint()),
            }],
        })
        .await
        .expect_err("a transfer with neither edge refuses");
    assert!(
        matches!(
            missing_edge,
            crate::ArtifactStoreError::CarryArtifactMissing { .. }
        ),
        "missing carry bytes classify by code: {missing_edge}"
    );

    store
        .end_process_env_referrer(&crate::ResolvedArtifactCleanup {
            referrer: staged.clone(),
            carries: Vec::new(),
        })
        .await
        .expect("retire staged owner");
    let retired_error = store
        .publish_process_execution_env(&staged_claim, &env_ref, &bytes)
        .await
        .expect_err("a retired staging owner refuses publication");
    assert!(
        matches!(retired_error, crate::ArtifactStoreError::ReferrerEnded { ref referrer } if *referrer == staged),
        "staged referrer fence classifies by code: {retired_error}"
    );
}
