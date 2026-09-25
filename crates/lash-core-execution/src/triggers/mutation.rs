use super::*;

/// Evaluate one mutation against the current logical row. Durable stores use
/// this shared oracle inside their own transaction, then persist the receipt
/// and returned record snapshot atomically.
pub fn evaluate_trigger_mutation(
    current: Option<TriggerSubscriptionRecord>,
    command: TriggerCommand,
    now: u64,
) -> Result<TriggerEffectResult, PluginError> {
    if !command.is_mutation() {
        return Err(PluginError::Session(
            "trigger mutation evaluator received a list command".to_string(),
        ));
    }
    let mut subscriptions = BTreeMap::new();
    if let Some(record) = current {
        subscriptions.insert(record.subscription_id.clone(), record);
    }
    Ok(apply_trigger_command(&mut subscriptions, command, now))
}

/// Testing seam for fixture generators that must pin otherwise-random trigger identity.
pub fn evaluate_trigger_mutation_with_incarnation(
    current: Option<TriggerSubscriptionRecord>,
    command: TriggerCommand,
    now: u64,
    incarnation: String,
) -> Result<TriggerEffectResult, PluginError> {
    if !command.is_mutation() {
        return Err(PluginError::Session(
            "trigger mutation evaluator received a list command".to_string(),
        ));
    }
    let mut subscriptions = BTreeMap::new();
    if let Some(record) = current {
        subscriptions.insert(record.subscription_id.clone(), record);
    }
    Ok(apply_trigger_command_with_incarnation(
        &mut subscriptions,
        command,
        now,
        &mut || incarnation.clone(),
    ))
}

/// Apply one trigger mutation to a subscription map keyed by subscription id.
/// This is the shared command semantics: durable stores reach it through
/// [`evaluate_trigger_mutation`] with the one current row as the map.
fn apply_trigger_command(
    subscriptions: &mut BTreeMap<String, TriggerSubscriptionRecord>,
    command: TriggerCommand,
    now: u64,
) -> TriggerEffectResult {
    apply_trigger_command_with_incarnation(subscriptions, command, now, &mut || {
        uuid::Uuid::new_v4().to_string()
    })
}

fn apply_trigger_command_with_incarnation(
    subscriptions: &mut BTreeMap<String, TriggerSubscriptionRecord>,
    command: TriggerCommand,
    now: u64,
    new_incarnation: &mut dyn FnMut() -> String,
) -> TriggerEffectResult {
    match command {
        // The evaluators refuse a list before it gets here: a list reads the
        // store's rows and is no mutation of one row.
        TriggerCommand::List { .. } => Err(TriggerOperationError::Invalid {
            message: "the trigger mutation evaluator received a list command".to_string(),
        }),
        TriggerCommand::Prune {
            owner_scope,
            actor,
            subscription_keys,
        } => {
            let records = subscriptions.values().cloned().collect::<Vec<_>>();
            let result =
                evaluate_trigger_prune(records, owner_scope, actor, subscription_keys, now)?;
            if let TriggerCommandOutcome::Prune { receipts } = &result {
                for receipt in receipts {
                    subscriptions.insert(
                        receipt.subscription_id.clone(),
                        receipt.record_snapshot.clone(),
                    );
                }
            }
            Ok(result)
        }
        TriggerCommand::Register {
            owner_scope,
            actor,
            draft,
        } => {
            draft.validate().map_err(TriggerOperationError::from)?;
            let subscription_id =
                deterministic_subscription_id(&owner_scope, &draft.subscription_key);
            let definition_fingerprint =
                trigger_subscription_definition_fingerprint(&owner_scope, &draft);
            if let Some(existing) = subscriptions.get(&subscription_id).cloned() {
                if !existing.is_tombstoned()
                    && existing.definition_fingerprint == definition_fingerprint
                {
                    return Ok(TriggerCommandOutcome::Mutation {
                        receipt: Box::new(TriggerMutationReceipt::from_record(
                            existing,
                            TriggerMutationOutcome::Unchanged,
                        )),
                    });
                }
                return Err(subscription_conflict(
                    &draft.subscription_key,
                    Some(&existing),
                    Some(definition_fingerprint),
                    if existing.is_tombstoned() {
                        "subscription is tombstoned; use revive"
                    } else {
                        "register does not replace a different definition; use update"
                    },
                ));
            }
            let record = subscription_record_from_draft(
                owner_scope,
                actor,
                draft,
                subscription_id.clone(),
                new_incarnation(),
                1,
                definition_fingerprint,
                true,
                now,
                now,
            );
            subscriptions.insert(subscription_id, record.clone());
            Ok(TriggerCommandOutcome::Mutation {
                receipt: Box::new(TriggerMutationReceipt::from_record(
                    record,
                    TriggerMutationOutcome::Created,
                )),
            })
        }
        TriggerCommand::Update {
            owner_scope,
            actor,
            subscription_key,
            mut draft,
            expected_revision,
        } => {
            draft.subscription_key.clone_from(&subscription_key);
            draft.validate().map_err(TriggerOperationError::from)?;
            let subscription_id = deterministic_subscription_id(&owner_scope, &subscription_key);
            let requested_hash = trigger_subscription_definition_fingerprint(&owner_scope, &draft);
            let Some(existing) = subscriptions.get(&subscription_id).cloned() else {
                return Err(subscription_conflict(
                    &subscription_key,
                    None,
                    Some(requested_hash),
                    "subscription does not exist",
                ));
            };
            ensure_live_revision(&existing, expected_revision, Some(requested_hash.clone()))?;
            let next_revision = next_trigger_revision(&existing)?;
            let record = subscription_record_from_draft(
                owner_scope,
                actor,
                draft,
                subscription_id.clone(),
                existing.incarnation,
                next_revision,
                requested_hash,
                existing.lifecycle.enabled(),
                existing.created_at_ms,
                now,
            );
            subscriptions.insert(subscription_id, record.clone());
            Ok(TriggerCommandOutcome::Mutation {
                receipt: Box::new(TriggerMutationReceipt::from_record(
                    record,
                    TriggerMutationOutcome::Updated,
                )),
            })
        }
        TriggerCommand::Enable {
            owner_scope,
            actor,
            subscription_key,
            expected_revision,
        } => mutate_enabled(
            subscriptions,
            owner_scope,
            actor,
            subscription_key,
            expected_revision,
            true,
            now,
        ),
        TriggerCommand::Disable {
            owner_scope,
            actor,
            subscription_key,
            expected_revision,
        } => mutate_enabled(
            subscriptions,
            owner_scope,
            actor,
            subscription_key,
            expected_revision,
            false,
            now,
        ),
        TriggerCommand::Delete {
            owner_scope,
            actor,
            subscription_key,
            expected_revision,
        } => {
            let subscription_id = deterministic_subscription_id(&owner_scope, &subscription_key);
            let Some(existing) = subscriptions.get_mut(&subscription_id) else {
                return Err(subscription_conflict(
                    &subscription_key,
                    None,
                    None,
                    "subscription does not exist",
                ));
            };
            ensure_live_revision(existing, expected_revision, None)?;
            let next_revision = next_trigger_revision(existing)?;
            existing.registrant = actor;
            existing.tombstone(now);
            existing.revision = next_revision;
            existing.updated_at_ms = now;
            Ok(TriggerCommandOutcome::Mutation {
                receipt: Box::new(TriggerMutationReceipt::from_record(
                    existing.clone(),
                    TriggerMutationOutcome::Deleted,
                )),
            })
        }
        TriggerCommand::Revive {
            owner_scope,
            actor,
            subscription_key,
            mut draft,
            expected_revision,
        } => {
            draft.subscription_key.clone_from(&subscription_key);
            draft.validate().map_err(TriggerOperationError::from)?;
            let subscription_id = deterministic_subscription_id(&owner_scope, &subscription_key);
            let requested_hash = trigger_subscription_definition_fingerprint(&owner_scope, &draft);
            let Some(existing) = subscriptions.get(&subscription_id).cloned() else {
                return Err(subscription_conflict(
                    &subscription_key,
                    None,
                    Some(requested_hash),
                    "subscription does not exist; use register",
                ));
            };
            if !existing.is_tombstoned() || existing.revision != expected_revision {
                return Err(subscription_conflict(
                    &subscription_key,
                    Some(&existing),
                    Some(requested_hash),
                    "revive requires the current tombstone revision",
                ));
            }
            let next_revision = next_trigger_revision(&existing)?;
            let record = subscription_record_from_draft(
                owner_scope,
                actor,
                draft,
                subscription_id.clone(),
                new_incarnation(),
                next_revision,
                requested_hash,
                true,
                existing.created_at_ms,
                now,
            );
            subscriptions.insert(subscription_id, record.clone());
            Ok(TriggerCommandOutcome::Mutation {
                receipt: Box::new(TriggerMutationReceipt::from_record(
                    record,
                    TriggerMutationOutcome::Revived,
                )),
            })
        }
    }
}

fn mutate_enabled(
    subscriptions: &mut BTreeMap<String, TriggerSubscriptionRecord>,
    owner_scope: TriggerOwnerScope,
    actor: crate::ProcessOriginator,
    subscription_key: String,
    expected_revision: u64,
    enabled: bool,
    now: u64,
) -> TriggerEffectResult {
    let subscription_id = deterministic_subscription_id(&owner_scope, &subscription_key);
    let Some(existing) = subscriptions.get_mut(&subscription_id) else {
        return Err(subscription_conflict(
            &subscription_key,
            None,
            None,
            "subscription does not exist",
        ));
    };
    ensure_live_revision(existing, expected_revision, None)?;
    let requested = if enabled {
        crate::triggers::TriggerSubscriptionLifecycle::Enabled
    } else {
        crate::triggers::TriggerSubscriptionLifecycle::Disabled
    };
    if existing.lifecycle != requested {
        let next_revision = next_trigger_revision(existing)?;
        existing.lifecycle = requested;
        existing.registrant = actor;
        existing.revision = next_revision;
        existing.updated_at_ms = now;
    }
    let disposition = if enabled {
        TriggerMutationOutcome::Enabled
    } else {
        TriggerMutationOutcome::Disabled
    };
    Ok(TriggerCommandOutcome::Mutation {
        receipt: Box::new(TriggerMutationReceipt::from_record(
            existing.clone(),
            disposition,
        )),
    })
}

fn ensure_live_revision(
    existing: &TriggerSubscriptionRecord,
    expected_revision: u64,
    requested_hash: Option<String>,
) -> Result<(), TriggerOperationError> {
    if existing.is_tombstoned() || existing.revision != expected_revision {
        return Err(subscription_conflict(
            &existing.subscription_key,
            Some(existing),
            requested_hash,
            if existing.is_tombstoned() {
                "subscription is tombstoned"
            } else {
                "expected revision does not match"
            },
        ));
    }
    Ok(())
}

fn subscription_conflict(
    subscription_key: &str,
    existing: Option<&TriggerSubscriptionRecord>,
    requested_definition_fingerprint: Option<String>,
    reason: &str,
) -> TriggerOperationError {
    TriggerOperationError::Conflict {
        subscription_key: subscription_key.to_string(),
        existing_revision: existing.map(|record| record.revision),
        existing_definition_fingerprint: existing
            .map(|record| record.definition_fingerprint.clone()),
        requested_definition_fingerprint,
        reason: reason.to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
fn subscription_record_from_draft(
    owner_scope: TriggerOwnerScope,
    actor: crate::ProcessOriginator,
    draft: TriggerSubscriptionDraft,
    subscription_id: String,
    incarnation: String,
    revision: u64,
    definition_fingerprint: String,
    enabled: bool,
    created_at_ms: u64,
    updated_at_ms: u64,
) -> TriggerSubscriptionRecord {
    TriggerSubscriptionRecord {
        subscription_id,
        owner_scope,
        subscription_key: draft.subscription_key,
        incarnation,
        revision,
        definition_fingerprint,
        registrant: actor,
        env_ref: draft.env_ref,
        wake_target: draft.wake_target,
        name: draft.name,
        source_type: draft.source_type,
        source_key: draft.source_key,
        source: draft.source,
        payload_schema: draft.payload_schema,
        source_capture: draft.source_capture,
        target: draft.target,
        target_identity: draft.target_identity,
        event_types: draft.event_types,
        input_template: draft.input_template,
        target_label: draft.target_label,
        lifecycle: if enabled {
            crate::triggers::TriggerSubscriptionLifecycle::Enabled
        } else {
            crate::triggers::TriggerSubscriptionLifecycle::Disabled
        },
        created_at_ms,
        updated_at_ms,
    }
}
