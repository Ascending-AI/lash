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
    let mut state = InMemoryTriggerEventState::default();
    if let Some(record) = current {
        state
            .subscriptions
            .insert(record.subscription_id.clone(), record);
    }
    Ok(apply_in_memory_trigger_command(&mut state, command, now))
}

/// Testing seam for fixture generators that must pin otherwise-random trigger identity.
#[doc(hidden)]
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
    let mut state = InMemoryTriggerEventState::default();
    if let Some(record) = current {
        state
            .subscriptions
            .insert(record.subscription_id.clone(), record);
    }
    Ok(apply_in_memory_trigger_command_with_incarnation(
        &mut state,
        command,
        now,
        &mut || incarnation.clone(),
    ))
}

pub(super) fn mutate_enabled(
    state: &mut InMemoryTriggerEventState,
    owner_scope: TriggerOwnerScope,
    actor: crate::ProcessOriginator,
    subscription_key: String,
    expected_revision: u64,
    enabled: bool,
    now: u64,
) -> TriggerEffectResult {
    let subscription_id = deterministic_subscription_id(&owner_scope, &subscription_key);
    let Some(existing) = state.subscriptions.get_mut(&subscription_id) else {
        return Err(subscription_conflict(
            &subscription_key,
            None,
            None,
            "subscription does not exist",
        ));
    };
    ensure_live_revision(existing, expected_revision, None)?;
    if existing.enabled != enabled {
        existing.enabled = enabled;
        existing.registrant = actor;
        existing.revision = existing.revision.saturating_add(1);
        existing.updated_at_ms = now;
    }
    let disposition = if enabled {
        TriggerMutationDisposition::Enabled
    } else {
        TriggerMutationDisposition::Disabled
    };
    Ok(TriggerCommandOutcome::Mutation {
        receipt: Box::new(TriggerMutationReceipt::from_record(
            existing.clone(),
            disposition,
        )),
    })
}

pub(super) fn ensure_live_revision(
    existing: &TriggerSubscriptionRecord,
    expected_revision: u64,
    requested_hash: Option<String>,
) -> Result<(), TriggerOperationError> {
    if existing.tombstoned || existing.revision != expected_revision {
        return Err(subscription_conflict(
            &existing.subscription_key,
            Some(existing),
            requested_hash,
            if existing.tombstoned {
                "subscription is tombstoned"
            } else {
                "expected revision does not match"
            },
        ));
    }
    Ok(())
}

pub(super) fn subscription_conflict(
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
pub(super) fn subscription_record_from_draft(
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
        target: draft.target,
        target_identity: draft.target_identity,
        event_types: draft.event_types,
        input_template: draft.input_template,
        target_label: draft.target_label,
        enabled,
        tombstoned: false,
        deleted_at_ms: None,
        created_at_ms,
        updated_at_ms,
    }
}
