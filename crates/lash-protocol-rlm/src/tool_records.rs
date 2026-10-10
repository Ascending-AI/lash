//! Shared bounded transcript projection for the cell and native channels.
use lash_core::{
    OmittedToolCalls, ToolCallOutcome, ToolCallOutput, ToolCallRecord, ToolControl, ToolFailure,
    ToolValue, TurnControl,
};
use std::collections::BTreeMap;

pub(crate) fn bounded_exec_tool_call_records(
    records: &[ToolCallRecord],
    config: &crate::RlmPresentationConfig,
) -> (Vec<ToolCallRecord>, Option<OmittedToolCalls>) {
    // HostBridge supplies execution-index order, so concurrent dispatch keeps a
    // deterministic host-record order and configured retention boundary.
    let retained_count = records.len().min(config.max_tool_call_records);
    let bounded = records[..retained_count]
        .iter()
        .map(|record| bounded_tool_call_record(record, config))
        .collect::<Vec<_>>();
    let omitted = &records[retained_count..];
    let summary = (!omitted.is_empty()).then(|| OmittedToolCalls {
        count: omitted.len(),
        failures: omitted
            .iter()
            .filter(|record| !record.output.is_success())
            .count(),
        attachments: omitted
            .iter()
            .flat_map(|record| tool_output_attachments(&record.output))
            .collect(),
    });
    (bounded, summary)
}

/// The cell entry's executed calls: the diagnostic tail of what the cell
/// ran, and how many earlier calls it leaves out.
pub(crate) fn bounded_executed_calls(
    mut calls: Vec<lash_core::ExecutedCall>,
    config: &crate::RlmPresentationConfig,
) -> (Vec<lash_core::ExecutedCall>, usize) {
    let omitted = calls.len().saturating_sub(config.max_tool_call_records);
    calls.drain(..omitted);
    (calls, omitted)
}

pub(crate) fn bounded_tool_call_record(
    record: &ToolCallRecord,
    config: &crate::RlmPresentationConfig,
) -> ToolCallRecord {
    ToolCallRecord {
        call_id: record.call_id.clone(),
        provider_call_id: record.provider_call_id.clone(),
        tool: record.tool.clone(),
        args: record.args.clone(),
        output: bounded_tool_call_output(&record.output, config),
    }
}

fn bounded_tool_call_output(
    output: &ToolCallOutput,
    config: &crate::RlmPresentationConfig,
) -> ToolCallOutput {
    let outcome = match &output.outcome {
        ToolCallOutcome::Success(value) => {
            ToolCallOutcome::Success(bounded_tool_value(value, config))
        }
        ToolCallOutcome::Failure(failure) => {
            ToolCallOutcome::Failure(bounded_tool_failure(failure, config))
        }
        ToolCallOutcome::Cancelled(cancellation) => {
            let mut bounded = cancellation.clone();
            bounded.raw = bounded
                .raw
                .as_ref()
                .map(|value| bounded_tool_value(value, config));
            ToolCallOutcome::Cancelled(bounded)
        }
    };
    let control = output.control.as_ref().map(|control| match control {
        ToolControl::Turn {
            control: control @ TurnControl::SwitchAgentFrame { .. },
        } => ToolControl::Turn {
            control: control.clone(),
        },
        ToolControl::Turn {
            control: TurnControl::Finish { value },
        } => ToolControl::Turn {
            control: TurnControl::Finish {
                value: bounded_tool_value(value, config),
            },
        },
        ToolControl::Fail { failure } => ToolControl::Fail {
            failure: bounded_tool_failure(failure, config),
        },
        ToolControl::AbortRun { code, message } => ToolControl::AbortRun {
            code: code.clone(),
            message: message.clone(),
        },
    });
    ToolCallOutput {
        outcome,
        control,
        view: None,
        projection_value: None,
    }
}

fn bounded_tool_failure(
    failure: &ToolFailure,
    config: &crate::RlmPresentationConfig,
) -> ToolFailure {
    let mut bounded = failure.clone();
    bounded.raw = bounded
        .raw
        .as_ref()
        .map(|value| bounded_tool_value(value, config));
    bounded
}

fn bounded_tool_value(value: &ToolValue, config: &crate::RlmPresentationConfig) -> ToolValue {
    match value {
        ToolValue::String(value) if value.len() > config.max_inline_scalar_bytes => {
            omitted_bytes_marker(value.len())
        }
        ToolValue::Array(values) => ToolValue::Array(
            values
                .iter()
                .map(|value| bounded_tool_value(value, config))
                .collect(),
        ),
        ToolValue::Object(entries) => ToolValue::Object(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), bounded_tool_value(value, config)))
                .collect(),
        ),
        ToolValue::UntrustedJson(value) => {
            ToolValue::untrusted_json(bounded_untrusted_json(value, config))
        }
        ToolValue::Null
        | ToolValue::Bool(_)
        | ToolValue::Number(_)
        | ToolValue::String(_)
        | ToolValue::Attachment(_) => value.clone(),
    }
}

fn bounded_untrusted_json(
    value: &serde_json::Value,
    config: &crate::RlmPresentationConfig,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(value) if value.len() > config.max_inline_scalar_bytes => {
            serde_json::json!({ "omitted_bytes": value.len() })
        }
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .iter()
                .map(|value| bounded_untrusted_json(value, config))
                .collect(),
        ),
        serde_json::Value::Object(entries) => serde_json::Value::Object(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), bounded_untrusted_json(value, config)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn omitted_bytes_marker(omitted_bytes: usize) -> ToolValue {
    ToolValue::Object(BTreeMap::from([(
        "omitted_bytes".to_string(),
        ToolValue::untrusted_json(serde_json::json!(omitted_bytes)),
    )]))
}

fn tool_output_attachments(output: &ToolCallOutput) -> Vec<lash_core::AttachmentRef> {
    let mut attachments = output.attachments();
    match output.control.as_ref() {
        Some(ToolControl::Turn {
            control: TurnControl::Finish { value },
        }) => attachments.extend(value.attachments()),
        Some(ToolControl::Fail { failure }) => attachments.extend(
            failure
                .raw
                .as_ref()
                .map(ToolValue::attachments)
                .unwrap_or_default(),
        ),
        Some(
            ToolControl::Turn {
                control: TurnControl::SwitchAgentFrame { .. },
            }
            | ToolControl::AbortRun { .. },
        )
        | None => {}
    }
    attachments
}
