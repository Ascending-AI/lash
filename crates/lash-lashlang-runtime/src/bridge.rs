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
    if let lash_core::ToolCallOutcome::Failure(failure) = &output.outcome {
        return Err(ExecutionHostError::from_tool_failure(failure));
    }
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
    if let lash_core::ToolCallOutcome::Failure(failure) = &output.outcome {
        return Err(ExecutionHostError::from_tool_failure(failure));
    }
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

/// Resolves a guest sleep into a durable intent without sampling the clock.
///
/// `until` keeps its absolute deadline; the effect seam derives the wait from
/// the substrate clock, so the journaled envelope is replay-stable even though
/// the remaining duration shrinks between attempts (FIG-2968).
pub fn process_sleep(
    kind: lashlang::SleepKind,
    value: &LashlangValue,
) -> Result<lash_core::SleepSpec, ExecutionHostError> {
    match kind {
        lashlang::SleepKind::For => Ok(lash_core::SleepSpec::For {
            duration_ms: duration_value_ms(value)?,
        }),
        lashlang::SleepKind::Until => Ok(lash_core::SleepSpec::Until {
            deadline_ms: deadline_value_ms(value)?,
        }),
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

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::{
        AttachmentSource, MediaType, ToolCallOutput, ToolFailure, ToolFailureClass,
        ToolFailureSource, ToolRetryStatus, ToolValue, facade_support::ToolInvocationReply,
    };
    use std::collections::BTreeMap;

    fn policy_failure(retry: ToolRetryStatus) -> ToolFailure {
        ToolFailure {
            class: ToolFailureClass::PermissionDenied,
            code: "approval_denied".to_string(),
            message: "approval was denied".to_string(),
            source: ToolFailureSource::Policy,
            retry,
            raw: None,
        }
    }

    #[test]
    fn both_tool_bridges_preserve_structured_failure_fields() {
        let borrowed = ToolCallOutput::failure(policy_failure(ToolRetryStatus::Safe {
            after_ms: Some(1_250),
        }));
        let borrowed_error = protocol_tool_output_to_lashlang_value(&borrowed)
            .expect_err("a failed output must remain an execution-host error");
        assert_eq!(
            serde_json::to_value(borrowed_error).expect("execution-host error serializes"),
            serde_json::json!({
                "message": "approval was denied",
                "tool_failure": {
                    "class": "permission_denied",
                    "code": "approval_denied",
                    "source": "policy",
                    "retry": { "type": "safe", "after_ms": 1_250 }
                }
            })
        );

        let reply = ToolInvocationReply::from_output(ToolCallOutput::failure(policy_failure(
            ToolRetryStatus::Exhausted { attempts: 3 },
        )));
        let owned_error = protocol_tool_reply_to_lashlang_value(reply)
            .expect_err("a failed reply must remain an execution-host error");
        assert_eq!(
            serde_json::to_value(owned_error).expect("execution-host error serializes"),
            serde_json::json!({
                "message": "approval was denied",
                "tool_failure": {
                    "class": "permission_denied",
                    "code": "approval_denied",
                    "source": "policy",
                    "retry": { "type": "exhausted", "attempts": 3 }
                }
            })
        );
    }

    #[test]
    fn successful_tool_bridges_keep_scalar_record_and_attachment_projections() {
        let owned = protocol_tool_reply_to_lashlang_value(ToolInvocationReply::success(
            serde_json::json!("ok"),
        ))
        .expect("a successful scalar reply projects");
        assert_eq!(owned, LashlangValue::String("ok".into()));

        let mut record = BTreeMap::new();
        record.insert(
            "count".to_string(),
            ToolValue::Number(serde_json::Number::from(2)),
        );
        record.insert(
            "attachment".to_string(),
            ToolValue::Attachment(AttachmentSource::external_url(
                MediaType::parse("image/png").unwrap(),
                "https://example.test/image.png",
            )),
        );
        let output = ToolCallOutput::success_tool_value(ToolValue::Object(record));
        let expected = output.value_for_projection();
        let borrowed = protocol_tool_output_to_lashlang_value(&output)
            .expect("a successful record reply projects");
        assert_eq!(
            lashlang_value_to_json(&borrowed).expect("projected Lashlang value serializes"),
            expected
        );
    }

    #[test]
    fn sleep_for_resolves_to_a_relative_duration() {
        let value = LashlangValue::Number(1_500.0);
        assert_eq!(
            process_sleep(lashlang::SleepKind::For, &value).expect("a numeric duration resolves"),
            lash_core::SleepSpec::For { duration_ms: 1_500 }
        );
    }

    #[test]
    fn sleep_until_keeps_the_absolute_deadline_and_never_samples_the_clock() {
        // A deadline at a fixed absolute instant. `process_sleep` must return
        // the deadline itself: the rejected implementation subtracted
        // `Utc::now()` here, so two calls a moment apart produced different
        // durations and the journaled envelope failed its own replay fence
        // (FIG-2968).
        let deadline_ms = 1_800_000_000_000_u64;
        let value = LashlangValue::Number(deadline_ms as f64);
        let first = process_sleep(lashlang::SleepKind::Until, &value).expect("deadline resolves");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = process_sleep(lashlang::SleepKind::Until, &value).expect("deadline resolves");
        assert_eq!(first, second);
        assert_eq!(first, lash_core::SleepSpec::Until { deadline_ms });
    }

    #[test]
    fn sleep_until_parses_an_absolute_rfc3339_deadline() {
        let value = LashlangValue::String("2030-01-01T00:00:00Z".into());
        match process_sleep(lashlang::SleepKind::Until, &value).expect("rfc3339 deadline resolves")
        {
            lash_core::SleepSpec::Until { deadline_ms } => {
                assert_eq!(deadline_ms, 1_893_456_000_000);
            }
            other => panic!("expected an absolute deadline, got {other:?}"),
        }
    }
}
