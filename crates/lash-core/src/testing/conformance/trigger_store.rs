//! [`TriggerStore`](crate::TriggerStore) conformance for keyed, revisioned
//! subscriptions and atomic occurrence reservation.

use super::*;

pub async fn trigger_store<F>(make: F)
where
    F: Fn() -> Arc<dyn crate::TriggerStore>,
{
    let first = make();
    let second = make();
    assert_fresh_instances(&first, &second, "trigger_store");
    drop((first, second));
    trigger_source_key_and_subscription_identity_are_stable();
    same_owner_key_definition_is_idempotent(make()).await;
    changed_register_conflicts_and_update_is_cas(make()).await;
    committed_mutation_receipt_survives_later_revision(make()).await;
    conflicting_mutation_receipt_survives_later_revision(make()).await;
    list_operations_are_not_receipted(make()).await;
    mutation_receipts_follow_owner_retention(make()).await;
    reservations_execute_the_reserved_revision(make()).await;
    disable_preserves_reserved_work_and_requires_explicit_enable(make()).await;
    register_disable_reenable_roundtrip_is_fenced_and_receipted(make()).await;
    delete_tombstones_preserves_history_and_revive_changes_incarnation(make()).await;
    owner_namespaces_are_exact_and_session_cleanup_is_scoped(make()).await;
    explicit_prune_is_journaled_and_owner_scoped(make()).await;
    occurrence_and_reservations_are_atomic_and_idempotent(make()).await;
    occurrence_time_bounds_match_the_rust_predicate(make()).await;
    session_tombstone_and_receipts_follow_deleted_owner_and_last_delivery(make()).await;
    host_tombstone_remains_a_permanent_revive_fence(make()).await;
    zero_match_occurrence_reconciles_without_deliveries(make()).await;
    occurrence_with_live_delivery_survives_reconciliation(make()).await;
    zero_match_occurrence_is_immediately_reclaimable(make()).await;
    matched_occurrence_waits_for_terminal_deliveries(make()).await;
    cutoff_defers_but_never_initiates_occurrence_reclaim(make()).await;
    null_source_occurrence_replay_is_idempotent(make()).await;
    first_ingress_and_replay_share_canonical_subscription_order(make()).await;
}

/// Arms a backend failure on the delete of one exact occurrence.
#[async_trait::async_trait]
pub trait TriggerOccurrenceRetentionFaultInjector: Send + Sync {
    async fn fail_occurrence_delete(&self, occurrence_id: &str);
    async fn clear_occurrence_delete_failure(&self);
}

/// Raw receipt access for conformance-suite embedders proving compatibility
/// with ownerless receipts written before owner namespaces were journaled.
#[async_trait::async_trait]
pub trait LegacyTriggerMutationReceiptInjector: Send + Sync {
    async fn insert_legacy_receipt(
        &self,
        operation_id: &str,
        request_fingerprint: &str,
        result_json: &str,
        created_at_ms: u64,
    );

    async fn receipt_exists(&self, operation_id: &str) -> bool;
}

/// Proves for conformance-suite embedders that an old receipt whose JSON names
/// no owner survives both deleted-session reconciliation and the host cutoff.
pub async fn legacy_ownerless_trigger_receipt_is_retained_law(
    store: Arc<dyn crate::TriggerStore>,
    injector: &dyn LegacyTriggerMutationReceiptInjector,
) {
    const SESSION_ID: &str = "legacy-ownerless-receipt-session";
    const PUBLIC_OPERATION_ID: &str = "legacy-ownerless-empty-prune";
    const REQUEST_FINGERPRINT: &str = "legacy-ownerless-request-fingerprint";
    const OLD_RESULT_JSON: &str = r#"{"Ok":{"type":"prune","receipts":[]}}"#;

    let old_result: crate::TriggerEffectResult = Ok(crate::TriggerCommandOutcome::Prune {
        receipts: Vec::new(),
    });
    assert_eq!(
        serde_json::to_string(&old_result).expect("encode old-form trigger receipt"),
        OLD_RESULT_JSON,
        "the fixture must remain byte-for-byte what the old store code wrote"
    );

    let receipt_id = crate::trigger_operation_receipt_id(
        &crate::TriggerOwnerScope::session(SESSION_ID),
        PUBLIC_OPERATION_ID,
    );
    injector
        .insert_legacy_receipt(&receipt_id, REQUEST_FINGERPRINT, OLD_RESULT_JSON, 0)
        .await;

    let report = store
        .reconcile_trigger_retention(&[], &[SESSION_ID.to_string()])
        .await
        .expect("reconcile around an ownerless legacy receipt");
    assert_eq!(
        report.reclaimed_mutation_receipt_count, 0,
        "the deleted-session cascade cannot classify an ownerless legacy receipt"
    );
    assert!(
        injector.receipt_exists(&receipt_id).await,
        "the ownerless legacy receipt must survive the deleted-session cascade"
    );

    assert_eq!(
        store.prune_mutation_receipts(u64::MAX).await.unwrap(),
        0,
        "the host cutoff must retain a receipt whose owner cannot be determined"
    );
    assert!(
        injector.receipt_exists(&receipt_id).await,
        "the ownerless legacy receipt must survive the host cutoff"
    );
}

/// Proves that a mid-pass host-lever delete failure is `Err` with completed
/// work in its partial report, never a forged `NothingToDo` success.
pub async fn trigger_occurrence_retention_failure_law(
    store: Arc<dyn crate::TriggerStore>,
    fault: &dyn TriggerOccurrenceRetentionFaultInjector,
) {
    for key in ["failure-a", "failure-b"] {
        store
            .ingest_occurrence(button_occurrence("no-matching-subscription", key))
            .await
            .expect("ingest zero-match occurrence for failure law");
    }
    let mut occurrence_ids = store
        .list_occurrences(crate::TriggerOccurrenceFilter::default())
        .await
        .expect("enumerate failure-law occurrences")
        .into_iter()
        .map(|occurrence| occurrence.occurrence_id)
        .collect::<Vec<_>>();
    occurrence_ids.sort();
    fault.fail_occurrence_delete(&occurrence_ids[1]).await;

    let failure = store
        .reclaim_trigger_occurrences(u64::MAX)
        .await
        .expect_err("the injected delete must fail the reclaim pass");
    assert!(
        matches!(failure.stop, crate::MaintenanceStop::Failed(_)),
        "a backend delete failure must use the failed arm: {failure:?}"
    );
    assert_eq!(failure.partial.inspected_occurrence_count, 2);
    assert_eq!(
        failure.partial.reclaimed_occurrence_count, 1,
        "the first committed delete must survive in the partial report"
    );
    assert_eq!(
        crate::MaintenanceReport::sweep(&failure.partial),
        crate::MaintenanceSweep::Swept,
        "the partial report names completed work; it is never forged as NothingToDo"
    );
    let remaining = store
        .list_occurrences(crate::TriggerOccurrenceFilter::default())
        .await
        .expect("inspect state after injected failure");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].occurrence_id, occurrence_ids[1]);
}

/// Proves that reconciliation delete failure rolls the whole trigger-retention
/// transaction back and that the same decision succeeds when retried.
pub async fn trigger_retention_reconciliation_failure_law(
    store: Arc<dyn crate::TriggerStore>,
    fault: &dyn TriggerOccurrenceRetentionFaultInjector,
) {
    for key in ["transaction-failure-a", "transaction-failure-b"] {
        store
            .ingest_occurrence(button_occurrence("no-matching-subscription", key))
            .await
            .expect("ingest zero-match occurrence for transaction failure law");
    }
    let mut occurrence_ids = store
        .list_occurrences(crate::TriggerOccurrenceFilter::default())
        .await
        .expect("enumerate transaction failure-law occurrences")
        .into_iter()
        .map(|occurrence| occurrence.occurrence_id)
        .collect::<Vec<_>>();
    occurrence_ids.sort();
    fault.fail_occurrence_delete(&occurrence_ids[1]).await;

    store
        .reconcile_trigger_retention(&[], &[])
        .await
        .expect_err("the injected delete must fail reconciliation");
    let remaining = store
        .list_occurrences(crate::TriggerOccurrenceFilter::default())
        .await
        .expect("inspect state after injected reconciliation failure");
    assert_eq!(
        remaining.len(),
        2,
        "the injected failure must roll every occurrence delete back"
    );

    fault.clear_occurrence_delete_failure().await;
    let retried = store
        .reconcile_trigger_retention(&[], &[])
        .await
        .expect("retry occurrence retention after one-shot fault");
    assert_eq!(retried.reclaimed_occurrence_count, 2);
    assert!(
        store
            .list_occurrences(crate::TriggerOccurrenceFilter::default())
            .await
            .expect("inspect state after successful retry")
            .is_empty()
    );
}

async fn session_tombstone_and_receipts_follow_deleted_owner_and_last_delivery(
    store: Arc<dyn crate::TriggerStore>,
) {
    const SESSION: &str = "dead-owner-retention-session";
    const KEY: &str = "dead-owner-retention-key";
    const ACTIVE_KEY: &str = "dead-owner-retention-active-key";
    const REGISTER_OPERATION: &str = "dead-owner-retention-register";
    let draft = sample_draft(
        SESSION,
        KEY,
        "dead-owner-retention-source",
        "dead-owner-retention-worker",
    );
    let created = mutate(
        &store,
        REGISTER_OPERATION,
        register_command(SESSION, draft.clone()),
    )
    .await;
    let ingress = store
        .ingest_occurrence(button_occurrence(
            "dead-owner-retention-source",
            "dead-owner-retention-occurrence",
        ))
        .await
        .expect("ingest dead-owner retention occurrence");
    assert_eq!(ingress.reservations.len(), 1);
    let deleted = mutate(
        &store,
        "dead-owner-retention-delete",
        revision_command(SESSION, KEY, created.revision, "delete"),
    )
    .await;
    assert!(deleted.record_snapshot.tombstoned);

    let blocked = store
        .reconcile_trigger_retention(&[], &[SESSION.to_string()])
        .await
        .expect("reconcile while dead owner's delivery remains");
    assert_eq!(
        blocked,
        crate::TriggerRetentionReconciliationReport::default(),
        "the live delivery must retain its occurrence, tombstone, and receipts"
    );
    assert_eq!(
        mutate(
            &store,
            REGISTER_OPERATION,
            register_command(SESSION, draft.clone()),
        )
        .await,
        created,
        "the registration receipt must replay while a delivery remains"
    );
    assert!(
        execute(
            &store,
            "dead-owner-retention-register-probe",
            register_command(SESSION, draft.clone()),
        )
        .await
        .is_err(),
        "the tombstone remains the Revive fence while a delivery references it"
    );
    mutate(
        &store,
        "dead-owner-retention-active-register",
        register_command(
            SESSION,
            sample_draft(
                SESSION,
                ACTIVE_KEY,
                "dead-owner-retention-active-source",
                "dead-owner-retention-active-worker",
            ),
        ),
    )
    .await;

    let reservation = &ingress.reservations[0];
    let report = store
        .reconcile_trigger_retention(
            &[crate::TriggerDeliveryRetentionCandidate {
                occurrence_id: ingress.occurrence.occurrence_id,
                subscription_id: reservation.subscription.subscription_id.clone(),
                process_id: reservation.process_id.clone(),
            }],
            &[SESSION.to_string()],
        )
        .await
        .expect("reconcile dead owner's final delivery");
    assert_eq!(report.reclaimed_delivery_count, 1);
    assert_eq!(report.reclaimed_occurrence_count, 1);
    assert_eq!(
        report.reclaimed_subscription_count, 2,
        "the deleted-session cascade covers enabled and tombstoned subscriptions"
    );
    assert_eq!(report.reclaimed_mutation_receipt_count, 4);

    let mut replacement = draft;
    replacement.source_key = "dead-owner-retention-replacement".to_string();
    let recreated = mutate(
        &store,
        REGISTER_OPERATION,
        register_command(SESSION, replacement),
    )
    .await;
    assert_eq!(recreated.revision, 1, "the old receipt was reclaimed");
}

async fn host_tombstone_remains_a_permanent_revive_fence(store: Arc<dyn crate::TriggerStore>) {
    let owner_scope = crate::TriggerOwnerScope::host("retention-host").unwrap();
    let mut draft = sample_draft(
        "unused-host-session",
        "host-retention-key",
        "host-retention-source",
        "host-retention-worker",
    );
    draft.wake_target = None;
    let actor = crate::ProcessOriginator::host_scoped("retention-host");
    let created = mutate(
        &store,
        "host-retention-register",
        crate::TriggerCommand::Register {
            owner_scope: owner_scope.clone(),
            actor: actor.clone(),
            draft: draft.clone(),
        },
    )
    .await;
    let deleted = mutate(
        &store,
        "host-retention-delete",
        crate::TriggerCommand::Delete {
            owner_scope: owner_scope.clone(),
            actor: actor.clone(),
            subscription_key: draft.subscription_key.clone(),
            expected_revision: created.revision,
        },
    )
    .await;
    store
        .ingest_occurrence(button_occurrence(
            "host-zero-match-source",
            "host-zero-match-occurrence",
        ))
        .await
        .expect("ingest host-law zero-match occurrence");

    let report = store
        .reconcile_trigger_retention(&[], &["unrelated-deleted-session".to_string()])
        .await
        .expect("reconcile around host tombstone");
    assert_eq!(report.reclaimed_occurrence_count, 1);
    assert_eq!(report.reclaimed_subscription_count, 0);
    assert_eq!(report.reclaimed_mutation_receipt_count, 0);

    let revived = mutate(
        &store,
        "host-retention-revive",
        crate::TriggerCommand::Revive {
            owner_scope,
            actor,
            subscription_key: draft.subscription_key.clone(),
            draft,
            expected_revision: deleted.revision,
        },
    )
    .await;
    assert_eq!(revived.revision, 3);
}

async fn zero_match_occurrence_reconciles_without_deliveries(store: Arc<dyn crate::TriggerStore>) {
    store
        .ingest_occurrence(button_occurrence(
            "reconciliation-zero-match-source",
            "reconciliation-zero-match-occurrence",
        ))
        .await
        .expect("ingest reconciliation zero-match occurrence");
    let report = store
        .reconcile_trigger_retention(&[], &[])
        .await
        .expect("reconcile zero-match occurrence");
    assert_eq!(report.reclaimed_occurrence_count, 1);
    assert!(
        store
            .list_occurrences(crate::TriggerOccurrenceFilter::default())
            .await
            .expect("list after zero-match reconciliation")
            .is_empty()
    );
}

async fn occurrence_with_live_delivery_survives_reconciliation(
    store: Arc<dyn crate::TriggerStore>,
) {
    store
        .ingest_occurrence(button_occurrence(
            "live-reconciliation-zero-control",
            "live-reconciliation-zero-control-occurrence",
        ))
        .await
        .expect("ingest zero-match control occurrence");
    mutate(
        &store,
        "live-reconciliation-register",
        register_command(
            "live-reconciliation-session",
            sample_draft(
                "live-reconciliation-session",
                "live-reconciliation-key",
                "live-reconciliation-source",
                "live-reconciliation-worker",
            ),
        ),
    )
    .await;
    let ingress = store
        .ingest_occurrence(button_occurrence(
            "live-reconciliation-source",
            "live-reconciliation-occurrence",
        ))
        .await
        .expect("ingest live-fan-out occurrence");
    assert_eq!(ingress.reservations.len(), 1);

    let report = store
        .reconcile_trigger_retention(&[], &[])
        .await
        .expect("reconcile with a live delivery");
    assert_eq!(
        report.reclaimed_occurrence_count, 1,
        "the same pass must reclaim its zero-match control"
    );
    assert_eq!(
        store
            .list_occurrences(crate::TriggerOccurrenceFilter::default())
            .await
            .expect("list live occurrence after reconciliation"),
        vec![ingress.occurrence]
    );
}

pub async fn trigger_store_reopenable<F>(make: F)
where
    F: Fn() -> ReopenableTriggerStore,
{
    let probe = make();
    assert_fresh_instances(&probe.open, &probe.reopen, "trigger_store_reopenable");
    trigger_store(|| make().open).await;
    same_identity_and_receipt_survive_store_reopen(make()).await;
}

fn owner(session_id: &str) -> crate::TriggerOwnerScope {
    crate::TriggerOwnerScope::session(session_id)
}

fn actor(session_id: &str) -> crate::ProcessOriginator {
    crate::ProcessOriginator::session(crate::SessionScope::new(session_id))
}

fn sample_draft(
    session_id: &str,
    subscription_key: &str,
    source_key: &str,
    process_name: &str,
) -> crate::TriggerSubscriptionDraft {
    let mut inputs = BTreeMap::new();
    inputs.insert("event".to_string(), crate::TriggerInputBinding::Event);
    crate::TriggerSubscriptionDraft {
        subscription_key: subscription_key.to_string(),
        env_ref: crate::ProcessExecutionEnvRef::new(format!("process-env:{session_id}")),
        wake_target: Some(crate::SessionScope::new(session_id)),
        name: Some(process_name.to_string()),
        source_type: "ui.button.pressed".to_string(),
        source_key: source_key.to_string(),
        source: serde_json::json!({ "button": "Blue" }),
        payload_schema: crate::LashSchema::new(serde_json::json!({
            "type": "object",
            "properties": { "button": { "type": "string" } },
            "required": ["button"],
            "additionalProperties": false
        })),
        target: crate::ProcessInput::Engine {
            kind: "test".to_string(),
            payload: serde_json::json!({ "process": process_name }),
        },
        target_identity: crate::ProcessIdentity::new("test")
            .with_label(Some(process_name.to_string()))
            .with_definition(Some(serde_json::json!({ "process_name": process_name }))),
        event_types: Vec::new(),
        input_template: inputs,
        target_label: Some(process_name.to_string()),
    }
}

fn register_command(
    session_id: &str,
    draft: crate::TriggerSubscriptionDraft,
) -> crate::TriggerCommand {
    crate::TriggerCommand::Register {
        owner_scope: owner(session_id),
        actor: actor(session_id),
        draft,
    }
}

fn update_command(
    session_id: &str,
    key: &str,
    draft: crate::TriggerSubscriptionDraft,
    expected_revision: u64,
) -> crate::TriggerCommand {
    crate::TriggerCommand::Update {
        owner_scope: owner(session_id),
        actor: actor(session_id),
        subscription_key: key.to_string(),
        draft,
        expected_revision,
    }
}

fn revision_command(
    session_id: &str,
    key: &str,
    expected_revision: u64,
    verb: &str,
) -> crate::TriggerCommand {
    let owner_scope = owner(session_id);
    let actor = actor(session_id);
    let subscription_key = key.to_string();
    match verb {
        "enable" => crate::TriggerCommand::Enable {
            owner_scope,
            actor,
            subscription_key,
            expected_revision,
        },
        "disable" => crate::TriggerCommand::Disable {
            owner_scope,
            actor,
            subscription_key,
            expected_revision,
        },
        "delete" => crate::TriggerCommand::Delete {
            owner_scope,
            actor,
            subscription_key,
            expected_revision,
        },
        _ => panic!("unknown test trigger verb"),
    }
}

async fn execute(
    store: &Arc<dyn crate::TriggerStore>,
    operation_id: &str,
    command: crate::TriggerCommand,
) -> crate::TriggerEffectResult {
    store
        .execute_command(operation_id, command)
        .await
        .expect("trigger store command")
}

async fn mutate(
    store: &Arc<dyn crate::TriggerStore>,
    operation_id: &str,
    command: crate::TriggerCommand,
) -> crate::TriggerMutationReceipt {
    match execute(store, operation_id, command)
        .await
        .expect("trigger mutation outcome")
    {
        crate::TriggerCommandOutcome::Mutation { receipt } => *receipt,
        crate::TriggerCommandOutcome::List { .. } | crate::TriggerCommandOutcome::Prune { .. } => {
            panic!("expected mutation receipt")
        }
    }
}

fn button_occurrence(
    source_key: impl Into<String>,
    idempotency_key: impl Into<String>,
) -> crate::TriggerOccurrenceRequest {
    crate::TriggerOccurrenceRequest::new(
        "ui.button.pressed",
        source_key,
        serde_json::json!({ "button": "Blue" }),
        idempotency_key,
    )
    .with_source(serde_json::json!({ "button": "Blue" }))
}

fn trigger_source_key_and_subscription_identity_are_stable() {
    let source = serde_json::json!({ "button": "Blue" });
    let first = crate::default_trigger_source_key("ui.button.pressed", &source);
    let second = crate::default_trigger_source_key("ui.button.pressed", &source);
    assert_eq!(first, second);
    assert_ne!(
        crate::deterministic_subscription_id(&owner("session-a"), "ab:c"),
        crate::deterministic_subscription_id(&owner("session-a"), "a:bc")
    );
}

async fn same_owner_key_definition_is_idempotent(store: Arc<dyn crate::TriggerStore>) {
    let draft = sample_draft("session-a", "button-blue", "blue", "worker");
    let first = mutate(
        &store,
        "register-first",
        register_command("session-a", draft.clone()),
    )
    .await;
    let second = mutate(
        &store,
        "register-second",
        register_command("session-a", draft),
    )
    .await;
    assert_eq!(first.subscription_id, second.subscription_id);
    assert_eq!(first.incarnation, second.incarnation);
    assert_eq!(first.revision, 1);
    assert_eq!(second.revision, 1);
    assert_eq!(second.disposition, crate::TriggerMutationOutcome::Unchanged);
    let rows = store
        .list_subscriptions(crate::TriggerSubscriptionFilter::for_session("session-a"))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
}

async fn changed_register_conflicts_and_update_is_cas(store: Arc<dyn crate::TriggerStore>) {
    let key = "cas-key";
    let original = sample_draft("session-a", key, "v1", "worker");
    let created = mutate(
        &store,
        "cas-register",
        register_command("session-a", original),
    )
    .await;
    let requested = sample_draft("session-a", key, "v2", "worker");
    let conflict = execute(
        &store,
        "cas-register-different",
        register_command("session-a", requested.clone()),
    )
    .await
    .expect_err("register must not upsert");
    match conflict {
        crate::TriggerOperationError::Conflict {
            existing_revision,
            existing_definition_fingerprint,
            requested_definition_fingerprint,
            ..
        } => {
            assert_eq!(existing_revision, Some(1));
            assert_eq!(
                existing_definition_fingerprint,
                Some(created.definition_fingerprint)
            );
            assert!(requested_definition_fingerprint.is_some());
        }
        error => panic!("unexpected error: {error}"),
    }

    let store_a = Arc::clone(&store);
    let store_b = Arc::clone(&store);
    let left = update_command("session-a", key, requested, 1);
    let right = update_command(
        "session-a",
        key,
        sample_draft("session-a", key, "v3", "worker"),
        1,
    );
    let (left, right) = tokio::join!(
        execute(&store_a, "cas-left", left),
        execute(&store_b, "cas-right", right)
    );
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    assert_eq!(usize::from(left.is_err()) + usize::from(right.is_err()), 1);
}

async fn committed_mutation_receipt_survives_later_revision(store: Arc<dyn crate::TriggerStore>) {
    let key = "receipt-key";
    mutate(
        &store,
        "receipt-register",
        register_command("session-a", sample_draft("session-a", key, "v1", "worker")),
    )
    .await;
    let update = update_command(
        "session-a",
        key,
        sample_draft("session-a", key, "v2", "worker"),
        1,
    );
    let committed = mutate(&store, "receipt-update", update.clone()).await;
    assert_eq!(committed.revision, 2);
    mutate(
        &store,
        "receipt-disable",
        revision_command("session-a", key, 2, "disable"),
    )
    .await;
    let retried = mutate(&store, "receipt-update", update).await;
    assert_eq!(
        retried, committed,
        "retry must return the historical receipt"
    );
    let current = store
        .list_subscriptions(crate::TriggerSubscriptionFilter::for_session("session-a"))
        .await
        .unwrap();
    assert_eq!(current[0].revision, 3);
    assert!(!current[0].enabled);
}

async fn conflicting_mutation_receipt_survives_later_revision(store: Arc<dyn crate::TriggerStore>) {
    let key = "conflict-receipt-key";
    mutate(
        &store,
        "conflict-receipt-register",
        register_command("session-a", sample_draft("session-a", key, "v1", "worker")),
    )
    .await;
    let conflicting = revision_command("session-a", key, 99, "disable");
    let original = execute(&store, "conflict-receipt-disable", conflicting.clone())
        .await
        .expect_err("stale disable conflicts");
    mutate(
        &store,
        "conflict-receipt-valid-disable",
        revision_command("session-a", key, 1, "disable"),
    )
    .await;
    let retried = execute(&store, "conflict-receipt-disable", conflicting)
        .await
        .expect_err("conflicting retry remains a conflict");
    assert_eq!(retried, original, "retry returns the original conflict");
}

async fn list_operations_are_not_receipted(store: Arc<dyn crate::TriggerStore>) {
    let key = "unreceipted-list-key";
    mutate(
        &store,
        "unreceipted-list-register",
        register_command("session-a", sample_draft("session-a", key, "v1", "worker")),
    )
    .await;
    let listed_enabled = execute(
        &store,
        "reused-list-operation-id",
        crate::TriggerCommand::List {
            owner_scope: owner("session-a"),
            filter: crate::TriggerSubscriptionFilter {
                enabled: Some(true),
                ..Default::default()
            },
        },
    )
    .await
    .expect("first list");
    assert!(matches!(
        listed_enabled,
        crate::TriggerCommandOutcome::List { records } if records.len() == 1
    ));
    mutate(
        &store,
        "unreceipted-list-disable",
        revision_command("session-a", key, 1, "disable"),
    )
    .await;
    let listed_disabled = execute(
        &store,
        "reused-list-operation-id",
        crate::TriggerCommand::List {
            owner_scope: owner("session-a"),
            filter: crate::TriggerSubscriptionFilter {
                enabled: Some(false),
                ..Default::default()
            },
        },
    )
    .await
    .expect("second list");
    assert!(matches!(
        listed_disabled,
        crate::TriggerCommandOutcome::List { records } if records.len() == 1
    ));
}

async fn mutation_receipts_follow_owner_retention(store: Arc<dyn crate::TriggerStore>) {
    let key = "receipt-retention-key";
    let command = register_command("session-a", sample_draft("session-a", key, "v1", "worker"));
    let created = mutate(&store, "receipt-retention-register", command.clone()).await;
    assert_eq!(created.disposition, crate::TriggerMutationOutcome::Created);

    for (operation_id, owner_scope, actor) in [
        (
            "receipt-retention-host",
            crate::TriggerOwnerScope::host("receipt-retention-binding").unwrap(),
            crate::ProcessOriginator::host_scoped("receipt-retention-binding"),
        ),
        (
            "receipt-retention-platform",
            crate::TriggerOwnerScope::Platform,
            crate::ProcessOriginator::host(),
        ),
    ] {
        execute(
            &store,
            operation_id,
            crate::TriggerCommand::Prune {
                owner_scope,
                actor,
                subscription_keys: Vec::new(),
            },
        )
        .await
        .expect("host or platform prune is journaled");
    }

    assert_eq!(
        store.prune_mutation_receipts(u64::MAX).await.unwrap(),
        2,
        "the retention cutoff removes only aged host and platform receipts"
    );
    let replayed = mutate(&store, "receipt-retention-register", command).await;
    assert_eq!(
        replayed, created,
        "a live session's mutation receipt survives the host cutoff"
    );
}

async fn explicit_prune_is_journaled_and_owner_scoped(store: Arc<dyn crate::TriggerStore>) {
    for session_id in ["prune-owner", "prune-neighbor"] {
        mutate(
            &store,
            &format!("prune-register-{session_id}"),
            register_command(
                session_id,
                sample_draft(session_id, "shared-key", "blue", "worker"),
            ),
        )
        .await;
    }
    let command = crate::TriggerCommand::Prune {
        owner_scope: owner("prune-owner"),
        actor: actor("prune-owner"),
        subscription_keys: vec!["shared-key".to_string()],
    };
    let first = execute(&store, "explicit-prune", command.clone())
        .await
        .expect("explicit prune succeeds");
    let crate::TriggerCommandOutcome::Prune { receipts } = first else {
        panic!("prune must return typed receipts");
    };
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].owner_scope, owner("prune-owner"));
    assert_eq!(
        receipts[0].disposition,
        crate::TriggerMutationOutcome::Deleted
    );

    let replay = execute(&store, "explicit-prune", command)
        .await
        .expect("prune replay returns its journaled result");
    assert_eq!(
        replay,
        crate::TriggerCommandOutcome::Prune {
            receipts: receipts.clone(),
        }
    );
    assert!(
        store
            .list_subscriptions(crate::TriggerSubscriptionFilter::for_session("prune-owner",))
            .await
            .unwrap()
            .is_empty()
    );
    let neighbor = store
        .list_subscriptions(crate::TriggerSubscriptionFilter::for_session(
            "prune-neighbor",
        ))
        .await
        .unwrap();
    assert_eq!(neighbor.len(), 1);
    assert_eq!(neighbor[0].subscription_key, "shared-key");
}

async fn reservations_execute_the_reserved_revision(store: Arc<dyn crate::TriggerStore>) {
    let key = "snapshot-key";
    let source_key = "snapshot-v1";
    mutate(
        &store,
        "snapshot-register",
        register_command(
            "session-a",
            sample_draft("session-a", key, source_key, "worker-v1"),
        ),
    )
    .await;
    let first = store
        .ingest_occurrence(button_occurrence(source_key, "snapshot-occurrence-v1"))
        .await
        .unwrap();
    assert_eq!(first.reservations.len(), 1);
    assert_eq!(first.reservations[0].subscription.revision, 1);

    mutate(
        &store,
        "snapshot-update",
        update_command(
            "session-a",
            key,
            sample_draft("session-a", key, source_key, "worker-v2"),
            1,
        ),
    )
    .await;
    let historical = store
        .list_deliveries_by_occurrence_id(&first.occurrence.occurrence_id)
        .await
        .unwrap();
    assert_eq!(historical[0].subscription.revision, 1);
    assert_eq!(
        historical[0].subscription.target_label.as_deref(),
        Some("worker-v1")
    );

    let second = store
        .ingest_occurrence(button_occurrence(source_key, "snapshot-occurrence-v2"))
        .await
        .unwrap();
    assert_eq!(second.reservations[0].subscription.revision, 2);
    assert_eq!(
        second.reservations[0].subscription.target_label.as_deref(),
        Some("worker-v2")
    );
    assert_ne!(
        first.reservations[0].process_id,
        second.reservations[0].process_id
    );
}

async fn disable_preserves_reserved_work_and_requires_explicit_enable(
    store: Arc<dyn crate::TriggerStore>,
) {
    let key = "disable-key";
    let source_key = "disable-source";
    let draft = sample_draft("session-a", key, source_key, "worker");
    mutate(
        &store,
        "disable-register",
        register_command("session-a", draft.clone()),
    )
    .await;
    let reserved = store
        .ingest_occurrence(button_occurrence(source_key, "disable-before"))
        .await
        .unwrap();
    let disabled = mutate(
        &store,
        "disable-command",
        revision_command("session-a", key, 1, "disable"),
    )
    .await;
    assert_eq!(disabled.revision, 2);
    assert_eq!(
        store
            .list_deliveries_by_occurrence_id(&reserved.occurrence.occurrence_id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .ingest_occurrence(button_occurrence(source_key, "disable-after"))
            .await
            .unwrap()
            .reservations
            .is_empty()
    );
    let repeated = mutate(
        &store,
        "disable-reregister",
        register_command("session-a", draft),
    )
    .await;
    assert!(!repeated.enabled);
    assert_eq!(repeated.revision, 2);
    mutate(
        &store,
        "disable-enable",
        revision_command("session-a", key, 2, "enable"),
    )
    .await;
    assert_eq!(
        store
            .ingest_occurrence(button_occurrence(source_key, "disable-reenabled"))
            .await
            .unwrap()
            .reservations
            .len(),
        1
    );
}

/// Re-enable is a first-class fenced verb, not a gap hosts must work around by
/// writing the store's tables themselves. The whole
/// `register -> disable -> re-enable` roundtrip must run through
/// `execute_command`: the fence rejects a stale revision, the accepted enable
/// advances the revision and flips the record, its receipt replays byte-for-byte
/// afterwards, and occurrences reach the target again.
async fn register_disable_reenable_roundtrip_is_fenced_and_receipted(
    store: Arc<dyn crate::TriggerStore>,
) {
    let key = "reenable-roundtrip-key";
    let source_key = "reenable-roundtrip-source";
    let registered = mutate(
        &store,
        "reenable-roundtrip-register",
        register_command(
            "session-a",
            sample_draft("session-a", key, source_key, "worker"),
        ),
    )
    .await;
    assert_eq!(registered.revision, 1);
    assert!(registered.enabled);

    let disabled = mutate(
        &store,
        "reenable-roundtrip-disable",
        revision_command("session-a", key, 1, "disable"),
    )
    .await;
    assert_eq!(disabled.revision, 2);
    assert_eq!(
        disabled.disposition,
        crate::TriggerMutationOutcome::Disabled
    );
    assert!(
        store
            .ingest_occurrence(button_occurrence(source_key, "reenable-roundtrip-disabled"))
            .await
            .unwrap()
            .reservations
            .is_empty(),
        "a disabled subscription reserves nothing"
    );

    // The revision the caller read before the disable is now stale: the fence
    // must reject it instead of enabling from a superseded view.
    let stale = execute(
        &store,
        "reenable-roundtrip-stale-enable",
        revision_command("session-a", key, 1, "enable"),
    )
    .await
    .expect_err("stale enable conflicts");
    assert!(matches!(
        stale,
        crate::TriggerOperationError::Conflict {
            existing_revision: Some(2),
            ..
        }
    ));

    // The host re-reads through the same command surface, then enables at the
    // observed revision.
    let listed = execute(
        &store,
        "reenable-roundtrip-list",
        crate::TriggerCommand::List {
            owner_scope: owner("session-a"),
            filter: crate::TriggerSubscriptionFilter {
                subscription_key: Some(key.to_string()),
                ..Default::default()
            },
        },
    )
    .await
    .expect("list before enable");
    let crate::TriggerCommandOutcome::List { records } = listed else {
        panic!("expected list records");
    };
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].revision, 2);
    assert!(!records[0].enabled);

    let reenabled = mutate(
        &store,
        "reenable-roundtrip-enable",
        revision_command("session-a", key, records[0].revision, "enable"),
    )
    .await;
    assert_eq!(
        reenabled.disposition,
        crate::TriggerMutationOutcome::Enabled
    );
    assert_eq!(reenabled.revision, 3);
    assert!(reenabled.enabled);
    assert!(reenabled.record_snapshot.enabled);
    assert!(!reenabled.record_snapshot.tombstoned);
    assert_eq!(reenabled.subscription_id, registered.subscription_id);
    assert_eq!(reenabled.incarnation, registered.incarnation);
    assert_eq!(
        reenabled.definition_fingerprint,
        registered.definition_fingerprint
    );

    let live = store
        .list_subscriptions(crate::TriggerSubscriptionFilter::for_session("session-a"))
        .await
        .unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].revision, 3);
    assert!(live[0].enabled);

    // Delivery resumes for occurrences emitted after the re-enable.
    let delivered = store
        .ingest_occurrence(button_occurrence(source_key, "reenable-roundtrip-enabled"))
        .await
        .unwrap();
    assert_eq!(delivered.reservations.len(), 1);
    assert_eq!(delivered.reservations[0].subscription.revision, 3);

    // Both journaled operations replay to their original receipts, even though
    // the row has moved past them.
    let replayed_enable = mutate(
        &store,
        "reenable-roundtrip-enable",
        revision_command("session-a", key, records[0].revision, "enable"),
    )
    .await;
    assert_eq!(replayed_enable, reenabled);
    let replayed_disable = mutate(
        &store,
        "reenable-roundtrip-disable",
        revision_command("session-a", key, 1, "disable"),
    )
    .await;
    assert_eq!(replayed_disable, disabled);
    assert!(
        store
            .list_subscriptions(crate::TriggerSubscriptionFilter::for_session("session-a"))
            .await
            .unwrap()[0]
            .enabled,
        "receipt replay must not re-apply the mutation"
    );
}

async fn delete_tombstones_preserves_history_and_revive_changes_incarnation(
    store: Arc<dyn crate::TriggerStore>,
) {
    let key = "revive-key";
    let source_key = "revive-source";
    let draft = sample_draft("session-a", key, source_key, "worker");
    let created = mutate(
        &store,
        "revive-register",
        register_command("session-a", draft.clone()),
    )
    .await;
    let ingress = store
        .ingest_occurrence(button_occurrence(source_key, "revive-occurrence"))
        .await
        .unwrap();
    let deleted = mutate(
        &store,
        "revive-delete",
        revision_command("session-a", key, 1, "delete"),
    )
    .await;
    assert!(deleted.record_snapshot.tombstoned);
    assert!(
        store
            .list_subscriptions(crate::TriggerSubscriptionFilter::for_session("session-a"))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .list_deliveries_by_occurrence_id(&ingress.occurrence.occurrence_id)
            .await
            .unwrap()[0]
            .subscription
            .incarnation,
        created.incarnation
    );
    assert!(
        execute(
            &store,
            "revive-register-after-delete",
            register_command("session-a", draft.clone())
        )
        .await
        .is_err()
    );
    let revived = mutate(
        &store,
        "revive-command",
        crate::TriggerCommand::Revive {
            owner_scope: owner("session-a"),
            actor: actor("session-a"),
            subscription_key: key.to_string(),
            draft,
            expected_revision: deleted.revision,
        },
    )
    .await;
    assert_eq!(revived.subscription_id, created.subscription_id);
    assert_ne!(revived.incarnation, created.incarnation);
    assert_eq!(revived.revision, 3);
}

async fn owner_namespaces_are_exact_and_session_cleanup_is_scoped(
    store: Arc<dyn crate::TriggerStore>,
) {
    let session_draft = sample_draft("root", "shared-key", "session-source", "session-worker");
    mutate(
        &store,
        "scope-session-register",
        register_command("root", session_draft),
    )
    .await;
    let mut host_draft = sample_draft("host", "shared-key", "host-source", "host-worker");
    host_draft.wake_target = None;
    let host_owner = crate::TriggerOwnerScope::host("binding-a").unwrap();
    let host = mutate(
        &store,
        "scope-host-register",
        crate::TriggerCommand::Register {
            owner_scope: host_owner.clone(),
            actor: crate::ProcessOriginator::host_scoped("binding-a"),
            draft: host_draft,
        },
    )
    .await;
    assert_ne!(
        host.subscription_id,
        crate::deterministic_subscription_id(&owner("root"), "shared-key")
    );
    let visible_to_session = execute(
        &store,
        "scope-session-list",
        crate::TriggerCommand::List {
            owner_scope: owner("root"),
            filter: crate::TriggerSubscriptionFilter::default(),
        },
    )
    .await
    .unwrap();
    let crate::TriggerCommandOutcome::List { records } = visible_to_session else {
        panic!("expected list")
    };
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].owner_scope, owner("root"));
    assert_eq!(store.delete_session_subscriptions("root").await.unwrap(), 1);
    let host_rows = store
        .list_subscriptions(crate::TriggerSubscriptionFilter::for_registrant_scope(
            host_owner.namespace(),
        ))
        .await
        .unwrap();
    assert_eq!(
        host_rows.len(),
        1,
        "session cleanup must not delete host resources"
    );
}

/// Occurrence time-bound filters must agree with
/// [`TriggerOccurrenceFilter::matches`](crate::TriggerOccurrenceFilter::matches)
/// for every `u64` bound, including bounds above `i64::MAX`.
///
/// SQL backends store `occurred_at_ms` in a signed 64-bit column; a raw
/// `as i64` cast of a huge bound wraps to a negative value and inverts the
/// comparison, so a host filtering with `u64::MAX` would get the complement of
/// the in-memory answer instead of the same answer.
async fn occurrence_time_bounds_match_the_rust_predicate(store: Arc<dyn crate::TriggerStore>) {
    for (index, source_key) in ["bounds-source-a", "bounds-source-b"].iter().enumerate() {
        store
            .ingest_occurrence(button_occurrence(
                *source_key,
                format!("bounds-occurrence-{index}"),
            ))
            .await
            .expect("ingest occurrence");
    }
    let all = store
        .list_occurrences(crate::TriggerOccurrenceFilter::default())
        .await
        .expect("list every occurrence");
    assert!(
        all.len() >= 2,
        "fixture must record occurrences so an inverted bound is observable"
    );
    let above_i64_max = (i64::MAX as u64) + 1;
    for filter in [
        crate::TriggerOccurrenceFilter {
            occurred_at_start_ms: Some(u64::MAX),
            ..crate::TriggerOccurrenceFilter::default()
        },
        crate::TriggerOccurrenceFilter {
            occurred_at_start_ms: Some(above_i64_max),
            ..crate::TriggerOccurrenceFilter::default()
        },
        crate::TriggerOccurrenceFilter {
            occurred_at_end_ms: Some(u64::MAX),
            ..crate::TriggerOccurrenceFilter::default()
        },
        crate::TriggerOccurrenceFilter {
            occurred_at_end_ms: Some(above_i64_max),
            ..crate::TriggerOccurrenceFilter::default()
        },
        crate::TriggerOccurrenceFilter {
            occurred_at_start_ms: Some(0),
            occurred_at_end_ms: Some(u64::MAX),
            ..crate::TriggerOccurrenceFilter::default()
        },
        crate::TriggerOccurrenceFilter {
            source_key: Some("bounds-source-a".to_string()),
            occurred_at_end_ms: Some(u64::MAX),
            ..crate::TriggerOccurrenceFilter::default()
        },
    ] {
        let expected = all
            .iter()
            .filter(|record| filter.matches(record))
            .map(|record| record.occurrence_id.clone())
            .collect::<Vec<_>>();
        let actual = store
            .list_occurrences(filter.clone())
            .await
            .expect("list filtered occurrences")
            .into_iter()
            .map(|record| record.occurrence_id)
            .collect::<Vec<_>>();
        assert_eq!(
            actual, expected,
            "occurrence pushdown must match the Rust predicate for {filter:?}"
        );
    }
}

async fn occurrence_and_reservations_are_atomic_and_idempotent(
    store: Arc<dyn crate::TriggerStore>,
) {
    mutate(
        &store,
        "atomic-register",
        register_command(
            "session-a",
            sample_draft("session-a", "atomic-key", "atomic-source", "worker"),
        ),
    )
    .await;
    let request = button_occurrence("atomic-source", "atomic-occurrence");
    let first = store.ingest_occurrence(request.clone()).await.unwrap();
    let replay = store.ingest_occurrence(request).await.unwrap();
    assert_eq!(first.occurrence, replay.occurrence);
    assert_eq!(first.reservations.len(), 1);
    assert_eq!(replay.reservations.len(), 1);
    assert_eq!(
        first.reservations[0].process_id,
        replay.reservations[0].process_id
    );
    assert_eq!(
        replay.reservations[0].reservation_status,
        crate::TriggerDeliveryReservationOutcome::AlreadyReserved
    );
}

/// Law: ingest accounting arms a zero-match fan-out immediately, so the host
/// lever can reclaim it without any timer or delivery transition.
async fn zero_match_occurrence_is_immediately_reclaimable(store: Arc<dyn crate::TriggerStore>) {
    let ingress = store
        .ingest_occurrence(button_occurrence(
            "zero-match-source",
            "zero-match-immediate-eligibility",
        ))
        .await
        .expect("ingest zero-match occurrence");
    assert!(ingress.reservations.is_empty());

    let report = store
        .reclaim_trigger_occurrences(u64::MAX)
        .await
        .expect("reclaim armed zero-match occurrence");
    assert_eq!(report.inspected_occurrence_count, 1);
    assert_eq!(report.reclaimed_occurrence_count, 1);
    assert_eq!(report.live_fan_out_count, 0);
    assert_eq!(report.grace_deferred_count, 0);
    assert_eq!(
        crate::MaintenanceReport::sweep(&report),
        crate::MaintenanceSweep::Swept
    );
    assert!(
        store
            .list_occurrences(crate::TriggerOccurrenceFilter::default())
            .await
            .expect("list after zero-match reclaim")
            .is_empty()
    );
}

/// Law and negative proof: even the widest possible cutoff cannot reach a
/// matched occurrence while its delivery fan-out remains live. Deleting the
/// final terminal delivery arms the parent, after which the same host lever can
/// reclaim it.
async fn matched_occurrence_waits_for_terminal_deliveries(store: Arc<dyn crate::TriggerStore>) {
    mutate(
        &store,
        "matched-retention-register",
        register_command(
            "matched-retention-session",
            sample_draft(
                "matched-retention-session",
                "matched-retention-key",
                "matched-retention-source",
                "matched-retention-worker",
            ),
        ),
    )
    .await;
    let ingress = store
        .ingest_occurrence(button_occurrence(
            "matched-retention-source",
            "matched-retention-occurrence",
        ))
        .await
        .expect("ingest matched occurrence");
    assert_eq!(ingress.reservations.len(), 1);

    let blocked = store
        .reclaim_trigger_occurrences(u64::MAX)
        .await
        .expect("live fan-out is a reported blocker, not a backend failure");
    assert_eq!(blocked.inspected_occurrence_count, 1);
    assert_eq!(blocked.reclaimed_occurrence_count, 0);
    assert_eq!(blocked.live_fan_out_count, 1);
    assert_eq!(blocked.grace_deferred_count, 0);
    assert_eq!(
        crate::MaintenanceReport::sweep(&blocked),
        crate::MaintenanceSweep::Incomplete
    );
    assert_eq!(
        store
            .list_occurrences(crate::TriggerOccurrenceFilter::default())
            .await
            .expect("matched occurrence survives ancient cutoff")
            .len(),
        1,
        "a cutoff must never initiate reclaim for a live fan-out"
    );

    let reservation = &ingress.reservations[0];
    let terminal_delivery = crate::TriggerDeliveryRetentionCandidate {
        occurrence_id: ingress.occurrence.occurrence_id.clone(),
        subscription_id: reservation.subscription.subscription_id.clone(),
        process_id: reservation.process_id.clone(),
    };
    assert_eq!(
        store
            .delete_delivery_retention_candidates(&[terminal_delivery])
            .await
            .expect("delete the final terminal delivery"),
        1
    );
    let reclaimed = store
        .reclaim_trigger_occurrences(u64::MAX)
        .await
        .expect("reclaim occurrence armed by final delivery terminality");
    assert_eq!(reclaimed.reclaimed_occurrence_count, 1);
    assert_eq!(reclaimed.live_fan_out_count, 0);
}

/// Law: the cutoff delays eligibility that was already armed. Moving the
/// cutoff forward can reclaim that row, but still cannot arm a live fan-out.
async fn cutoff_defers_but_never_initiates_occurrence_reclaim(store: Arc<dyn crate::TriggerStore>) {
    store
        .ingest_occurrence(button_occurrence(
            "cutoff-zero-match",
            "cutoff-zero-match-occurrence",
        ))
        .await
        .expect("ingest cutoff-deferred zero-match occurrence");
    mutate(
        &store,
        "cutoff-live-register",
        register_command(
            "cutoff-live-session",
            sample_draft(
                "cutoff-live-session",
                "cutoff-live-key",
                "cutoff-live-source",
                "cutoff-live-worker",
            ),
        ),
    )
    .await;
    store
        .ingest_occurrence(button_occurrence(
            "cutoff-live-source",
            "cutoff-live-occurrence",
        ))
        .await
        .expect("ingest cutoff-proof live occurrence");

    let deferred = store
        .reclaim_trigger_occurrences(0)
        .await
        .expect("old cutoff completes with typed blockers");
    assert_eq!(deferred.inspected_occurrence_count, 2);
    assert_eq!(deferred.reclaimed_occurrence_count, 0);
    assert_eq!(deferred.grace_deferred_count, 1);
    assert_eq!(deferred.live_fan_out_count, 1);

    let advanced = store
        .reclaim_trigger_occurrences(u64::MAX)
        .await
        .expect("advanced cutoff reclaims only the armed occurrence");
    assert_eq!(advanced.inspected_occurrence_count, 2);
    assert_eq!(advanced.reclaimed_occurrence_count, 1);
    assert_eq!(advanced.live_fan_out_count, 1);
    assert_eq!(advanced.grace_deferred_count, 0);
    assert_eq!(
        crate::MaintenanceReport::sweep(&advanced),
        crate::MaintenanceSweep::Incomplete
    );
    assert_eq!(
        store
            .list_occurrences(crate::TriggerOccurrenceFilter::default())
            .await
            .expect("only live occurrence remains")
            .len(),
        1
    );
}

async fn null_source_occurrence_replay_is_idempotent(store: Arc<dyn crate::TriggerStore>) {
    let request = crate::TriggerOccurrenceRequest::new(
        "ui.button.pressed",
        "null-source",
        serde_json::json!({"button": "Blue"}),
        "null-source-occurrence",
    )
    .with_source(serde_json::Value::Null);
    let first = store
        .ingest_occurrence(request.clone())
        .await
        .expect("ingest null-source occurrence");
    let replay = store
        .ingest_occurrence(request)
        .await
        .expect("an exact null-source retry must be idempotent");
    assert_eq!(
        replay.occurrence.occurrence_id, first.occurrence.occurrence_id,
        "a null-source retry must return the original occurrence"
    );
}

async fn first_ingress_and_replay_share_canonical_subscription_order(
    store: Arc<dyn crate::TriggerStore>,
) {
    let owner_scope = crate::TriggerOwnerScope::host("fig811").unwrap();
    for key in ["gamma", "alpha"] {
        let mut draft = sample_draft("fig811", key, "canonical-order-source", key);
        draft.wake_target = None;
        mutate(
            &store,
            &format!("canonical-order-register-{key}"),
            crate::TriggerCommand::Register {
                owner_scope: owner_scope.clone(),
                actor: crate::ProcessOriginator::host_scoped("fig811"),
                draft,
            },
        )
        .await;
    }
    let request = button_occurrence("canonical-order-source", "canonical-order-occurrence");
    let first = store.ingest_occurrence(request.clone()).await.unwrap();
    let replay = store.ingest_occurrence(request).await.unwrap();
    let alpha_id = crate::deterministic_subscription_id(&owner_scope, "alpha");
    let gamma_id = crate::deterministic_subscription_id(&owner_scope, "gamma");
    assert!(
        alpha_id > gamma_id,
        "fixture must oppose hash order so canonical-order coverage cannot pass accidentally"
    );
    let keys = |ingress: &crate::TriggerIngressReceipt| {
        ingress
            .reservations
            .iter()
            .map(|reservation| reservation.subscription.subscription_key.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(keys(&first), vec!["alpha".to_string(), "gamma".to_string()]);
    assert_eq!(
        keys(&replay),
        vec!["alpha".to_string(), "gamma".to_string()]
    );
}

async fn same_identity_and_receipt_survive_store_reopen(factory: ReopenableTriggerStore) {
    let draft = sample_draft("session-a", "reopen-key", "reopen-source", "worker");
    let command = register_command("session-a", draft.clone());
    let first = mutate(&factory.open, "reopen-register", command.clone()).await;
    drop(factory.open);
    let replay = mutate(&factory.reopen, "reopen-register", command).await;
    assert_eq!(replay, first);
    let repeated = mutate(
        &factory.reopen,
        "reopen-register-again",
        register_command("session-a", draft),
    )
    .await;
    assert_eq!(repeated.subscription_id, first.subscription_id);
    assert_eq!(repeated.revision, 1);
    assert_eq!(
        factory
            .reopen
            .list_subscriptions(crate::TriggerSubscriptionFilter::for_session("session-a"))
            .await
            .unwrap()
            .len(),
        1
    );
}

#[cfg(test)]
mod trigger_retention_law_tests {
    use super::*;

    fn store() -> Arc<dyn crate::TriggerStore> {
        Arc::new(crate::InMemoryTriggerStore::default())
    }

    #[tokio::test]
    async fn session_scope_waits_for_owner_frontier_and_last_delivery() {
        session_tombstone_and_receipts_follow_deleted_owner_and_last_delivery(store()).await;
    }

    #[tokio::test]
    async fn host_scope_retains_the_permanent_revive_fence() {
        host_tombstone_remains_a_permanent_revive_fence(store()).await;
    }

    #[tokio::test]
    async fn committed_zero_match_fan_out_is_reclaimed() {
        zero_match_occurrence_reconciles_without_deliveries(store()).await;
    }

    #[tokio::test]
    async fn occurrence_with_a_live_delivery_is_retained() {
        occurrence_with_live_delivery_survives_reconciliation(store()).await;
    }
}
