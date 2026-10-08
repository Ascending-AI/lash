//! What both SQL trigger stores prepare identically around the shared
//! mutation evaluator: the command's admission check, its receipt id, its
//! fingerprint and its incarnation, the stored-receipt verdict, and the
//! record JSON the tables store.
//!
//! A backend keeps its own SQL, transaction and locking mechanics; the rules
//! here are pure and have one home.

use super::*;

/// Everything one non-list trigger command needs from durable identity,
/// resolved before the backend's transaction mechanics begin.
pub struct TriggerMutationPreparation {
    /// The operation id the caller issued; the receipt-reuse conflict names
    /// it, and the command's incarnation derives from it (ADR 0113 §1) — not
    /// from [`Self::receipt_id`].
    pub public_operation_id: String,
    /// The `trigger_mutation_receipts` key of this operation.
    pub receipt_id: String,
    /// The command's fingerprint, compared to a stored receipt's.
    pub request_fingerprint: String,
    /// The incarnation a `Register`/`Revive` mutation stamps on its record.
    pub incarnation: String,
    pub owner_scope: TriggerOwnerScope,
    pub subscription_key: String,
    /// The `trigger_subscriptions` key of the command's target, and the lock
    /// a PostgreSQL backend takes before its receipt read.
    pub subscription_id: String,
}

/// What [`prepare_trigger_command`] resolved one command to.
pub enum PreparedTriggerCommand {
    /// A listing is never receipted: serve the filter — its registrant scope
    /// already bound to the command's owner — with the backend's own read.
    List(TriggerSubscriptionFilter),
    /// A mutation or prune: the command and everything its execution needs
    /// besides the store's rows. Both members box so a `List` filter never
    /// carries the mutation payload's size.
    Mutation {
        command: Box<TriggerCommand>,
        preparation: Box<TriggerMutationPreparation>,
    },
}

/// The admission and identity resolution every store runs before it decides
/// durable state: an invalid operation or owner id is refused, a `List`
/// command binds its registrant scope, and every other command gets its
/// receipt id, fingerprint, incarnation, subscription key and id.
///
/// `fixed_incarnation` is the testing seam that pins otherwise-random
/// incarnation identity for durable fixture generation.
pub fn prepare_trigger_command(
    command: TriggerCommand,
    operation_id: &str,
    fixed_incarnation: Option<String>,
) -> Result<PreparedTriggerCommand, TriggerOperationError> {
    let owner_valid = match command.owner_scope() {
        TriggerOwnerScope::Session { session_id } => {
            crate::store::namespace::is_valid_opaque_key(session_id)
        }
        TriggerOwnerScope::Host { binding_id } => {
            crate::store::namespace::is_valid_opaque_key(binding_id.trim())
        }
        TriggerOwnerScope::Platform => true,
    };
    if !crate::store::namespace::is_valid_opaque_key(operation_id.trim()) || !owner_valid {
        return Err(TriggerOperationError::Invalid {
            message: "invalid trigger operation or owner identifier".into(),
        });
    }
    if let TriggerCommand::List {
        owner_scope,
        mut filter,
    } = command
    {
        filter.registrant_scope_id = Some(owner_scope.namespace());
        return Ok(PreparedTriggerCommand::List(filter));
    }
    let public_operation_id = operation_id.to_string();
    Ok(PreparedTriggerCommand::Mutation {
        preparation: Box::new(TriggerMutationPreparation {
            receipt_id: trigger_operation_receipt_id(command.owner_scope(), operation_id),
            request_fingerprint: trigger_command_fingerprint(&command),
            // The incarnation derives from the command's own operation id,
            // not its receipt id: the revision referrer the command's effect
            // held before this commit names that incarnation (ADR 0113 §1).
            incarnation: fixed_incarnation.unwrap_or_else(|| {
                trigger_incarnation(command.owner_scope(), &public_operation_id)
            }),
            owner_scope: command.owner_scope().clone(),
            subscription_key: command.subscription_key().unwrap_or_default().to_string(),
            subscription_id: deterministic_subscription_id(
                command.owner_scope(),
                command.subscription_key().unwrap_or_default(),
            ),
            public_operation_id,
        }),
        command: Box::new(command),
    })
}

/// What a stored receipt row means for `preparation`: the recorded result,
/// replayed, or — when the same operation id journaled a different command —
/// the reuse conflict.
pub fn stored_trigger_receipt(
    stored_fingerprint: String,
    stored_result_json: &str,
    preparation: &TriggerMutationPreparation,
) -> Result<TriggerEffectResult, PluginError> {
    if stored_fingerprint != preparation.request_fingerprint {
        return Ok(Err(TriggerOperationError::Conflict {
            subscription_key: preparation.subscription_key.clone(),
            existing_revision: None,
            existing_definition_fingerprint: Some(stored_fingerprint),
            requested_definition_fingerprint: Some(preparation.request_fingerprint.clone()),
            reason: format!(
                "operation id `{}` was reused with different content",
                preparation.public_operation_id
            ),
        }));
    }
    decode_trigger_mutation_receipt_json(stored_result_json)
}

/// The subscription snapshots a mutation result wrote: the receipt's record,
/// or every receipt of a prune. A backend upserts each, then journals the
/// result.
pub fn trigger_mutation_records(result: &TriggerEffectResult) -> Vec<&TriggerSubscriptionRecord> {
    match result {
        Ok(TriggerCommandOutcome::Mutation { receipt }) => vec![&receipt.record],
        Ok(TriggerCommandOutcome::Prune { receipts }) => {
            receipts.iter().map(|receipt| &receipt.record).collect()
        }
        Ok(TriggerCommandOutcome::List { .. }) | Err(_) => Vec::new(),
    }
}

/// The sessions whose own subscriptions a mutation result wrote, once each:
/// a backend wakes each in the mutation's transaction, so the session reads
/// its scheduled sources again at its next pass (FIG-5348).
pub fn trigger_mutation_sessions(result: &TriggerEffectResult) -> Vec<crate::SessionId> {
    let mut sessions = Vec::new();
    for record in trigger_mutation_records(result) {
        if let TriggerOwnerScope::Session { session_id } = &record.owner_scope
            && !sessions.contains(session_id)
        {
            sessions.push(session_id.clone());
        }
    }
    sessions
}

/// The `record_json` column of `trigger_subscriptions`.
pub fn decode_trigger_subscription_json(
    json: &str,
) -> Result<TriggerSubscriptionRecord, PluginError> {
    serde_json::from_str(json).map_err(|error| {
        PluginError::Session(format!(
            "failed to decode trigger subscription row: {error}"
        ))
    })
}

/// The `record_json` column of `trigger_occurrences`.
pub fn decode_trigger_occurrence_json(json: &str) -> Result<TriggerOccurrenceRecord, PluginError> {
    serde_json::from_str(json).map_err(|error| {
        PluginError::Session(format!("failed to decode trigger occurrence row: {error}"))
    })
}

/// The `result_json` column of `trigger_mutation_receipts`.
pub fn decode_trigger_mutation_receipt_json(
    json: &str,
) -> Result<TriggerEffectResult, PluginError> {
    serde_json::from_str(json).map_err(|error| {
        PluginError::Session(format!(
            "failed to decode trigger mutation receipt: {error}"
        ))
    })
}

/// One `trigger_deliveries` row: the frozen record JSON, the bound process
/// and the creation stamp.
pub fn decode_trigger_delivery(
    occurrence_json: &str,
    subscription_json: &str,
    process_id: Option<ProcessId>,
    created_at_ms: i64,
    status: &str,
    refusal_json: Option<&str>,
) -> Result<TriggerDeliveryReservation, PluginError> {
    Ok(TriggerDeliveryReservation {
        occurrence: decode_trigger_occurrence_json(occurrence_json)?,
        subscription: decode_trigger_subscription_json(subscription_json)?,
        outcome: decode_trigger_delivery_outcome(process_id, status, refusal_json)?,
        created_at_ms: u64::try_from(created_at_ms).map_err(|_| {
            PluginError::StoredDataCorrupt {
                record_kind: "TriggerDelivery".to_string(),
                message: format!("created_at_ms must be non-negative, got {created_at_ms}"),
            }
        })?,
    })
}

/// Decode a delivery's stored disposition, refusing inconsistent rows.
pub fn decode_trigger_delivery_outcome(
    process_id: Option<ProcessId>,
    status: &str,
    refusal_json: Option<&str>,
) -> Result<TriggerDeliveryEmitOutcome, PluginError> {
    let corrupt = |message: String| PluginError::StoredDataCorrupt {
        record_kind: "TriggerDelivery".to_owned(),
        message,
    };
    match (status, process_id, refusal_json) {
        ("started", Some(process_id), None) => {
            Ok(TriggerDeliveryEmitOutcome::Started { process_id })
        }
        ("refused", None, Some(json)) => {
            let outcome = serde_json::from_str::<TriggerDeliveryEmitOutcome>(json)
                .map_err(|error| corrupt(error.to_string()))?;
            match outcome {
                TriggerDeliveryEmitOutcome::Failed { .. } => Ok(outcome),
                _ => Err(corrupt(
                    "refused delivery contains a started outcome".to_owned(),
                )),
            }
        }
        _ => Err(corrupt("inconsistent delivery disposition".to_owned())),
    }
}

/// The `record_json`/`result_json` column values both backends write.
pub fn encode_trigger_row<T: serde::Serialize>(value: &T) -> Result<String, PluginError> {
    serde_json::to_string(value)
        .map_err(|error| PluginError::Session(format!("failed to encode trigger row: {error}")))
}
