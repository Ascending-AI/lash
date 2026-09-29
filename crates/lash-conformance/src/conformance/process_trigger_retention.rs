//! Cross-backend conformance for process retention's trigger-store effects.

use crate::conformance::DeploymentViewExt as _;
use lash_sansio::SessionId;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use crate::{
    ProcessAwaitOutput, ProcessCompletionAuthority, ProcessId, ProcessIdentity, ProcessInput,
    ProcessOriginator, ProcessProvenance, ProcessRegistration, ProcessRegistry,
    ProjectionWatermark, SessionScope, TriggerCommand, TriggerCommandOutcome, TriggerOwnerScope,
    TriggerStore, TriggerSubscriptionDraft,
};

/// Fresh paired process and trigger stores for retention conformance.
pub struct ProcessTriggerRetentionHandles {
    pub registry: Arc<dyn ProcessRegistry>,
    pub triggers: Arc<dyn TriggerStore>,
    pub sessions: Arc<dyn crate::DeploymentStore>,
    /// The trigger store's `TriggerDelivery` obligation ledger (ADR 0109).
    pub deliveries: Arc<dyn crate::ObligationLedger>,
    /// The process registry's `ProcessStart` obligation ledger (ADR 0109):
    /// what delivers a registered start to the engine that runs it.
    pub process_starts: Arc<dyn crate::ObligationLedger>,
    /// The environments a delivery's process names (ADR 0113 §3.3).
    pub process_env: Arc<dyn crate::ProcessExecutionEnvStore>,
}

pub async fn process_trigger_retention<F, Fut>(make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ProcessTriggerRetentionHandles>,
{
    let first = make().await;
    let second = make().await;
    assert!(
        !Arc::ptr_eq(&first.registry, &second.registry),
        "process_trigger_retention reused one process-registry Arc"
    );
    assert!(
        !Arc::ptr_eq(&first.triggers, &second.triggers),
        "process_trigger_retention reused one trigger-store Arc"
    );
    drop((first, second));
    deleted_session_frontier_authorizes_trigger_owner_reclamation(make().await).await;
    process_prune_preserves_trigger_mutation_receipts(make().await).await;
    zero_match_occurrence_is_reclaimed_at_delivery_reconciliation(make().await).await;
    delivery_delete_is_bound_to_observed_row_identity(make().await).await;
    process_prune_only_deletes_deliveries_for_pruned_processes(make().await).await;
    pruned_delivery_process_is_not_a_recovery_candidate(make().await).await;
    unregistered_delivery_is_offered_to_the_recovery_sweep(make().await).await;
    the_narrow_delivery_worklist_agrees_with_the_delivery_table(make().await).await;
    outstanding_delivery_blocks_interleaved_tombstone_compaction(make().await).await;
}

/// The reserve/start crash window's recovery (ADR 0021, FIG-4090), a law of
/// its own so a crash-window run can repeat it alone.
pub async fn trigger_delivery_recovery<F, Fut>(make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ProcessTriggerRetentionHandles>,
{
    a_reserved_delivery_recovers_through_its_obligation_into_one_bound_process(make().await).await;
}

/// The bind-crash window of [`trigger_delivery_recovery`] once the child has
/// run (ADR 0021, FIG-4203): the child completes and a retention pass runs
/// before the bind recovers, and the delivery still starts exactly one
/// process. Repeated with the pin's release losing its receipt, before and
/// after the release landed.
pub async fn trigger_delivery_pinned_recovery<F, Fut>(make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ProcessTriggerRetentionHandles>,
{
    a_completed_child_whose_bind_was_lost_is_bound_not_started_again(make().await, None).await;
    a_completed_child_whose_bind_was_lost_is_bound_not_started_again(
        make().await,
        Some(crate::testing::TriggerDeliveryPinReleaseLoss::BeforeReleasing),
    )
    .await;
    a_completed_child_whose_bind_was_lost_is_bound_not_started_again(
        make().await,
        Some(crate::testing::TriggerDeliveryPinReleaseLoss::AfterReleasing),
    )
    .await;
}

/// A reserved non-engine target cannot start through the runtime router. Its
/// due pass stalls with a typed refusal instead of leaving the delivery owed.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn trigger_delivery_refusal<F, Fut>(make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ProcessTriggerRetentionHandles>,
{
    let handles = make().await;
    let session = SessionId::from("delivery-refusal-session");
    handles
        .triggers
        .execute_command(
            "delivery-refusal-register",
            TriggerCommand::Register {
                owner_scope: owner(&session),
                actor: actor(&session),
                draft: TriggerSubscriptionDraft {
                    target: ProcessInput::External {
                        metadata: serde_json::Value::Null,
                    },
                    ..draft(&session, "delivery-refusal-key", "delivery-refusal-source")
                },
            },
        )
        .await
        .expect("register non-engine target")
        .expect("store the captured subscription");
    let reserved = handles
        .triggers
        .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            "delivery-refusal-source",
            serde_json::json!({"button": "Blue"}),
            "delivery-refusal-occurrence",
        ))
        .await
        .expect("reserve delivery");
    assert_eq!(reserved.reservations.len(), 1);
    let relay = lash_core::runtime::trigger_delivery::TriggerDeliveryRelay::new(
        Arc::clone(&handles.deliveries),
        lash_core::facade_support::TriggerRouter::new(
            Arc::clone(&handles.triggers),
            crate::ProcessWorkWiring::without_process_work(Arc::clone(&handles.registry)),
        )
        .with_process_artifacts(
            Arc::clone(&handles.process_env),
            crate::ProcessEngineRegistry::new(),
        ),
    );
    let clock = crate::testing::TestClock::new(4_000_000_000_000);
    let pass = lash_core::drive::relay::relay_due(&relay, &clock, std::num::NonZeroUsize::MIN)
        .await
        .expect("run the due pass");
    assert_eq!((pass.claimed, pass.retried, pass.stalled), (1, 0, 1));
    let stalled = handles
        .deliveries
        .list_stalled(None, std::num::NonZeroUsize::MIN)
        .await
        .expect("read typed refusal");
    assert_eq!(stalled.len(), 1);
    assert_eq!(stalled[0].reason, crate::StallReason::Refused);
    assert!(
        stalled[0].last_error.as_deref().is_some_and(
            |error| error.contains("trigger target must be an engine process, got external")
        ),
        "the runtime refuses a non-engine target: {stalled:?}"
    );
    assert!(
        handles
            .registry
            .get_process_by_start_key(&lash_core::facade_support::trigger_delivery_start_key(
                &reserved.reservations[0],
            ))
            .await
            .expect("read the refused start")
            .is_none()
    );
    let next = lash_core::drive::relay::relay_due(&relay, &clock, std::num::NonZeroUsize::MIN)
        .await
        .expect("a later due pass");
    assert_eq!(next.claimed, 0, "the refused delivery is settled");
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn deleted_session_frontier_authorizes_trigger_owner_reclamation(
    handles: ProcessTriggerRetentionHandles,
) {
    const SESSION: &str = "frontier-trigger-retention-session";
    const RECEIPT_ONLY_SESSION: &str = "frontier-trigger-receipt-only-session";
    const KEY: &str = "frontier-trigger-retention-key";
    const REGISTER_OPERATION: &str = "frontier-trigger-retention-register";
    let request = super::session_store_factory::session_store_request(
        &SessionId::from(SESSION),
        "frontier-trigger-retention-model",
        crate::SessionRelation::Root,
    );
    handles
        .sessions
        .admit_view(&request)
        .await
        .expect("materialize trigger owner session");
    let original_draft = draft(
        &SessionId::from(SESSION),
        KEY,
        "frontier-trigger-retention-source",
    );
    let created = handles
        .triggers
        .execute_command(
            REGISTER_OPERATION,
            TriggerCommand::Register {
                owner_scope: owner(&SessionId::from(SESSION)),
                actor: actor(&SessionId::from(SESSION)),
                draft: original_draft.clone(),
            },
        )
        .await
        .expect("register frontier-owned trigger")
        .expect("frontier-owned registration succeeds");
    let TriggerCommandOutcome::Mutation { receipt: created } = created else {
        panic!("register must return a mutation receipt")
    };
    handles
        .triggers
        .execute_command(
            "frontier-trigger-retention-delete",
            TriggerCommand::Delete {
                owner_scope: owner(&SessionId::from(SESSION)),
                actor: actor(&SessionId::from(SESSION)),
                subscription_key: KEY.to_string(),
                expected_revision: created.revision,
            },
        )
        .await
        .expect("delete frontier-owned trigger")
        .expect("frontier-owned delete succeeds");
    handles
        .sessions
        .delete_session(&SessionId::from(SESSION))
        .await
        .expect("delete trigger owner session");
    let receipt_only_request = super::session_store_factory::session_store_request(
        &SessionId::from(RECEIPT_ONLY_SESSION),
        "frontier-trigger-receipt-only-model",
        crate::SessionRelation::Root,
    );
    handles
        .sessions
        .admit_view(&receipt_only_request)
        .await
        .expect("materialize receipt-only trigger owner session");
    handles
        .triggers
        .execute_command(
            "frontier-trigger-receipt-only-operation",
            TriggerCommand::Prune {
                owner_scope: owner(&SessionId::from(RECEIPT_ONLY_SESSION)),
                actor: actor(&SessionId::from(RECEIPT_ONLY_SESSION)),
                subscription_keys: Vec::new(),
            },
        )
        .await
        .expect("journal receipt-only trigger command")
        .expect("receipt-only trigger command succeeds");
    handles
        .sessions
        .delete_session(&SessionId::from(RECEIPT_ONLY_SESSION))
        .await
        .expect("delete receipt-only trigger owner session");

    let report = crate::reconcile_pruned_trigger_deliveries(
        handles.registry.as_ref(),
        handles.triggers.as_ref(),
        Some(handles.sessions.as_ref()),
    )
    .await
    .expect("reconcile deleted trigger owner");
    assert_eq!(report.reclaimed_subscription_count, 1);
    assert_eq!(report.reclaimed_mutation_receipt_count, 3);

    let mut replacement = original_draft;
    replacement.source_key = "frontier-trigger-retention-replacement".to_string();
    let recreated = handles
        .triggers
        .execute_command(
            REGISTER_OPERATION,
            TriggerCommand::Register {
                owner_scope: owner(&SessionId::from(SESSION)),
                actor: actor(&SessionId::from(SESSION)),
                draft: replacement,
            },
        )
        .await
        .expect("reuse operation id after dead-owner receipt reclamation")
        .expect("recreate trigger after dead-owner fence reclamation");
    let TriggerCommandOutcome::Mutation { receipt: recreated } = recreated else {
        panic!("recreate must return a mutation receipt")
    };
    assert_eq!(recreated.revision, 1);
    handles
        .triggers
        .execute_command(
            "frontier-trigger-receipt-only-operation",
            TriggerCommand::Prune {
                owner_scope: owner(&SessionId::from(RECEIPT_ONLY_SESSION)),
                actor: actor(&SessionId::from(RECEIPT_ONLY_SESSION)),
                subscription_keys: vec!["different-content-after-reclaim".to_string()],
            },
        )
        .await
        .expect("reuse receipt-only operation id after owner cascade")
        .expect("receipt-only operation is re-evaluated after owner cascade");
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn zero_match_occurrence_is_reclaimed_at_delivery_reconciliation(
    handles: ProcessTriggerRetentionHandles,
) {
    let ingress = handles
        .triggers
        .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            "zero-match-reconciliation-source",
            serde_json::json!({ "button": "Blue" }),
            "zero-match-reconciliation-occurrence",
        ))
        .await
        .expect("ingest zero-match occurrence");
    assert!(ingress.reservations.is_empty());

    crate::reconcile_pruned_trigger_deliveries(
        handles.registry.as_ref(),
        handles.triggers.as_ref(),
        Some(handles.sessions.as_ref()),
    )
    .await
    .expect("reconcile zero-match occurrence");

    assert!(
        handles
            .triggers
            .list_occurrences(crate::TriggerOccurrenceFilter::default())
            .await
            .expect("list occurrences after reconciliation")
            .is_empty(),
        "a committed zero-match fan-out must be reclaimed at delivery reconciliation"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn delivery_delete_is_bound_to_observed_row_identity(
    handles: ProcessTriggerRetentionHandles,
) {
    const SESSION: &str = "delivery-retention-identity-session";
    register_trigger(
        &handles.triggers,
        &SessionId::from(SESSION),
        "delivery-retention-identity-key",
        "delivery-retention-identity-source",
        "delivery-retention-identity-register",
    )
    .await;
    for occurrence in ["first", "second"] {
        let ingress = handles
            .triggers
            .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
                "ui.button.pressed",
                "delivery-retention-identity-source",
                serde_json::json!({ "button": "Blue" }),
                format!("delivery-retention-identity-{occurrence}"),
            ))
            .await
            .expect("ingest identity-law occurrence");
        assert_eq!(ingress.reservations.len(), 1);
        // A retention candidate is a delivery whose process started.
        start_and_bind_delivery(&handles, &ingress.reservations[0]).await;
    }
    let candidates = handles
        .triggers
        .list_delivery_retention_candidates()
        .await
        .expect("list delivery retention candidates");
    assert_eq!(candidates.len(), 2);

    // Model a row replacement between classification and deletion: the row key
    // now identifies the second row while the stale plan still carries the
    // first row's process id. No current row matches the complete observation.
    let mut stale_observation = candidates[0].clone();
    stale_observation.occurrence_id = candidates[1].occurrence_id.clone();
    stale_observation.subscription_id = candidates[1].subscription_id.clone();
    assert_eq!(
        handles
            .triggers
            .delete_delivery_retention_candidates(&[stale_observation])
            .await
            .expect("apply stale row observation"),
        0,
        "a stale classification must not expand into a process-wide delete"
    );
    assert_eq!(
        handles
            .triggers
            .list_delivery_retention_candidates()
            .await
            .expect("list rows after stale delete")
            .len(),
        2,
        "both the original and replacement rows survive a mismatched observation"
    );

    assert_eq!(
        handles
            .triggers
            .delete_delivery_retention_candidates(std::slice::from_ref(&candidates[0]))
            .await
            .expect("delete exact observed row"),
        1,
        "the exact observed row remains reclaimable"
    );
    assert_eq!(
        handles
            .triggers
            .list_delivery_retention_candidates()
            .await
            .expect("list rows after exact delete"),
        vec![candidates[1].clone()],
        "an exact delete preserves every unlisted row"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn outstanding_delivery_blocks_interleaved_tombstone_compaction(
    handles: ProcessTriggerRetentionHandles,
) {
    const SESSION: &str = "process-compact-interleave-session";
    register_trigger(
        &handles.triggers,
        &SessionId::from(SESSION),
        "process-compact-interleave-key",
        "process-compact-interleave-source",
        "process-compact-interleave-register",
    )
    .await;
    let ingress = handles
        .triggers
        .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            "process-compact-interleave-source",
            serde_json::json!({ "button": "Blue" }),
            "process-compact-interleave-occurrence",
        ))
        .await
        .expect("ingest occurrence");
    assert_eq!(ingress.reservations.len(), 1);
    let process_id = start_and_bind_delivery(&handles, &ingress.reservations[0]).await;
    handles
        .registry
        .complete_process(
            &process_id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!("done"),
            )),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete delivery process");

    // Compaction's preceding reconciliation observes the process as live and
    // preserves its delivery. A concurrent retention writer then prunes the
    // process before raw compaction starts. The raw lever must perform its own
    // complete delivery survey and refuse the new tombstone.
    assert_eq!(
        crate::reconcile_pruned_trigger_deliveries(
            handles.registry.as_ref(),
            handles.triggers.as_ref(),
            Some(handles.sessions.as_ref()),
        )
        .await
        .expect("reconcile while process is live")
        .reclaimed_delivery_count,
        0
    );
    handles
        .registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .expect("interleaved process prune");
    assert_eq!(
        handles
            .registry
            .compact_process_tombstones(
                u64::MAX,
                ProjectionWatermark::NoProjector,
                Some(handles.triggers.as_ref()),
            )
            .await
            .expect("delivery-aware tombstone compaction"),
        0,
        "compaction must refuse a tombstone guarded by an outstanding delivery"
    );
    assert_eq!(
        handles
            .registry
            .filter_tombstoned_process_ids(std::slice::from_ref(&process_id))
            .await
            .expect("classify guarded tombstone"),
        vec![process_id.clone()],
        "the outstanding delivery's tombstone must remain durable"
    );
    assert_eq!(
        handles
            .triggers
            .list_deliveries_by_process_id(&process_id)
            .await
            .expect("list guarded delivery")
            .len(),
        1,
        "refusing compaction preserves the delivery beside its tombstone"
    );

    assert_eq!(
        crate::reconcile_pruned_trigger_deliveries(
            handles.registry.as_ref(),
            handles.triggers.as_ref(),
            Some(handles.sessions.as_ref()),
        )
        .await
        .expect("reconcile guarded delivery")
        .reclaimed_delivery_count,
        1
    );
    assert_eq!(
        handles
            .registry
            .compact_process_tombstones(
                u64::MAX,
                ProjectionWatermark::NoProjector,
                Some(handles.triggers.as_ref()),
            )
            .await
            .expect("compact reconciled tombstone"),
        1,
        "a later cycle may compact after the delivery is gone"
    );
    assert!(
        handles
            .triggers
            .list_deliveries_by_process_id(&process_id)
            .await
            .expect("list delivery after compaction")
            .is_empty(),
        "compaction can never orphan the delivery"
    );
    lash_core::testing::runbook_evidence::checkpoint(serde_json::json!({
        "checkpoint": "outstanding_delivery_refuses_tombstone_compaction",
        "process_id": process_id,
        "guarded_compacted_tombstones": 0,
        "tombstone_durable_while_guarded": true,
        "deliveries_beside_guarded_tombstone": 1,
        "reclaimed_delivery_count_after_reconcile": 1,
        "compacted_tombstones_after_reconcile": 1,
        "deliveries_after_compaction": 0,
    }));
}

fn owner(session_id: &SessionId) -> TriggerOwnerScope {
    TriggerOwnerScope::session(session_id)
}

fn actor(session_id: &SessionId) -> ProcessOriginator {
    ProcessOriginator::session(SessionScope::new(session_id))
}

fn draft(session_id: &SessionId, key: &str, source_key: &str) -> TriggerSubscriptionDraft {
    let mut input_template = BTreeMap::new();
    input_template.insert("event".to_string(), crate::TriggerInputBinding::Event);
    TriggerSubscriptionDraft {
        source_capture: crate::TriggerSourceCapture::provider(
            ["ui", "button"],
            crate::LashSchema::any(),
            "ui-provider",
            serde_json::json!({"account": "a"}),
        ),
        subscription_key: key.to_string(),
        env_ref: crate::ProcessExecutionEnvRef::new(format!("process-env:{session_id}")),
        wake_target: Some(SessionScope::new(session_id)),
        name: Some("worker".to_string()),
        source_type: "ui.button.pressed".to_string(),
        source_key: source_key.to_string(),
        source: serde_json::json!({ "button": "Blue" }),
        payload_schema: crate::LashSchema::new(serde_json::json!({
            "type": "object",
            "properties": { "button": { "type": "string" } },
            "required": ["button"],
            "additionalProperties": false
        })),
        target: ProcessInput::Engine {
            kind: "test".to_string(),
            payload: serde_json::json!({ "process": "worker" }),
        },
        target_identity: ProcessIdentity::for_definition(
            lash_core::ProcessDefinitionRef::unclaimed(
                "test",
                serde_json::json!({ "process_name": "worker" }),
            ),
            Some("worker".to_string()),
        ),
        event_types: Vec::new(),
        input_template,
        target_label: Some("worker".to_string()),
    }
}

/// Start a reserved delivery's process the way the router does: register it
/// under the delivery's start key, then bind it to the delivery (ADR 0107).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn start_and_bind_delivery(
    handles: &ProcessTriggerRetentionHandles,
    reservation: &crate::TriggerDeliveryReservation,
) -> ProcessId {
    let start_key = crate::StartKey::for_trigger_delivery(
        crate::DERIVED_START_KEYS,
        &reservation.occurrence.occurrence_id,
        &reservation.subscription.subscription_id,
        &reservation.subscription.incarnation,
        reservation.subscription.revision,
    );
    let process_id = handles
        .registry
        .register_process(
            ProcessRegistration::new(
                ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
                ProcessIdentity::new("test"),
            ))
            .with_start_key(Some(start_key)),
        )
        .await
        .expect("register delivery process")
        .id;
    handles
        .triggers
        .bind_delivery_process(
            &reservation.occurrence.occurrence_id,
            &reservation.subscription.subscription_id,
            &process_id,
        )
        .await
        .expect("bind the delivery's process");
    process_id
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn register_trigger(
    triggers: &Arc<dyn TriggerStore>,
    session_id: &SessionId,
    key: &str,
    source_key: &str,
    operation_id: &str,
) {
    triggers
        .execute_command(
            operation_id,
            TriggerCommand::Register {
                owner_scope: owner(session_id),
                actor: actor(session_id),
                draft: draft(session_id, key, source_key),
            },
        )
        .await
        .expect("register trigger call")
        .expect("register trigger succeeds");
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn process_prune_preserves_trigger_mutation_receipts(
    handles: ProcessTriggerRetentionHandles,
) {
    const SESSION: &str = "process-prune-receipt-session";
    const KEY: &str = "process-prune-receipt-key";
    register_trigger(
        &handles.triggers,
        &SessionId::from(SESSION),
        KEY,
        "process-prune-receipt-v1",
        "process-prune-receipt-register",
    )
    .await;
    let update = TriggerCommand::Update {
        owner_scope: owner(&SessionId::from(SESSION)),
        actor: actor(&SessionId::from(SESSION)),
        subscription_key: KEY.to_string(),
        draft: draft(&SessionId::from(SESSION), KEY, "process-prune-receipt-v2"),
        expected_revision: 1,
    };
    let committed = handles
        .triggers
        .execute_command("process-prune-receipt-update", update.clone())
        .await
        .expect("update trigger call")
        .expect("update trigger succeeds");

    let report = handles
        .registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .expect("prune with no processes");
    assert_eq!(report.pruned_processes, 0, "no process was eligible");

    let retried = handles
        .triggers
        .execute_command("process-prune-receipt-update", update)
        .await
        .expect("retry trigger update")
        .expect("retry returns the committed result");
    assert_eq!(
        retried, committed,
        "process prune must preserve the original trigger mutation receipt"
    );
    lash_core::testing::runbook_evidence::checkpoint(serde_json::json!({
        "checkpoint": "prune_preserves_trigger_mutation_receipt",
        "session_id": SESSION,
        "subscription_key": KEY,
        "operation_id": "process-prune-receipt-update",
        "pruned_processes": report.pruned_processes,
        "replayed_receipt_matches_committed": retried == committed,
    }));
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn prune_with_trigger_cleanup(handles: &ProcessTriggerRetentionHandles) {
    handles
        .registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .expect("prune terminal processes");
    crate::reconcile_pruned_trigger_deliveries(
        handles.registry.as_ref(),
        handles.triggers.as_ref(),
        Some(handles.sessions.as_ref()),
    )
    .await
    .expect("reconcile pruned trigger deliveries");
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn process_prune_only_deletes_deliveries_for_pruned_processes(
    handles: ProcessTriggerRetentionHandles,
) {
    const SESSION: &str = "process-prune-scope-session";
    register_trigger(
        &handles.triggers,
        &SessionId::from(SESSION),
        "process-prune-scope-key",
        "process-prune-scope-source",
        "process-prune-scope-register",
    )
    .await;
    let first = handles
        .triggers
        .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            "process-prune-scope-source",
            serde_json::json!({ "button": "Blue" }),
            "process-prune-scope-first",
        ))
        .await
        .expect("ingest first occurrence");
    let second = handles
        .triggers
        .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            "process-prune-scope-source",
            serde_json::json!({ "button": "Blue" }),
            "process-prune-scope-second",
        ))
        .await
        .expect("ingest second occurrence");
    assert_eq!(first.reservations.len(), 1);
    assert_eq!(second.reservations.len(), 1);
    let pruned_id = start_and_bind_delivery(&handles, &first.reservations[0]).await;
    let live_id = start_and_bind_delivery(&handles, &second.reservations[0]).await;

    handles
        .registry
        .complete_process(
            &pruned_id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!("done"),
            )),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete prunable delivery process");

    prune_with_trigger_cleanup(&handles).await;

    assert!(
        handles
            .triggers
            .list_deliveries_by_process_id(&pruned_id)
            .await
            .expect("list pruned deliveries")
            .is_empty(),
        "process prune must delete a pruned process's trigger delivery"
    );
    assert_eq!(
        handles
            .triggers
            .list_deliveries_by_process_id(&live_id)
            .await
            .expect("list live deliveries")
            .len(),
        1,
        "process prune must preserve deliveries for processes it did not prune"
    );
    lash_core::testing::runbook_evidence::checkpoint(serde_json::json!({
        "checkpoint": "prune_reconciles_only_pruned_process_deliveries",
        "pruned_process_id": pruned_id,
        "live_process_id": live_id,
        "pruned_process_deliveries": 0,
        "live_process_deliveries": 1,
    }));
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn pruned_delivery_process_is_not_a_recovery_candidate(
    handles: ProcessTriggerRetentionHandles,
) {
    const SESSION: &str = "process-prune-tombstone-session";
    register_trigger(
        &handles.triggers,
        &SessionId::from(SESSION),
        "process-prune-tombstone-key",
        "process-prune-tombstone-source",
        "process-prune-tombstone-register",
    )
    .await;
    let ingress = handles
        .triggers
        .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            "process-prune-tombstone-source",
            serde_json::json!({ "button": "Blue" }),
            "process-prune-tombstone-occurrence",
        ))
        .await
        .expect("ingest occurrence");
    assert_eq!(ingress.reservations.len(), 1);
    let process_id = start_and_bind_delivery(&handles, &ingress.reservations[0]).await;
    handles
        .registry
        .complete_process(
            &process_id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!("done"),
            )),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete delivery process");

    prune_with_trigger_cleanup(&handles).await;

    assert!(
        handles
            .triggers
            .list_deliveries_by_process_id(&process_id)
            .await
            .expect("list delivery after prune")
            .is_empty(),
        "pruned delivery must not survive"
    );
    assert!(
        handles
            .registry
            .filter_unregistered_process_ids(std::slice::from_ref(&process_id))
            .await
            .expect("filter recovery candidates")
            .is_empty(),
        "a tombstoned process must not be offered back to the recovery sweep"
    );
}

/// The delivery table's view of the reserve/start crash window, from the
/// store side.
///
/// Recovery itself runs through the reservation's `TriggerDelivery`
/// obligation ([`trigger_delivery_recovery`]); these two reads are the
/// delivery table's own account of the same window: `TriggerStore::list_deliveries`
/// lists every reservation, and `ProcessRegistry::filter_unregistered_process_ids`
/// narrows process ids to those with no row. The neighbouring law covers the
/// *negative* direction only -- a pruned process is not offered back. This
/// one covers the other: a reserved-but-unstarted delivery IS listed unbound,
/// and a started one is not reported unregistered.
///
/// A backend whose `list_deliveries` quietly filtered to deliveries with a
/// live subscription, or whose `filter_unregistered_process_ids` reported a
/// registered process as missing, would misreport the window to every reader
/// of the table -- and would still pass every other law in this group.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn unregistered_delivery_is_offered_to_the_recovery_sweep(
    handles: ProcessTriggerRetentionHandles,
) {
    const SESSION: &str = "delivery-recovery-sweep-session";
    register_trigger(
        &handles.triggers,
        &SessionId::from(SESSION),
        "delivery-recovery-sweep-key",
        "delivery-recovery-sweep-source",
        "delivery-recovery-sweep-register",
    )
    .await;
    let ingress = handles
        .triggers
        .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            "delivery-recovery-sweep-source",
            serde_json::json!({ "button": "Blue" }),
            "delivery-recovery-sweep-occurrence",
        ))
        .await
        .expect("ingest occurrence");
    assert_eq!(ingress.reservations.len(), 1);
    let reservation = ingress.reservations[0].clone();
    assert_eq!(
        reservation.process_id, None,
        "a reservation is unbound until its start registers a process (ADR 0107)"
    );

    // The crash window itself: the delivery row is reserved and no process was
    // ever registered for it. The sweep has to be able to see that fact: an
    // unbound delivery is the recovery candidate.
    let reserved = handles
        .triggers
        .list_deliveries()
        .await
        .expect("list deliveries");
    assert!(
        reserved.iter().any(|delivery| {
            delivery.occurrence.occurrence_id == reservation.occurrence.occurrence_id
                && delivery.process_id.is_none()
        }),
        "a reserved delivery whose process was never registered must stay a \
         recovery candidate in the direct delivery-table view"
    );

    // Starting it closes the window. The same reads must now agree that there
    // is nothing to recover, or the sweep starts the process a second time.
    let process_id = start_and_bind_delivery(&handles, &reservation).await;
    assert!(
        handles
            .registry
            .filter_unregistered_process_ids(std::slice::from_ref(&process_id))
            .await
            .expect("filter recovery candidates after registration")
            .is_empty(),
        "a registered delivery process must not be offered to the sweep again"
    );
    assert!(
        handles
            .triggers
            .list_deliveries()
            .await
            .expect("list deliveries after registration")
            .iter()
            .any(|delivery| delivery.process_id.as_ref() == Some(&process_id)),
        "the delivery row itself outlives the start, bound to its process: it is \
         retention's to reclaim, not the sweep's"
    );
}

/// ADR 0021's recovery, through the obligation outbox (ADR 0109, FIG-4090).
///
/// The reserving transaction arms the delivery's `TriggerDelivery`
/// obligation, and the write that binds the delivery to its process delivers
/// it. A crash after the reservation and before the start leaves the row due,
/// and nothing emits the occurrence again. A restarted deployment's relay
/// takes the due row, starts the delivery from the reservation the store
/// holds, and binds it: exactly one process, under the delivery's start key,
/// bound to the delivery.
///
/// The second crash window sits between the registration and the bind. The
/// next pass registers again under the same start key, which finds the
/// process the first attempt registered, and binds that one: still exactly
/// one process. A pass after recovery finds nothing due.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn a_reserved_delivery_recovers_through_its_obligation_into_one_bound_process(
    handles: ProcessTriggerRetentionHandles,
) {
    const SESSION: &str = "delivery-obligation-session";
    const SOURCE: &str = "delivery-obligation-source";
    let session_id = SessionId::from(SESSION);
    // The environment the subscription names is durable before any delivery
    // starts, the way a registration publishes it.
    let spec = crate::ProcessExecutionEnvSpec::new(
        crate::PluginOptions::default(),
        crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
    );
    let env_ref = spec.stable_ref().expect("stable env ref");
    handles
        .process_env
        .publish_process_execution_env(
            &crate::ReferrerClaim::unguarded(crate::ArtifactReferrer::HostPin(
                crate::HostArtifactPin::mint(),
            ))
            .expect("host pin claim"),
            &env_ref,
            &spec.to_store_bytes().expect("encode env"),
        )
        .await
        .expect("publish the subscription's environment");
    handles
        .triggers
        .execute_command(
            "delivery-obligation-register",
            TriggerCommand::Register {
                owner_scope: owner(&session_id),
                actor: actor(&session_id),
                draft: TriggerSubscriptionDraft {
                    env_ref,
                    ..draft(&session_id, "delivery-obligation-key", SOURCE)
                },
            },
        )
        .await
        .expect("register trigger call")
        .expect("register trigger succeeds");
    let reserve = |idempotency_key: &'static str| {
        let triggers = Arc::clone(&handles.triggers);
        async move {
            let ingress = triggers
                .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
                    "ui.button.pressed",
                    SOURCE,
                    serde_json::json!({ "button": "Blue" }),
                    idempotency_key,
                ))
                .await
                .expect("ingest occurrence");
            assert_eq!(ingress.reservations.len(), 1, "one subscription matches");
            ingress.reservations[0].clone()
        }
    };
    // A restarted deployment's clock, past every due instant the store armed.
    let clock = crate::testing::TestClock::new(4_000_000_000_000);
    let relay_over = |triggers: Arc<dyn TriggerStore>| {
        lash_core::runtime::trigger_delivery::TriggerDeliveryRelay::new(
            Arc::clone(&handles.deliveries),
            lash_core::facade_support::TriggerRouter::new(
                triggers,
                crate::ProcessWorkWiring::without_process_work(Arc::clone(&handles.registry)),
            )
            .with_process_artifacts(
                Arc::clone(&handles.process_env),
                crate::ProcessEngineRegistry::new().with_registration(
                    crate::ProcessEngineRegistration::accepting(Arc::new(TriggerTargetEngine)),
                ),
            ),
        )
    };
    let page = std::num::NonZeroUsize::new(16).expect("a nonzero page");
    let occurrences = || async {
        handles
            .triggers
            .list_occurrences(crate::TriggerOccurrenceFilter::default())
            .await
            .expect("list occurrences")
            .len()
    };

    // Crash after the reservation, before the registration: the row is
    // reserved, unbound, and owes its start.
    let reserved = reserve("delivery-obligation-reserve-crash").await;
    assert_eq!(reserved.process_id, None);
    let start_key = lash_core::facade_support::trigger_delivery_start_key(&reserved);
    assert!(
        handles
            .registry
            .get_process_by_start_key(&start_key)
            .await
            .expect("read the start key")
            .is_none(),
        "the crash left no process"
    );
    let occurrences_before = occurrences().await;
    let pass = lash_core::drive::relay::relay_due(
        &relay_over(Arc::clone(&handles.triggers)),
        &clock,
        page,
    )
    .await
    .expect("the restarted relay's due pass");
    assert_eq!(
        (pass.claimed, pass.retried, pass.stalled),
        (1, 0, 0),
        "the restart claims the reserved delivery and starts it: {pass:?}"
    );
    let recovered = handles
        .registry
        .get_process_by_start_key(&start_key)
        .await
        .expect("read the start key")
        .expect("recovery registered the delivery's process");
    let bound = handles
        .triggers
        .list_deliveries_by_occurrence_id(&reserved.occurrence.occurrence_id)
        .await
        .expect("list the delivery");
    assert_eq!(bound.len(), 1);
    assert_eq!(
        bound[0].process_id.as_ref(),
        Some(&recovered.id),
        "the delivery is bound to the one process its start key registered"
    );
    assert_eq!(
        occurrences().await,
        occurrences_before,
        "recovery starts from the reservation, never from a re-emitted occurrence"
    );

    // Crash after the registration, before the bind.
    let reserved = reserve("delivery-obligation-register-crash").await;
    let start_key = lash_core::facade_support::trigger_delivery_start_key(&reserved);
    let crashing = Arc::new(BindCrashesOnce::new(Arc::clone(&handles.triggers)));
    clock.advance(3_600_000);
    let pass = lash_core::drive::relay::relay_due(
        &relay_over(Arc::clone(&crashing) as Arc<dyn TriggerStore>),
        &clock,
        page,
    )
    .await
    .expect("the crashing relay's due pass");
    assert_eq!(
        (pass.claimed, pass.retried, pass.stalled),
        (1, 1, 0),
        "the bind's crash leaves the delivery owed: {pass:?}"
    );
    let registered = handles
        .registry
        .get_process_by_start_key(&start_key)
        .await
        .expect("read the start key")
        .expect("the crash came after the registration");
    assert_eq!(
        handles
            .triggers
            .list_deliveries_by_occurrence_id(&reserved.occurrence.occurrence_id)
            .await
            .expect("list the delivery")[0]
            .process_id,
        None,
        "the crash came before the bind"
    );
    clock.advance(3_600_000);
    let pass = lash_core::drive::relay::relay_due(
        &relay_over(Arc::clone(&handles.triggers)),
        &clock,
        page,
    )
    .await
    .expect("the restarted relay's due pass");
    assert_eq!(
        (pass.claimed, pass.retried, pass.stalled),
        (1, 0, 0),
        "the restart retakes the owed delivery: {pass:?}"
    );
    assert_eq!(
        handles
            .registry
            .get_process_by_start_key(&start_key)
            .await
            .expect("read the start key")
            .map(|record| record.id),
        Some(registered.id.clone()),
        "the start key found the registered process again rather than a second one"
    );
    assert_eq!(
        handles
            .triggers
            .list_deliveries_by_occurrence_id(&reserved.occurrence.occurrence_id)
            .await
            .expect("list the delivery")[0]
            .process_id,
        Some(registered.id),
        "the delivery is bound to the process the first attempt registered"
    );

    // Both deliveries are delivered: a later pass has nothing to recover.
    clock.advance(3_600_000);
    let pass = lash_core::drive::relay::relay_due(
        &relay_over(Arc::clone(&handles.triggers)),
        &clock,
        page,
    )
    .await
    .expect("a later due pass");
    assert_eq!(pass.claimed, 0, "a bound delivery owes nothing: {pass:?}");
    assert_eq!(
        handles
            .deliveries
            .count_stalled()
            .await
            .expect("count stalled deliveries"),
        0
    );
}

/// A completed child whose bind was lost is bound, never started again (ADR
/// 0021, FIG-4203).
///
/// The delivery's first attempt registers its child and the child runs: it
/// records its one effect and completes. The bind is lost. A retention pass
/// then runs with a cutoff past the child's completion and no projector, the
/// most destructive prune there is. The child's registration pinned it until
/// the bind commits, so the prune keeps it, and the recovery that follows
/// finds it under the delivery's start key and binds it: the effect ran
/// once.
///
/// `lost_release` repeats the law with the pin's release losing its receipt
/// after the recovery's bind: before the release landed, so the pin stays
/// until the next retention pass releases it, or after. Either way the next
/// retention pass prunes the bound child, and nothing starts it again.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn a_completed_child_whose_bind_was_lost_is_bound_not_started_again(
    handles: ProcessTriggerRetentionHandles,
    lost_release: Option<crate::testing::TriggerDeliveryPinReleaseLoss>,
) {
    const SESSION: &str = "delivery-pin-session";
    const SOURCE: &str = "delivery-pin-source";
    let session_id = SessionId::from(SESSION);
    let spec = crate::ProcessExecutionEnvSpec::new(
        crate::PluginOptions::default(),
        crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
    );
    let env_ref = spec.stable_ref().expect("stable env ref");
    handles
        .process_env
        .publish_process_execution_env(
            &crate::ReferrerClaim::unguarded(crate::ArtifactReferrer::HostPin(
                crate::HostArtifactPin::mint(),
            ))
            .expect("host pin claim"),
            &env_ref,
            &spec.to_store_bytes().expect("encode env"),
        )
        .await
        .expect("publish the subscription's environment");
    // No wake target: a wake still owed would keep the completed child from
    // being pruned on its own, and the law is about the pin.
    handles
        .triggers
        .execute_command(
            "delivery-pin-register",
            TriggerCommand::Register {
                owner_scope: owner(&session_id),
                actor: actor(&session_id),
                draft: TriggerSubscriptionDraft {
                    env_ref,
                    wake_target: None,
                    ..draft(&session_id, "delivery-pin-key", SOURCE)
                },
            },
        )
        .await
        .expect("register trigger call")
        .expect("register trigger succeeds");
    let ingress = handles
        .triggers
        .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            SOURCE,
            serde_json::json!({ "button": "Blue" }),
            "delivery-pin-occurrence",
        ))
        .await
        .expect("ingest occurrence");
    assert_eq!(ingress.reservations.len(), 1, "one subscription matches");
    let reserved = ingress.reservations[0].clone();
    let occurrence_id = reserved.occurrence.occurrence_id.clone();
    let start_key = lash_core::facade_support::trigger_delivery_start_key(&reserved);

    // A restarted deployment's clock, past every due instant the stores armed.
    let clock = Arc::new(crate::testing::TestClock::new(4_000_000_000_000));
    let faults = crate::testing::ProcessRegistryFaults::new(Arc::clone(&handles.registry));
    let watched = crate::facade_support::watch_process_registry(Arc::new(faults.clone()));
    let engine = Arc::new(EffectRecordingEngine::new(
        Arc::clone(&handles.registry),
        crate::NoProcessWork::new(&watched),
    ));
    let process_work = crate::ProcessWorkWiring::new(
        watched,
        Arc::clone(&engine) as Arc<dyn crate::ProcessWorkSubstrate>,
    );
    let relay_over = |triggers: Arc<dyn TriggerStore>| {
        lash_core::runtime::trigger_delivery::TriggerDeliveryRelay::new(
            Arc::clone(&handles.deliveries),
            lash_core::facade_support::TriggerRouter::new(triggers, process_work.clone())
                .with_process_artifacts(
                    Arc::clone(&handles.process_env),
                    crate::ProcessEngineRegistry::new().with_registration(
                        crate::ProcessEngineRegistration::accepting(Arc::new(TriggerTargetEngine)),
                    ),
                )
                .with_process_starts(
                    Arc::clone(&handles.process_starts),
                    Arc::clone(&clock) as Arc<dyn crate::Clock>,
                ),
        )
    };
    let page = std::num::NonZeroUsize::new(16).expect("a nonzero page");
    let bound_process = || async {
        handles
            .triggers
            .list_deliveries_by_occurrence_id(&occurrence_id)
            .await
            .expect("list the delivery")
            .into_iter()
            .map(|delivery| delivery.process_id)
            .collect::<Vec<_>>()
    };
    let pinned = || async {
        handles
            .registry
            .list_trigger_delivery_pins()
            .await
            .expect("list trigger delivery pins")
            .into_iter()
            .map(|pinned| pinned.process_id)
            .collect::<Vec<_>>()
    };
    // The retention pass a host runs: release the pins whose delivery no
    // longer needs them, prune every retired row the registry lets go, then
    // reclaim the deliveries of pruned processes.
    let retention_pass = || async {
        let released = lash_core::facade_support::release_bound_trigger_delivery_pins(
            handles.registry.as_ref(),
            handles.triggers.as_ref(),
        )
        .await
        .expect("release bound trigger delivery pins");
        let report = handles
            .registry
            .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
            .await
            .expect("prune terminal processes");
        crate::reconcile_pruned_trigger_deliveries(
            handles.registry.as_ref(),
            handles.triggers.as_ref(),
            Some(handles.sessions.as_ref()),
        )
        .await
        .expect("reconcile pruned trigger deliveries");
        (released, report.pruned_processes)
    };

    // 1. The first attempt registers and starts the child; its bind is lost.
    let crashing = Arc::new(BindCrashesOnce::new(Arc::clone(&handles.triggers)));
    let pass = lash_core::drive::relay::relay_due(
        &relay_over(Arc::clone(&crashing) as Arc<dyn TriggerStore>),
        clock.as_ref(),
        page,
    )
    .await
    .expect("the crashing relay's due pass");
    assert_eq!(
        (pass.claimed, pass.retried, pass.stalled),
        (1, 1, 0),
        "the lost bind leaves the delivery owed: {pass:?}"
    );
    let child = handles
        .registry
        .get_process_by_start_key(&start_key)
        .await
        .expect("read the start key")
        .expect("the first attempt registered the child");
    assert_eq!(bound_process().await, vec![None], "the bind was lost");

    // 2. The child ran: it recorded its one effect and completed.
    assert_eq!(engine.effects(), vec![child.id.clone()]);
    assert!(
        handles
            .registry
            .get_process(&child.id)
            .await
            .expect("read the child")
            .expect("the child is retained")
            .status
            .is_terminal(),
        "the child completed before the bind recovered"
    );
    assert_eq!(
        pinned().await,
        vec![child.id.clone()],
        "the pin holds the child"
    );

    // 3. The most destructive retention pass keeps the pinned child.
    assert_eq!(
        retention_pass().await,
        (0, 0),
        "an unbound delivery keeps its pin, and a pinned child is not pruned"
    );
    assert_eq!(
        handles
            .registry
            .get_process_by_start_key(&start_key)
            .await
            .expect("read the start key")
            .map(|record| record.id),
        Some(child.id.clone()),
        "the start key still leads to the completed child"
    );

    // 4. The recovery binds the child the first attempt registered.
    if let Some(point) = lost_release {
        faults.lose_next_trigger_delivery_pin_release(point);
    }
    clock.advance(3_600_000);
    let pass = lash_core::drive::relay::relay_due(
        &relay_over(Arc::clone(&handles.triggers)),
        clock.as_ref(),
        page,
    )
    .await
    .expect("the restarted relay's due pass");
    assert_eq!(
        (pass.claimed, pass.retried, pass.stalled),
        (1, 0, 0),
        "the restart retakes the owed delivery and binds it: {pass:?}"
    );
    assert_eq!(
        bound_process().await,
        vec![Some(child.id.clone())],
        "the delivery is bound to the child the first attempt registered"
    );
    assert_eq!(
        engine.effects(),
        vec![child.id.clone()],
        "the occurrence's effect ran once"
    );
    let still_pinned = match lost_release {
        Some(crate::testing::TriggerDeliveryPinReleaseLoss::BeforeReleasing) => {
            vec![child.id.clone()]
        }
        Some(crate::testing::TriggerDeliveryPinReleaseLoss::AfterReleasing) | None => Vec::new(),
    };
    assert_eq!(
        pinned().await,
        still_pinned,
        "the bind released the pin unless its release never landed"
    );

    // The next retention pass releases a pin the release left behind, then
    // prunes the bound child and reclaims its delivery.
    let released = usize::from(!still_pinned.is_empty());
    assert_eq!(
        retention_pass().await,
        (released, 1),
        "the bound child's pin is gone and the child is pruned"
    );
    assert_eq!(pinned().await, Vec::<ProcessId>::new());
    let pruned = handles.registry.get_process(&child.id).await;
    assert!(
        matches!(
            pruned,
            Err(crate::PluginError::ProcessNoLongerRetained { .. })
        ),
        "the bound child was pruned: {pruned:?}"
    );
    assert_eq!(
        bound_process().await,
        Vec::<Option<ProcessId>>::new(),
        "the pruned child's delivery was reclaimed"
    );

    // Nothing is owed and nothing ran again.
    clock.advance(3_600_000);
    let pass = lash_core::drive::relay::relay_due(
        &relay_over(Arc::clone(&handles.triggers)),
        clock.as_ref(),
        page,
    )
    .await
    .expect("a later due pass");
    assert_eq!(pass.claimed, 0, "a bound delivery owes nothing: {pass:?}");
    assert_eq!(engine.effects(), vec![child.id]);
    assert_eq!(
        handles
            .deliveries
            .count_stalled()
            .await
            .expect("count stalled deliveries"),
        0
    );
}

/// The engine a delivery's start is delivered to, as a workflow substrate
/// keyed by process id: it runs the delivery's target, which records one
/// durable effect, and completes the process under its workflow key. A
/// repeated send for a process it already ran coalesces, as a workflow
/// engine's does, so every recorded effect is a distinct process's run.
struct EffectRecordingEngine {
    registry: Arc<dyn ProcessRegistry>,
    waits: crate::NoProcessWork,
    effects: std::sync::Mutex<Vec<ProcessId>>,
}

impl EffectRecordingEngine {
    fn new(registry: Arc<dyn ProcessRegistry>, waits: crate::NoProcessWork) -> Self {
        Self {
            registry,
            waits,
            effects: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The processes whose run recorded the effect, in run order.
    fn effects(&self) -> Vec<ProcessId> {
        self.effects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait::async_trait]
impl crate::ProcessWorkSubstrate for EffectRecordingEngine {
    async fn deliver_process_start(
        &self,
        record: &crate::ProcessRecord,
    ) -> Result<(), crate::PluginError> {
        {
            let mut effects = self
                .effects
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if effects.contains(&record.id) {
                return Ok(());
            }
            effects.push(record.id.clone());
        }
        self.registry
            .complete_process(
                &record.id,
                ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                    serde_json::json!({ "effects": 1 }),
                )),
                ProcessCompletionAuthority::WorkflowKey {
                    workflow_key: record.id.to_string(),
                },
            )
            .await
            .map(drop)
    }

    async fn await_process_terminal(
        &self,
        process_id: &ProcessId,
    ) -> Result<crate::ProcessTerminalWait, crate::PluginError> {
        self.waits.await_process_terminal(process_id).await
    }

    async fn deliver_cancel(
        &self,
        process_id: &ProcessId,
        request: &crate::CancelRequest,
        key: &str,
    ) -> Result<(), crate::PluginError> {
        self.waits.deliver_cancel(process_id, request, key).await
    }

    async fn publish_process_terminal(
        &self,
        process_id: &ProcessId,
        output: &ProcessAwaitOutput,
        key: &str,
    ) -> Result<(), crate::PluginError> {
        self.waits
            .publish_process_terminal(process_id, output, key)
            .await
    }
}

/// The `test` engine the law's subscription targets: it names no artifacts,
/// so a start stages only its environment.
struct TriggerTargetEngine;

#[async_trait::async_trait]
impl crate::ProcessEngine for TriggerTargetEngine {
    fn kind(&self) -> &'static str {
        "test"
    }

    async fn run(
        &self,
        _context: crate::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        Ok(
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            ))
            .into(),
        )
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<crate::ArtifactName>, crate::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &crate::ReferrerClaim,
        _artifact_ref: &str,
    ) -> Result<(), crate::PluginError> {
        Ok(())
    }
}

/// A trigger store whose first delivery bind fails as a crash would: the
/// registration before it landed, the bind did not.
struct BindCrashesOnce {
    inner: Arc<dyn TriggerStore>,
    crashed: std::sync::atomic::AtomicBool,
}

impl BindCrashesOnce {
    fn new(inner: Arc<dyn TriggerStore>) -> Self {
        Self {
            inner,
            crashed: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[async_trait::async_trait]
impl TriggerStore for BindCrashesOnce {
    async fn execute_command(
        &self,
        operation_id: &str,
        command: TriggerCommand,
    ) -> Result<crate::TriggerEffectResult, crate::PluginError> {
        self.inner.execute_command(operation_id, command).await
    }

    async fn list_subscriptions(
        &self,
        filter: crate::TriggerSubscriptionFilter,
    ) -> Result<Vec<crate::TriggerSubscriptionRecord>, crate::PluginError> {
        self.inner.list_subscriptions(filter).await
    }

    async fn delete_session_subscriptions(
        &self,
        session_id: &SessionId,
    ) -> Result<usize, crate::PluginError> {
        self.inner.delete_session_subscriptions(session_id).await
    }

    async fn ingest_occurrence(
        &self,
        request: crate::TriggerOccurrenceRequest,
    ) -> Result<crate::TriggerIngressReceipt, crate::PluginError> {
        self.inner.ingest_occurrence(request).await
    }

    async fn list_occurrences(
        &self,
        filter: crate::TriggerOccurrenceFilter,
    ) -> Result<Vec<crate::TriggerOccurrenceRecord>, crate::PluginError> {
        self.inner.list_occurrences(filter).await
    }

    async fn list_deliveries_by_occurrence_id(
        &self,
        occurrence_id: &str,
    ) -> Result<Vec<crate::TriggerDeliveryReservation>, crate::PluginError> {
        self.inner
            .list_deliveries_by_occurrence_id(occurrence_id)
            .await
    }

    async fn list_deliveries_by_subscription_id(
        &self,
        subscription_id: &str,
    ) -> Result<Vec<crate::TriggerDeliveryReservation>, crate::PluginError> {
        self.inner
            .list_deliveries_by_subscription_id(subscription_id)
            .await
    }

    async fn list_deliveries_by_process_id(
        &self,
        process_id: &ProcessId,
    ) -> Result<Vec<crate::TriggerDeliveryReservation>, crate::PluginError> {
        self.inner.list_deliveries_by_process_id(process_id).await
    }

    async fn list_deliveries(
        &self,
    ) -> Result<Vec<crate::TriggerDeliveryReservation>, crate::PluginError> {
        self.inner.list_deliveries().await
    }

    async fn bind_delivery_process(
        &self,
        occurrence_id: &str,
        subscription_id: &str,
        process_id: &ProcessId,
    ) -> Result<(), crate::PluginError> {
        if !self.crashed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Err(crate::PluginError::Session(
                "the deployment crashed before the delivery's bind".to_string(),
            ));
        }
        self.inner
            .bind_delivery_process(occurrence_id, subscription_id, process_id)
            .await
    }

    async fn list_delivery_process_ids(&self) -> Result<Vec<ProcessId>, crate::PluginError> {
        self.inner.list_delivery_process_ids().await
    }

    async fn list_delivery_retention_candidates(
        &self,
    ) -> Result<Vec<crate::TriggerDeliveryRetentionCandidate>, crate::PluginError> {
        self.inner.list_delivery_retention_candidates().await
    }

    async fn list_session_owner_ids_for_retention(
        &self,
    ) -> Result<Vec<SessionId>, crate::PluginError> {
        self.inner.list_session_owner_ids_for_retention().await
    }

    async fn reconcile_trigger_retention(
        &self,
        candidates: &[crate::TriggerDeliveryRetentionCandidate],
        deleted_session_ids: &[SessionId],
    ) -> Result<crate::TriggerRetentionReconciliationReport, crate::PluginError> {
        self.inner
            .reconcile_trigger_retention(candidates, deleted_session_ids)
            .await
    }

    async fn delete_delivery_retention_candidates(
        &self,
        candidates: &[crate::TriggerDeliveryRetentionCandidate],
    ) -> Result<usize, crate::PluginError> {
        self.inner
            .delete_delivery_retention_candidates(candidates)
            .await
    }

    async fn reclaim_trigger_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> crate::TriggerOccurrenceReclamationResult {
        self.inner
            .reclaim_trigger_occurrences(cutoff_epoch_ms)
            .await
    }

    async fn prune_mutation_receipts(
        &self,
        cutoff_epoch_ms: u64,
    ) -> Result<usize, crate::PluginError> {
        self.inner.prune_mutation_receipts(cutoff_epoch_ms).await
    }

    async fn prune_non_fired_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> Result<usize, crate::PluginError> {
        self.inner
            .prune_non_fired_occurrences(cutoff_epoch_ms)
            .await
    }
}

/// `list_delivery_process_ids` is the narrow worklist read, and it had no
/// conformance use at all -- zero occurrences across the whole suite.
///
/// It exists so retention reconciliation can walk delivery process ids without
/// materializing occurrence or subscription JSON, which means it is a second
/// projection of the same rows `list_deliveries` returns. Two projections of
/// one table drift silently: nothing else compares them, so a backend could
/// answer the narrow query from a stale index and only the reconciler would
/// notice, long after.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn the_narrow_delivery_worklist_agrees_with_the_delivery_table(
    handles: ProcessTriggerRetentionHandles,
) {
    const SESSION: &str = "delivery-worklist-session";
    for (index, (key, source, operation, occurrence)) in [
        (
            "delivery-worklist-key-a",
            "delivery-worklist-source-a",
            "delivery-worklist-register-a",
            "delivery-worklist-occurrence-a",
        ),
        (
            "delivery-worklist-key-b",
            "delivery-worklist-source-b",
            "delivery-worklist-register-b",
            "delivery-worklist-occurrence-b",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        register_trigger(
            &handles.triggers,
            &SessionId::from(SESSION),
            key,
            source,
            operation,
        )
        .await;
        let ingress = handles
            .triggers
            .ingest_occurrence(crate::TriggerOccurrenceRequest::new(
                "ui.button.pressed",
                source,
                serde_json::json!({ "index": index }),
                occurrence,
            ))
            .await
            .expect("ingest occurrence");
        assert_eq!(ingress.reservations.len(), 1);
        start_and_bind_delivery(&handles, &ingress.reservations[0]).await;
    }

    let mut from_table = handles
        .triggers
        .list_deliveries()
        .await
        .expect("list deliveries")
        .into_iter()
        .filter_map(|delivery| delivery.process_id)
        .collect::<Vec<_>>();
    from_table.sort();
    from_table.dedup();
    let mut from_worklist = handles
        .triggers
        .list_delivery_process_ids()
        .await
        .expect("list delivery process ids");
    from_worklist.sort();
    from_worklist.dedup();

    assert_eq!(
        from_worklist, from_table,
        "the narrow delivery worklist and the delivery table must name the \
         same distinct process ids"
    );
    assert_eq!(
        from_table.len(),
        2,
        "the law is vacuous unless both bound reservations reached the delivery table"
    );
}
