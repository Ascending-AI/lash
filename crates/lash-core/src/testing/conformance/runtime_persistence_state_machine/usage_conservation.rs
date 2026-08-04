//! Conservation checks for pending and durable runtime usage.
//!
//! The durable comparison is intentionally aggregate-by-source/model because
//! `RuntimePersistence::load_session` exposes the merged token ledger, not its
//! row identities. Store conformance separately pins identity persistence.
//! Confirmation is a differential mutation oracle: its expected snapshot states
//! which returned identities may disappear, then checks the production retain.

use super::*;
use std::collections::HashSet;

use crate::RuntimeUsageDelta;

pub(super) fn record_usage(
    model: &mut ReferenceModel,
    shape: &mut RunShape,
    slot: u8,
    value: u8,
) -> Result<(), String> {
    let entry = usage_entry(slot, value);
    let before =
        (entry.usage == crate::TokenUsage::default()).then(|| pending_usage_snapshot(model));
    crate::runtime::record_token_usage_shared(
        &model.pending_usage,
        &entry.source,
        &entry.model,
        &entry.usage,
    );
    if let Some(before) = before
        && json(&pending_usage_snapshot(model))? != json(&before)?
    {
        return Err("zero usage recording changed the pending ledger".to_string());
    }
    model.recorded_usage.add(&entry.usage);
    shape.usage_records += 1;
    Ok(())
}

pub(super) fn stage_usage(
    model: &mut ReferenceModel,
    shape: &mut RunShape,
    seed: u64,
    replay_last_commit: bool,
) -> Result<(), String> {
    let operation = if replay_last_commit {
        replayable_unconfirmed_operation(model)?
            .unwrap_or_else(|| fresh_usage_operation(model, seed))
    } else {
        fresh_usage_operation(model, seed)
    };
    let staged = crate::runtime::stage_token_ledger_shared(&model.pending_usage, &operation)
        .map_err(|error| error.to_string())?;
    model.staged_usage = Some(staged);
    model.staged_usage_operation = Some(operation);
    shape.usage_stages += 1;
    Ok(())
}

fn replayable_unconfirmed_operation(
    model: &ReferenceModel,
) -> Result<Option<crate::OperationId>, String> {
    let Some(commit) = model.last_usage_commit.as_ref() else {
        return Ok(None);
    };
    let operation_storage_key = commit
        .turn_commit
        .operation
        .storage_key()
        .map_err(|error| error.to_string())?;
    let pending = pending_usage_snapshot(model);
    let pending_deltas = pending
        .iter()
        .map(|row| {
            row.identity.clone().map(|identity| RuntimeUsageDelta {
                identity,
                entry: row.entry.clone(),
            })
        })
        .collect::<Option<Vec<_>>>();
    for confirmation in &model.pending_usage_confirmations {
        let deltas = confirmation.staged.deltas();
        let belongs_to_last_commit = deltas
            .iter()
            .any(|delta| delta.identity.operation_storage_key == operation_storage_key);
        if belongs_to_last_commit
            && let Some(pending_deltas) = &pending_deltas
            && json(pending_deltas)? == json(&deltas)?
        {
            return Ok(Some(commit.turn_commit.operation.clone()));
        }
    }
    Ok(None)
}

fn fresh_usage_operation(model: &mut ReferenceModel, seed: u64) -> crate::OperationId {
    model.operation_sequence += 1;
    crate::OperationId::new(
        crate::ExecutionScope::runtime_operation(format!(
            "runtime-persistence-property:{seed}:usage:{}",
            model.operation_sequence
        )),
        "append-session-nodes",
    )
}

pub(super) fn confirm_usage(
    model: &mut ReferenceModel,
    shape: &mut RunShape,
    selection: u8,
) -> Result<(), String> {
    if model.pending_usage_confirmations.is_empty() {
        return Ok(());
    }
    let index = usize::from(selection) % model.pending_usage_confirmations.len();
    let confirmation = model.pending_usage_confirmations.remove(index);
    let before = pending_usage_snapshot(model);
    let confirmed = confirmation
        .identities
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    let expected = before
        .into_iter()
        .filter(|pending| {
            pending
                .identity
                .as_ref()
                .is_none_or(|identity| !confirmed.contains(identity))
        })
        .collect::<Vec<_>>();
    confirmation
        .staged
        .confirm_identities(&confirmation.identities);
    if json(&pending_usage_snapshot(model))? != json(&expected)? {
        return Err(
            "usage confirmation removed rows other than the store-returned identities".to_string(),
        );
    }
    shape.usage_confirmations += 1;
    Ok(())
}

pub(super) async fn replay_usage_receipt(
    store: &dyn RuntimePersistence,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
) -> Result<(), String> {
    let (Some(mut commit), Some(staged), Some(operation)) = (
        model.last_usage_commit.clone(),
        model.staged_usage.take(),
        model.staged_usage_operation.take(),
    ) else {
        return Ok(());
    };
    if operation.storage_key().map_err(|error| error.to_string())?
        != commit
            .turn_commit
            .operation
            .storage_key()
            .map_err(|error| error.to_string())?
    {
        model.staged_usage = Some(staged);
        model.staged_usage_operation = Some(operation);
        return Ok(());
    }
    let submitted = staged.deltas().to_vec();
    commit.usage_deltas = submitted.clone();
    let before = session_snapshot(store).await?;
    let result = store
        .commit_runtime_state(commit)
        .await
        .map_err(|error| error.to_string())?;
    if !result.receipt_replayed {
        return Err("usage receipt replay was applied as a fresh commit".to_string());
    }
    assert_snapshot_unchanged(store, before, "usage receipt replay").await?;
    register_committed_usage(model, &submitted, &result.committed_usage_delta_identities)?;
    model
        .pending_usage_confirmations
        .push(PendingUsageConfirmation {
            staged,
            identities: result.committed_usage_delta_identities,
        });
    shape.usage_receipt_replays += 1;
    Ok(())
}

fn usage_entry(slot: u8, value: u8) -> crate::TokenLedgerEntry {
    let usage = match value {
        0 => crate::TokenUsage::default(),
        u8::MAX => {
            // Leave enough headroom for the largest generated case even if
            // every operation records this payload. Five counters across
            // MAX_OPS rows then remain immediately below the checked total's
            // i64 boundary instead of making the generated case invalid.
            let edge = i64::MAX / ((MAX_OPS as i64 + 1) * 5);
            crate::TokenUsage {
                input_tokens: edge,
                output_tokens: edge + 1,
                cache_read_input_tokens: edge + 2,
                cache_write_input_tokens: edge + 3,
                reasoning_output_tokens: edge + 4,
            }
        }
        _ => {
            let base = i64::from(value);
            crate::TokenUsage {
                input_tokens: base,
                output_tokens: base + 1,
                cache_read_input_tokens: base + 2,
                cache_write_input_tokens: base + 3,
                reasoning_output_tokens: base.min(base + 1),
            }
        }
    };
    crate::TokenLedgerEntry {
        source: format!("property-usage-source-{}", slot % 3),
        model: format!("property-usage-model-{}", slot % 2),
        usage,
    }
}

fn pending_usage_snapshot(model: &ReferenceModel) -> Vec<crate::runtime::PendingTokenLedgerEntry> {
    model
        .pending_usage
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
}

pub(super) fn register_committed_usage(
    model: &mut ReferenceModel,
    submitted: &[RuntimeUsageDelta],
    committed: &[RuntimeUsageDeltaIdentity],
) -> Result<(), String> {
    let mut returned = HashSet::new();
    for identity in committed {
        if !returned.insert(identity.clone()) {
            return Err("store returned a duplicate committed usage identity".to_string());
        }
        if let Some(delta) = submitted.iter().find(|delta| delta.identity == *identity) {
            if let Some(existing) = model.durable_usage.get(identity)
                && json(existing)? != json(&delta.entry)?
            {
                return Err("one durable usage identity named different entry content".to_string());
            }
            model
                .durable_usage
                .entry(identity.clone())
                .or_insert_with(|| delta.entry.clone());
        } else if !model.durable_usage.contains_key(identity) {
            return Err(
                "store confirmed a usage identity absent from every submitted row".to_string(),
            );
        }
    }
    Ok(())
}

pub(super) async fn assert_usage_conservation(
    store: &dyn RuntimePersistence,
    model: &ReferenceModel,
) -> Result<(), String> {
    let pending = pending_usage_snapshot(model);
    let mut pending_identities = HashSet::new();
    for row in &pending {
        let Some(identity) = row.identity.as_ref() else {
            continue;
        };
        if !pending_identities.insert(identity.clone()) {
            return Err("pending usage rows repeated one content-bound identity".to_string());
        }
        let expected = RuntimeUsageDeltaIdentity::for_entry(
            identity.operation_storage_key.clone(),
            identity.entry_ordinal,
            &row.entry,
        );
        if expected != *identity {
            return Err("pending usage identity was not bound to its entry content".to_string());
        }
        if let Some(durable) = model.durable_usage.get(identity)
            && json(durable)? != json(&row.entry)?
        {
            return Err("pending and durable copies of one usage identity disagreed".to_string());
        }
    }
    for (identity, entry) in &model.durable_usage {
        let expected = RuntimeUsageDeltaIdentity::for_entry(
            identity.operation_storage_key.clone(),
            identity.entry_ordinal,
            entry,
        );
        if expected != *identity {
            return Err("durable usage identity was not bound to its entry content".to_string());
        }
    }

    let actual_entries = store
        .load_session()
        .await
        .map_err(|error| error.to_string())?
        .map(|session| session.token_ledger)
        .unwrap_or_default();
    let expected_durable = usage_by_source_model(model.durable_usage.values());
    if usage_by_source_model(actual_entries.iter()) != expected_durable {
        return Err(
            "durable usage ledger differs from confirmed identity-bearing rows".to_string(),
        );
    }

    let report = crate::facade_support::SessionUsageReport::from_entries(&actual_entries);
    let mut durable_parts = crate::TokenUsage::default();
    for entry in &actual_entries {
        durable_parts.add(&entry.usage);
    }
    if report.usage.usage != durable_parts || report.usage.total_tokens != durable_parts.total() {
        return Err(
            "durable ledger total differs from the sum of its surviving entry parts".to_string(),
        );
    }

    let mut surviving = crate::TokenUsage::default();
    for row in &pending {
        surviving.add(&row.entry.usage);
    }
    for (identity, entry) in &model.durable_usage {
        if !pending_identities.contains(identity) {
            surviving.add(&entry.usage);
        }
    }
    if surviving != model.recorded_usage {
        return Err(
            "recorded usage vanished without a confirmed commit carrying its identity".to_string(),
        );
    }
    Ok(())
}

fn usage_by_source_model<'a>(
    entries: impl IntoIterator<Item = &'a crate::TokenLedgerEntry>,
) -> BTreeMap<(String, String), crate::TokenUsage> {
    let mut totals = BTreeMap::new();
    for entry in entries {
        totals
            .entry((entry.source.clone(), entry.model.clone()))
            .or_insert_with(crate::TokenUsage::default)
            .add(&entry.usage);
    }
    totals
}
