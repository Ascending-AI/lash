//! Record, wake-delivery and queued-work agreement between a backend under
//! test and the independently-derived reference model.
use super::*;

/// Render the differing record fields so a model disagreement names the field,
/// not just the record. Falls back to whole-record `Debug` when either side
/// cannot be serialized.
fn record_field_diff(expected: &ProcessRecord, actual: &ProcessRecord) -> String {
    let (
        Ok(serde_json::Value::Object(expected_fields)),
        Ok(serde_json::Value::Object(actual_fields)),
    ) = (serde_json::to_value(expected), serde_json::to_value(actual))
    else {
        return format!("expected={expected:?}, actual={actual:?}");
    };
    let mut keys = expected_fields.keys().collect::<BTreeSet<_>>();
    keys.extend(actual_fields.keys());
    let mut differences = Vec::new();
    for key in keys {
        let expected_value = expected_fields.get(key);
        let actual_value = actual_fields.get(key);
        if expected_value != actual_value {
            differences.push(format!(
                "{key}: expected={}, actual={}",
                expected_value.map_or("<absent>".to_string(), ToString::to_string),
                actual_value.map_or("<absent>".to_string(), ToString::to_string),
            ));
        }
    }
    if differences.is_empty() {
        return format!("expected={expected:?}, actual={actual:?}");
    }
    differences.join("; ")
}

fn normalize_record(mut record: ProcessRecord) -> ProcessRecord {
    // Backend clocks are intentionally not synchronized. The independent model
    // pins every semantic record field; the fold law separately pins timestamps
    // by reconstructing them from the persisted event log.
    record.created_at_ms = 0;
    record.updated_at_ms = 0;
    record
}

/// Derive the terminal outcome a conforming registry must record for a process
/// that already carries an accepted cancel request.
///
/// A settled cancellation records *why* the process was cancelled: the origin
/// of the standing durable cancel request is stamped onto the cancellation
/// payload before the terminal event is built, so the persisted outcome names
/// the request that settled the row rather than only the runner's local
/// wording. The rule applies to nothing else — a success, a failure, an
/// abandonment, and a cancellation with no standing request all keep their
/// original representation.
///
/// The model re-derives the stamp from its OWN tracked `cancel_request`, which
/// the `CancelRequest` operation folded in independently, and never reads it
/// back from the store under test. A backend that skips the stamp, stamps a
/// different origin, stamps a non-cancelled outcome, or stamps one with no
/// standing request therefore still fails the law.
pub(super) fn terminal_outcome_under_standing_cancel(
    output: ProcessAwaitOutput,
    cancel_request: Option<&lash_core::CancelRequest>,
) -> ProcessAwaitOutput {
    let Some(origin) = cancel_request.map(|request| request.origin) else {
        return output;
    };
    match output {
        ProcessAwaitOutput::Settled { mut output } => {
            if let crate::ToolCallOutcome::Cancelled(cancellation) = &mut output.outcome {
                cancellation.origin = Some(origin);
            }
            ProcessAwaitOutput::Settled { output }
        }
        other => other,
    }
}

pub(super) async fn assert_model_agreement(
    handles: &StoreContractHandles,
    model: &ReferenceModel,
) -> Result<(), String> {
    for (id, expected) in &model.processes {
        if expected.tombstoned {
            if matches!(handles.registry.get_process(id).await, Ok(Some(_))) {
                return Err(format!(
                    "tombstoned process `{id}` unexpectedly became live"
                ));
            }
            continue;
        }
        let Some(expected_record) = expected.expected_record.clone() else {
            continue;
        };
        let actual_record = handles
            .registry
            .get_process(id)
            .await
            .map_err(|error| format!("modeled live process `{id}` lookup failed: {error}"))?
            .ok_or_else(|| format!("modeled live process `{id}` was absent"))?;
        let expected_record = normalize_record(expected_record);
        let actual_record = normalize_record(actual_record);
        if expected_record != actual_record {
            let fields = record_field_diff(&expected_record, &actual_record);
            return Err(format!(
                "process record for `{id}` differs from the independently-derived reference model: {fields}"
            ));
        }
        let actual = handles
            .registry
            .observers_for_process(id)
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .collect::<BTreeSet<_>>();
        if actual != expected.observers {
            return Err(format!(
                "observer set for `{id}` differs from reference model"
            ));
        }
    }
    let mut actual_deliveries = handles
        .registry
        .list_wake_deliveries(None)
        .await
        .map_err(|error| error.to_string())?;
    actual_deliveries.sort_by(|left, right| left.delivery_id.cmp(&right.delivery_id));
    let expected_deliveries = model.wake_deliveries.values().cloned().collect::<Vec<_>>();
    if actual_deliveries != expected_deliveries {
        return Err(format!(
            "wake delivery states differ from reference model: actual={actual_deliveries:?}, expected={expected_deliveries:?}"
        ));
    }
    let queued = handles
        .runtime
        .list_queued_work(&SessionId::from("prop-runtime-session"))
        .await
        .map_err(|error| error.to_string())?;
    let mut actual_live =
        BTreeMap::<(SessionId, ProcessId), BTreeMap<u64, ExpectedQueuedWake>>::new();
    for batch in queued {
        for item in batch.items {
            if let QueuedWorkPayload::ProcessWake { wake } = item.payload {
                actual_live
                    .entry((batch.session_id.clone(), wake.process_id.clone()))
                    .or_default()
                    .insert(
                        wake.sequence,
                        ExpectedQueuedWake {
                            wake: *wake,
                            delivery_policy: batch.delivery_policy,
                            kind: batch.kind,
                            authority: batch.authority.clone(),
                            merge_key: batch.merge_key.clone(),
                            available_at_ms: batch.available_at_ms,
                        },
                    );
            }
        }
    }
    let expected_live = model
        .live_wakes
        .iter()
        .filter(|(_, wakes)| !wakes.is_empty())
        .map(|(key, wakes)| (key.clone(), wakes.clone()))
        .collect::<BTreeMap<_, _>>();
    if actual_live != expected_live {
        return Err(format!(
            "Enqueued-wake high-water safety: live wake payload/batch state differs; actual={actual_live:?}, expected={expected_live:?}"
        ));
    }
    Ok(())
}
