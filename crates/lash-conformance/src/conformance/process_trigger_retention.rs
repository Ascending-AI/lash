//! Cross-backend conformance for process retention's trigger-store effects.

use crate::conformance::DeploymentViewExt as _;
use lash_sansio::SessionId;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

mod redelivery;

use crate::{
    ProcessAwaitOutput, ProcessCompletionAuthority, ProcessIdentity, ProcessInput,
    ProcessOriginator, ProcessRegistry, ProjectionWatermark, SessionScope, TriggerCommand,
    TriggerCommandOutcome, TriggerOwnerScope, TriggerStore, TriggerSubscriptionDraft,
};

/// Fresh paired process and trigger stores for retention conformance.
pub struct ProcessTriggerRetentionHandles {
    /// The store set the other handles are taken from: an occurrence starts
    /// through its durable store.
    pub stores: Arc<dyn crate::StoreSet>,
    pub registry: Arc<dyn ProcessRegistry>,
    pub triggers: Arc<dyn TriggerStore>,
    pub sessions: Arc<dyn crate::DeploymentStore>,
    /// The environments a delivery's process names (ADR 0113 §3.3).
    pub process_env: Arc<dyn crate::ProcessExecutionEnvStore>,
}

impl ProcessTriggerRetentionHandles {
    /// Record `request`'s occurrence as a trigger router's start does, each
    /// delivery bound to a fixture process.
    async fn record_occurrence(
        &self,
        request: crate::TriggerOccurrenceRequest,
    ) -> Result<crate::TriggerIngressReceipt, crate::PluginError> {
        lash_core::testing::record_trigger_occurrence(
            self.triggers.as_ref(),
            self.registry.as_ref(),
            self.stores.durable_store().as_ref(),
            request,
        )
        .await
    }
}

pub async fn trigger_capture_route_and_compaction_refusal_matrix<F, Fut>(make: F)
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
    mutation_receipts_follow_the_host_retention_bound(make().await).await;
    process_prune_preserves_trigger_mutation_receipts(make().await).await;
    zero_match_occurrence_is_reclaimed_at_delivery_reconciliation(make().await).await;
    delivery_delete_is_bound_to_observed_row_identity(make().await).await;
    process_prune_only_deletes_deliveries_for_pruned_processes(make().await).await;
    pruned_delivery_process_is_not_a_recovery_candidate(make().await).await;
    the_narrow_delivery_worklist_agrees_with_the_delivery_table(make().await).await;
    outstanding_delivery_blocks_interleaved_tombstone_compaction(make().await).await;
}

/// A redelivered emission on a host that journals nothing never writes a
/// reclaimed occurrence or delivery back (FIG-4513): matched, zero-match and
/// audit occurrences, each reclaimed by its own retention path. A reclaim
/// pass at any cutoff inside the redelivery horizon leaves the refusal
/// standing (FIG-4573).
pub async fn trigger_occurrence_redelivery_after_reclaim<F, Fut>(make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ProcessTriggerRetentionHandles>,
{
    redelivery::a_redelivered_emission_writes_no_reclaimed_delivery_back(make().await).await;
    redelivery::a_redelivered_zero_match_emission_writes_no_reclaimed_occurrence_back(make().await)
        .await;
    redelivery::a_redelivered_audit_emission_writes_no_pruned_occurrence_back(make().await).await;
}

/// A reclaimed occurrence's tombstone survives every reclaim cutoff,
/// `u64::MAX` included, at any age (FIG-4610).
/// `make` opens a trigger store on the clock it is given.
pub async fn trigger_occurrence_tombstones_survive_every_reclaim<F, Fut>(make: F)
where
    F: Fn(Arc<dyn crate::Clock>) -> Fut,
    Fut: Future<Output = crate::TriggerStores>,
{
    redelivery::tombstones_survive_every_reclaim(make).await;
}

/// Forget deletes exactly the tombstones written before the host's cutoff.
pub async fn trigger_tombstone_forget_has_an_exclusive_write_time_cutoff<F, Fut>(make: F)
where
    F: Fn(Arc<dyn crate::Clock>) -> Fut,
    Fut: Future<Output = crate::TriggerStores>,
{
    redelivery::forgetting_selects_exactly_the_tombstones_written_before_the_cutoff(make).await;
}

/// A forgotten identity starts again; a retained identity still refuses.
pub async fn trigger_redelivery_after_forget_starts_again<F, Fut>(make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ProcessTriggerRetentionHandles>,
{
    redelivery::a_forgotten_redelivery_starts_again_while_a_retained_one_is_refused(make().await)
        .await;
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
                expected_revision: created.record.revision,
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

    // Mutation receipts no longer ride the session-delete reconcile
    // (FIG-4108): the host retention lever reclaims them by bound, so until it
    // runs the operation ids still replay their journaled answers and a
    // reused id carrying different content is a conflict, not a
    // re-evaluation.
    let resent = handles
        .triggers
        .execute_command(
            REGISTER_OPERATION,
            TriggerCommand::Register {
                owner_scope: owner(&SessionId::from(SESSION)),
                actor: actor(&SessionId::from(SESSION)),
                draft: original_draft,
            },
        )
        .await
        .expect("resend operation id while its receipt is retained")
        .expect("resend replays the retained receipt");
    let TriggerCommandOutcome::Mutation { receipt: resent } = resent else {
        panic!("resend must return a mutation receipt")
    };
    assert_eq!(resent, created);
    let reused = handles
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
        .expect("reuse receipt-only operation id while its receipt is retained");
    assert!(
        matches!(reused, Err(crate::TriggerOperationError::Conflict { .. })),
        "a retained receipt still fences a reused operation id: {reused:?}"
    );
}

/// FIG-4108 / ADR 0023: `reclaim_retained_evidence` is the one host retention
/// lever, and trigger mutation receipts are evidence it owns. Host- and
/// platform-owned receipts older than the bound are reclaimed; a
/// session-owned receipt is reclaimed once its owner is durably deleted and
/// it is older than the bound — unless a delivery still names the owner. A
/// receipt exactly at the bound — and every live owner's receipt — survives,
/// and a resend whose receipt was reclaimed is evaluated again rather than
/// replayed.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn mutation_receipts_follow_the_host_retention_bound(
    handles: ProcessTriggerRetentionHandles,
) {
    const DELETED_SESSION: &str = "receipt-lever-deleted-session";
    const BLOCKED_SESSION: &str = "receipt-lever-blocked-session";
    const LIVE_SESSION: &str = "receipt-lever-live-session";
    const REGISTER_OPERATION: &str = "receipt-lever-register";
    const BLOCKED_OPERATION: &str = "receipt-lever-register-blocked";
    const LIVE_OPERATION: &str = "receipt-lever-register-live";

    for session in [DELETED_SESSION, BLOCKED_SESSION, LIVE_SESSION] {
        let request = super::session_store_factory::session_store_request(
            &SessionId::from(session),
            "receipt-lever-model",
            crate::SessionRelation::Root,
        );
        handles
            .sessions
            .admit_view(&request)
            .await
            .expect("materialize trigger owner session");
    }

    let command = TriggerCommand::Register {
        owner_scope: owner(&SessionId::from(DELETED_SESSION)),
        actor: actor(&SessionId::from(DELETED_SESSION)),
        draft: draft(
            &SessionId::from(DELETED_SESSION),
            "receipt-lever-key",
            "receipt-lever-source",
        ),
    };
    let created = handles
        .triggers
        .execute_command(REGISTER_OPERATION, command.clone())
        .await
        .expect("register the deleted session's trigger")
        .expect("the registration commits");
    let TriggerCommandOutcome::Mutation { receipt: created } = created else {
        panic!("register must return a mutation receipt")
    };
    let written_at_ms = created.record.created_at_ms;

    let live_command = TriggerCommand::Register {
        owner_scope: owner(&SessionId::from(LIVE_SESSION)),
        actor: actor(&SessionId::from(LIVE_SESSION)),
        draft: draft(
            &SessionId::from(LIVE_SESSION),
            "receipt-lever-live-key",
            "receipt-lever-live-source",
        ),
    };
    let created_live = handles
        .triggers
        .execute_command(LIVE_OPERATION, live_command.clone())
        .await
        .expect("register the live session's trigger")
        .expect("the live registration commits");
    let TriggerCommandOutcome::Mutation {
        receipt: created_live,
    } = created_live
    else {
        panic!("register must return a mutation receipt")
    };

    // The blocked session's subscription captures an occurrence into an
    // outstanding delivery: while that row stands, the owner's receipts are
    // not the sweep's to take.
    let blocked_command = TriggerCommand::Register {
        owner_scope: owner(&SessionId::from(BLOCKED_SESSION)),
        actor: actor(&SessionId::from(BLOCKED_SESSION)),
        draft: draft(
            &SessionId::from(BLOCKED_SESSION),
            "receipt-lever-blocked-key",
            "receipt-lever-blocked-source",
        ),
    };
    let created_blocked = handles
        .triggers
        .execute_command(BLOCKED_OPERATION, blocked_command.clone())
        .await
        .expect("register the blocked session's trigger")
        .expect("the blocked registration commits");
    let TriggerCommandOutcome::Mutation {
        receipt: created_blocked,
    } = created_blocked
    else {
        panic!("register must return a mutation receipt")
    };
    let reserved = handles
        .record_occurrence(crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            "receipt-lever-blocked-source",
            serde_json::json!({ "button": "Blue" }),
            "receipt-lever-blocked-occurrence",
        ))
        .await
        .expect("capture a delivery for the blocked owner");
    assert_eq!(reserved.reservations.len(), 1);

    // Host- and platform-scoped commands journal the ownerless receipts the
    // same lever covers.
    for (operation_id, owner_scope, command_actor) in [
        (
            "receipt-lever-host",
            crate::TriggerOwnerScope::host("receipt-lever-binding").expect("host scope"),
            crate::ProcessOriginator::host_scoped("receipt-lever-binding"),
        ),
        (
            "receipt-lever-platform",
            crate::TriggerOwnerScope::Platform,
            crate::ProcessOriginator::host(),
        ),
    ] {
        handles
            .triggers
            .execute_command(
                operation_id,
                TriggerCommand::Prune {
                    owner_scope,
                    actor: command_actor,
                    subscription_keys: Vec::new(),
                },
            )
            .await
            .expect("journal an ownerless trigger command")
            .expect("the ownerless command commits");
    }

    for session in [DELETED_SESSION, BLOCKED_SESSION] {
        handles
            .sessions
            .delete_session(&SessionId::from(session))
            .await
            .expect("delete the receipt's owner session");
    }

    // The bound is exclusive: a sweep at the receipt's own write time takes
    // nothing, and the operation still replays its recorded answer.
    let at_bound = handles
        .sessions
        .reclaim_retained_evidence(crate::RetentionBound {
            committed_before_epoch_ms: written_at_ms,
            turn_watermark: crate::TurnProjectionWatermark::NoProjector,
        })
        .await
        .expect("sweep at the receipt's write time");
    assert_eq!(
        at_bound.removed_trigger_mutation_receipt_count, 0,
        "the bound is exclusive"
    );
    assert_eq!(
        handles
            .triggers
            .execute_command(REGISTER_OPERATION, command.clone())
            .await
            .expect("replay after the at-bound sweep")
            .expect("the replay returns its journaled result"),
        TriggerCommandOutcome::Mutation {
            receipt: created.clone()
        },
        "a receipt exactly at the bound still replays"
    );

    // Past the bound the lever reclaims every receipt of a dead or ownerless
    // scope, so the resend is evaluated again rather than replayed.
    let report = handles
        .sessions
        .reclaim_retained_evidence(crate::RetentionBound {
            committed_before_epoch_ms: u64::MAX,
            turn_watermark: crate::TurnProjectionWatermark::NoProjector,
        })
        .await
        .expect("sweep past the bound");
    assert_eq!(report.removed_receipt_count, 0, "no turn receipts existed");
    assert_eq!(
        report.removed_trigger_mutation_receipt_count, 3,
        "the deleted owner's receipt and both ownerless receipts go; \
         the blocked and live owners' receipts stay"
    );
    let resent = handles
        .triggers
        .execute_command(REGISTER_OPERATION, command.clone())
        .await
        .expect("resend after reclamation")
        .expect("the resend commits");
    let TriggerCommandOutcome::Mutation { receipt: resent } = resent else {
        panic!("a resend must return a mutation receipt")
    };
    assert_eq!(
        resent.disposition,
        crate::TriggerMutationOutcome::Unchanged,
        "the reclaimed receipt's resend re-evaluates against the live subscription"
    );
    assert_ne!(resent, created, "the resend journaled a fresh receipt");

    // A third send replays the receipt the resend journaled.
    assert_eq!(
        handles
            .triggers
            .execute_command(REGISTER_OPERATION, command)
            .await
            .expect("replay the resend")
            .expect("the resend's receipt replays"),
        TriggerCommandOutcome::Mutation { receipt: resent },
        "a resend after reclamation is idempotent on its new receipt"
    );

    // The delivery-blocked owner's receipt survives the sweep and still
    // replays its journaled answer.
    assert_eq!(
        handles
            .triggers
            .execute_command(BLOCKED_OPERATION, blocked_command)
            .await
            .expect("replay the blocked owner's command")
            .expect("the blocked replay returns its journaled result"),
        TriggerCommandOutcome::Mutation {
            receipt: created_blocked
        },
        "an outstanding delivery keeps its owner's receipt"
    );

    // The live owner's receipt is untouched by the sweep and still replays.
    assert_eq!(
        handles
            .triggers
            .execute_command(LIVE_OPERATION, live_command)
            .await
            .expect("replay the live owner's command")
            .expect("the live replay returns its journaled result"),
        TriggerCommandOutcome::Mutation {
            receipt: created_live
        },
        "a live owner's receipt survives the host sweep"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn zero_match_occurrence_is_reclaimed_at_delivery_reconciliation(
    handles: ProcessTriggerRetentionHandles,
) {
    let ingress = handles
        .record_occurrence(crate::TriggerOccurrenceRequest::new(
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
            .record_occurrence(crate::TriggerOccurrenceRequest::new(
                "ui.button.pressed",
                "delivery-retention-identity-source",
                serde_json::json!({ "button": "Blue" }),
                format!("delivery-retention-identity-{occurrence}"),
            ))
            .await
            .expect("ingest identity-law occurrence");
        assert_eq!(ingress.reservations.len(), 1);
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
        .record_occurrence(crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            "process-compact-interleave-source",
            serde_json::json!({ "button": "Blue" }),
            "process-compact-interleave-occurrence",
        ))
        .await
        .expect("ingest occurrence");
    assert_eq!(ingress.reservations.len(), 1);
    let process_id = ingress.reservations[0].process_id.clone();
    handles
        .registry
        .complete_process(
            &process_id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!("done"),
            )),
            ProcessCompletionAuthority::workflow_key(&process_id),
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

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
fn draft(session_id: &SessionId, key: &str, source_key: &str) -> TriggerSubscriptionDraft {
    let mut input_template = BTreeMap::new();
    input_template.insert("event".to_string(), crate::TriggerInputBinding::Event);
    TriggerSubscriptionDraft {
        source_capture: crate::TriggerSourceCapture::provider(
            ["ui", "button"],
            crate::JsonSchema::any(),
            "ui-provider",
            serde_json::json!({"account": "a"}),
        ),
        subscription_key: key.to_string(),
        env_ref: crate::ProcessExecutionEnvRef::new(format!("process-env:fixture-{session_id}")),
        wake_target: Some(SessionScope::new(session_id)),
        name: Some("worker".to_string()),
        source_type: "ui.button.pressed".to_string(),
        source_key: source_key.to_string(),
        source: serde_json::json!({ "button": "Blue" }),
        payload_schema: crate::JsonSchema::admit(serde_json::json!({
            "type": "object",
            "properties": { "button": { "type": "string" } },
            "required": ["button"],
            "additionalProperties": false
        }))
        .expect("valid declared payload schema"),
        target: ProcessInput::Engine {
            kind: "test".to_string(),
            payload: serde_json::json!({ "process": "worker" }),
        }
        .into(),
        target_identity: ProcessIdentity::labelled("test", Some("worker".to_string())),
        event_types: Vec::new(),
        input_template,
        target_label: Some("worker".to_string()),
    }
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
        .record_occurrence(crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            "process-prune-scope-source",
            serde_json::json!({ "button": "Blue" }),
            "process-prune-scope-first",
        ))
        .await
        .expect("ingest first occurrence");
    let second = handles
        .record_occurrence(crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            "process-prune-scope-source",
            serde_json::json!({ "button": "Blue" }),
            "process-prune-scope-second",
        ))
        .await
        .expect("ingest second occurrence");
    assert_eq!(first.reservations.len(), 1);
    assert_eq!(second.reservations.len(), 1);
    let pruned_id = first.reservations[0].process_id.clone();
    let live_id = second.reservations[0].process_id.clone();

    handles
        .registry
        .complete_process(
            &pruned_id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!("done"),
            )),
            ProcessCompletionAuthority::workflow_key(&pruned_id),
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
        .record_occurrence(crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            "process-prune-tombstone-source",
            serde_json::json!({ "button": "Blue" }),
            "process-prune-tombstone-occurrence",
        ))
        .await
        .expect("ingest occurrence");
    assert_eq!(ingress.reservations.len(), 1);
    let process_id = ingress.reservations[0].process_id.clone();
    handles
        .registry
        .complete_process(
            &process_id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!("done"),
            )),
            ProcessCompletionAuthority::workflow_key(&process_id),
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

/// The `test` engine the law's subscription targets: it names no artifacts,
/// so a start stages only its environment.
struct TriggerTargetEngine;

#[async_trait::async_trait]
impl crate::ProcessEngine for TriggerTargetEngine {
    fn kind(&self) -> &'static str {
        "test"
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

    fn state_format(&self) -> crate::EngineStateFormat {
        crate::EngineStateFormat {
            kind: self.kind().to_owned(),
            version: 0,
        }
    }

    fn cancel_grace(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }

    fn program_identity(
        &self,
        _payload: &serde_json::Value,
    ) -> Option<crate::ExecutableGeneration> {
        None
    }

    fn creation_config(
        &self,
        _env_spec: &crate::ProcessExecutionEnvSpec,
    ) -> Result<Option<serde_json::Value>, crate::PluginError> {
        Ok(None)
    }

    fn advance(
        &self,
        state: crate::EngineState,
        _event: crate::EngineEvent,
    ) -> Result<(crate::EngineState, crate::EngineAction), crate::ProcessInfraError> {
        Ok((
            state,
            crate::EngineAction::Terminal(crate::ProcessAwaitOutput::from_tool_output(
                crate::ToolCallOutput::success(serde_json::Value::Null),
            )),
        ))
    }

    async fn resolve(
        &self,
        _reference: &crate::ProcessDefinitionRef,
    ) -> Result<crate::ProcessDefinitionResolution, crate::ProcessDefinitionRefusal> {
        Ok(crate::ProcessDefinitionResolution::new(
            crate::ProcessSignature::Unknown,
            Vec::new(),
        ))
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
            .record_occurrence(crate::TriggerOccurrenceRequest::new(
                "ui.button.pressed",
                source,
                serde_json::json!({ "index": index }),
                occurrence,
            ))
            .await
            .expect("ingest occurrence");
        assert_eq!(ingress.reservations.len(), 1);
    }

    let mut from_table = handles
        .triggers
        .list_deliveries()
        .await
        .expect("list deliveries")
        .into_iter()
        .map(|delivery| delivery.process_id)
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
