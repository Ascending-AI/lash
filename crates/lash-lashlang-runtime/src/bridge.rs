use lashlang::{ExecutionHostError, Value as LashlangValue};

use crate::LashlangHostError;

pub fn lashlang_value_to_json(
    value: &LashlangValue,
) -> Result<serde_json::Value, ExecutionHostError> {
    serde_json::to_value(value)
        .map_err(|source| LashlangHostError::SerializeValue { source }.into())
}

pub fn protocol_tool_reply_to_lashlang_value(
    reply: lash_core::facade_support::ToolInvocationReply,
) -> Result<LashlangValue, ExecutionHostError> {
    let output = reply.output;
    let is_success = output.is_success();
    let value = output.into_value_for_projection();
    if is_success {
        Ok(lashlang::from_json(value))
    } else {
        Err(LashlangHostError::ToolRejected {
            message: json_error_message(value),
        }
        .into())
    }
}

pub fn protocol_tool_output_to_lashlang_value(
    output: &lash_core::ToolCallOutput,
) -> Result<LashlangValue, ExecutionHostError> {
    let value = output.value_for_projection();
    if output.is_success() {
        Ok(lashlang::from_json(value))
    } else {
        Err(LashlangHostError::ToolRejected {
            message: json_error_message(value),
        }
        .into())
    }
}

pub fn process_event_payload(
    value: &LashlangValue,
) -> Result<serde_json::Value, ExecutionHostError> {
    Ok(serde_json::json!({
        "value": lashlang_value_to_json(value)?,
        "text": value.to_string(),
    }))
}

pub fn sleep_duration_ms(
    kind: lashlang::SleepKind,
    value: &LashlangValue,
) -> Result<u64, ExecutionHostError> {
    match kind {
        lashlang::SleepKind::For => duration_value_ms(value),
        lashlang::SleepKind::Until => {
            let target = deadline_value_ms(value)?;
            let now = chrono::Utc::now().timestamp_millis();
            Ok(target.saturating_sub(now.max(0) as u64))
        }
    }
}

fn duration_value_ms(value: &LashlangValue) -> Result<u64, ExecutionHostError> {
    match value {
        LashlangValue::Number(value) if value.is_finite() && *value >= 0.0 => {
            Ok(value.round() as u64)
        }
        LashlangValue::String(value) => parse_duration_ms(value),
        other => Err(LashlangHostError::InvalidSleepDuration {
            actual: other.to_string(),
        }
        .into()),
    }
}

fn deadline_value_ms(value: &LashlangValue) -> Result<u64, ExecutionHostError> {
    match value {
        LashlangValue::Number(value) if value.is_finite() && *value >= 0.0 => {
            Ok(value.round() as u64)
        }
        LashlangValue::String(value) => chrono::DateTime::parse_from_rfc3339(value)
            .map(|deadline| deadline.timestamp_millis().max(0) as u64)
            .map_err(|source| LashlangHostError::InvalidSleepDeadline { source }.into()),
        other => Err(LashlangHostError::InvalidSleepDeadlineValue {
            actual: other.to_string(),
        }
        .into()),
    }
}

fn parse_duration_ms(value: &str) -> Result<u64, ExecutionHostError> {
    let value = value.trim();
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1.0)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000.0)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60_000.0)
    } else if let Some(number) = value.strip_suffix('h') {
        (number, 3_600_000.0)
    } else {
        (value, 1.0)
    };
    let parsed = number.trim().parse::<f64>().map_err(|source| {
        let error: ExecutionHostError = LashlangHostError::InvalidDurationNumber {
            value: value.to_string(),
            source,
        }
        .into();
        error
    })?;
    if !parsed.is_finite() || parsed < 0.0 {
        return Err(LashlangHostError::InvalidDurationValue {
            value: value.to_string(),
        }
        .into());
    }
    Ok((parsed * multiplier).round() as u64)
}

pub fn json_error_message(value: serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text,
        other => other.to_string(),
    }
}
