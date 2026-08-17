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
    mutation_receipts_follow_retention_cutoff(make()).await;
    reservations_execute_the_reserved_revision(make()).await;
    disable_preserves_reserved_work_and_requires_explicit_enable(make()).await;
    register_disable_reenable_roundtrip_is_fenced_and_receipted(make()).await;
    delete_tombstones_preserves_history_and_revive_changes_incarnation(make()).await;
    owner_namespaces_are_exact_and_session_cleanup_is_scoped(make()).await;
    explicit_prune_is_journaled_and_owner_scoped(make()).await;
    occurrence_and_reservations_are_atomic_and_idempotent(make()).await;
    occurrence_time_bounds_match_the_rust_predicate(make()).await;
    null_source_occurrence_replay_is_idempotent(make()).await;
    first_ingress_and_replay_share_canonical_subscription_order(make()).await;
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
    assert_eq!(
        second.disposition,
        crate::TriggerMutationDisposition::Unchanged
    );
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

async fn mutation_receipts_follow_retention_cutoff(store: Arc<dyn crate::TriggerStore>) {
    let key = "receipt-retention-key";
    let command = register_command("session-a", sample_draft("session-a", key, "v1", "worker"));
    let created = mutate(&store, "receipt-retention-register", command.clone()).await;
    assert_eq!(
        created.disposition,
        crate::TriggerMutationDisposition::Created
    );
    assert_eq!(
        store.prune_mutation_receipts(u64::MAX).await.unwrap(),
        1,
        "the retention cutoff removes the aged mutation receipt"
    );
    let reevaluated = mutate(&store, "receipt-retention-register", command).await;
    assert_eq!(
        reevaluated.disposition,
        crate::TriggerMutationDisposition::Unchanged,
        "after retention, the operation is evaluated against current state"
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
        crate::TriggerMutationDisposition::Deleted
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
        crate::TriggerMutationDisposition::Disabled
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
        crate::TriggerMutationDisposition::Enabled
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
        crate::TriggerDeliveryReservationStatus::AlreadyReserved
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
    let keys = |ingress: &crate::TriggerIngressResult| {
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
