use lash::{TurnActivity, TurnActivitySink, TurnEvent};
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub(crate) struct Telemetry {
    activities: Mutex<Vec<TurnActivity>>,
    pub(crate) capture: crate::provider_log::Capture,
    submit_values: Mutex<Vec<Option<Value>>>,
    submit_calls: std::sync::atomic::AtomicUsize,
}
#[async_trait::async_trait]
impl TurnActivitySink for Telemetry {
    async fn emit(&self, activity: TurnActivity) {
        tracing::debug!(target: "toolbench", parent: &self.capture.span(), activity = %self.capture.redact(serde_json::to_value(&activity).expect("activity serializes")), "turn activity");
        self.activities
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(activity);
    }
}
impl Telemetry {
    pub(crate) fn submit_count(&self) -> usize {
        self.submit_calls.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn record_submits(&self, parts: &[lash::direct::LlmOutputPart]) {
        let values = parts
            .iter()
            .filter_map(|part| {
                if let lash::direct::LlmOutputPart::ToolCall {
                    tool_name,
                    input_json,
                    ..
                } = part
                    && tool_name == "submit"
                {
                    Some(
                        serde_json::from_str::<Value>(input_json)
                            .ok()
                            .and_then(|args| args.get("value").cloned()),
                    )
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        self.submit_calls
            .fetch_add(values.len(), std::sync::atomic::Ordering::Relaxed);
        self.submit_values
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(values);
    }
    pub(crate) fn submit_values(&self) -> Vec<Option<Value>> {
        self.submit_values
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn activities(&self) -> Vec<TurnActivity> {
        self.activities
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    pub(crate) fn plugin(self: &Arc<Self>) -> Arc<lash::plugins::StaticPluginFactory> {
        let telemetry = Arc::clone(self);
        Arc::new(lash::plugins::StaticPluginFactory::new(
            "toolbench_telemetry",
            lash::plugins::PluginSpec::new().with_assistant_response(Arc::new(move |ctx| {
                let telemetry = Arc::clone(&telemetry);
                Box::pin(async move {
                    telemetry.record_submits(&ctx.response.parts);
                    Ok(lash::plugins::AssistantResponseTransform {
                        response: ctx.response,
                        events: Vec::new(),
                    })
                })
            })),
        ))
    }
    pub(crate) fn rows(&self, decisions: &[String], standard: bool) -> Vec<Value> {
        let captured = self.capture.rows();
        let activities = self.activities();
        let calls = activities
            .iter()
            .enumerate()
            .filter_map(|(offset, activity)| match &activity.event {
                TurnEvent::ModelCallRecorded { record } => Some((offset, record)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut rows = Vec::new();
        let mut response_index = 0;
        for (index, (offset, record)) in calls.iter().enumerate() {
            let end = calls
                .get(index + 1)
                .map_or(activities.len(), |(offset, _)| *offset);
            let execution = execution_fields(&activities[offset + 1..end], standard);
            for (attempt_offset, attempt) in record.attempts.iter().enumerate() {
                let transport = captured.get(rows.len());
                let completed =
                    matches!(attempt.outcome, lash::provider::AttemptOutcome::Completed);
                let mut row = serde_json::json!({
                    "call_index":index, "attempt_index":attempt.ordinal,
                    "decision": if completed { decisions.get(response_index).map(String::as_str).unwrap_or(if standard { "standard_tools_or_prose" } else { "empty_or_unclassified" }) } else { "transport_retry_or_failure" },
                    "tokens":attempt.usage.as_ref().map(|usage| serde_json::json!({"input":usage.input_tokens,"output":usage.output_tokens,"cache_read":usage.cache_read_input_tokens,"cache_write":usage.cache_write_input_tokens})),
                    "wall_ms":attempt.duration.as_millis(),
                    "cost": transport.map(|r| r["cost"].clone()).unwrap_or_else(|| serde_json::json!(0)),
                    "request_ms":transport.and_then(|r| r.get("request_ms")),
                    "retry_decision":attempt.retry_decision, "protocol_position":attempt.protocol_position,
                    "retry_budget_consumed":attempt.retry_budget_consumed,
                    "retries":attempt_offset, "is_retry":attempt_offset > 0,
                    "error": transport.and_then(|r| r.get("error")).filter(|e| !e.is_null()).cloned().or_else(|| {
                        if completed { None } else {
                            let e = attempt.error.as_ref();
                            Some(serde_json::json!({"kind":e.map(|e| e.class.as_str()).unwrap_or("provider_failure"), "status":e.and_then(|e| e.http_status), "message":e.and_then(|e| e.diagnostic.as_deref()).map(str::to_owned).unwrap_or_else(|| format!("{:?}",attempt.outcome)), "body_excerpt":e.and_then(|e| e.diagnostic.as_deref()), "provider_request_id":e.and_then(|e| e.provider_request_id.as_deref()), "provider_response_id":attempt.evidence.as_ref().and_then(|e| e.provider_response_id.as_deref()), "retry_after":e.and_then(|e| e.retry_after)}))
                        }
                    }),
                    "normalized_error":attempt.error,
                    "evidence":attempt.evidence, "outcome":attempt.outcome,
                });
                row["cost_unknown"] = row["cost"].is_null().into();
                let fields = if completed {
                    execution.clone()
                } else {
                    execution_fields(&[], standard)
                };
                row.as_object_mut()
                    .expect("attempt object")
                    .extend(fields.as_object().expect("execution object").clone());
                rows.push(row);
                if completed {
                    response_index += 1;
                }
            }
        }
        if has_unsealed_request(&activities) {
            // A deadline may interrupt a request OR the backoff between requests.
            // Preserve every transport invocation already observed, with costs,
            // even though Lash has not returned the logical call's ledger yet.
            let unsealed = &captured[rows.len().min(captured.len())..];
            for (offset, transport) in unsealed.iter().enumerate() {
                let mut row = serde_json::json!({
                    "call_index":calls.len(), "attempt_index":offset + 1,
                    "decision":"unsealed_provider_call", "tokens":transport["response"].get("usage").map(|u| serde_json::json!({"input":u["input_tokens"],"output":u["output_tokens"],"cache_read":u["cache_read_input_tokens"],"cache_write":u["cache_write_input_tokens"]})),
                    "request_ms":transport["request_ms"], "wall_ms":null,
                    "cost":transport["cost"], "cost_unknown":transport["cost"].is_null(),
                    "evidence":transport["response"]["execution_evidence"], "outcome":"interrupted",
                    "error":transport["error"].as_object().map(|e| Value::Object(e.clone())).unwrap_or_else(interrupted_error),
                    "retry_decision":null, "retry_decision_unavailable":"call interrupted before ledger sealed",
                    "retries":offset, "is_retry":offset > 0,
                });
                row.as_object_mut()
                    .unwrap()
                    .extend(execution_fields(&[], standard).as_object().unwrap().clone());
                rows.push(row);
            }
        }
        for (index, row) in rows.iter_mut().enumerate() {
            let transport = captured.get(index).cloned().unwrap_or(Value::Null);
            let usage = crate::accounting::Usage::from_raw(&transport["raw_usage"]);
            row.as_object_mut().unwrap().extend(
                serde_json::to_value(usage)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .clone(),
            );
            row["round"] = (index + 1).into();
            row["turn"] = 1.into();
            row["protocol_round"] = row["call_index"].as_u64().map(|n| n + 1).into();
            row["raw_usage"] = transport["raw_usage"].clone();
            row["provider_response_id"] = row["evidence"]["provider_response_id"].clone();
            if row["provider_response_id"].is_null() {
                row["provider_response_id"] =
                    transport["response"]["execution_evidence"]["provider_response_id"].clone();
            }
            if let Some(sizes) = transport["request_sizes"].as_object() {
                row.as_object_mut().unwrap().extend(sizes.clone());
            } else {
                for key in [
                    "system_prompt_chars",
                    "system_prompt_bytes",
                    "messages_chars",
                    "messages_bytes",
                    "tool_result_chars",
                    "tool_result_bytes",
                    "tool_definitions_count",
                    "tool_definitions_chars",
                    "tool_definitions_bytes",
                ] {
                    row[key] = Value::Null;
                }
            }
            if row.get("code").is_none() {
                row["code"] = Value::Null;
            }
            if row.get("tool_calls").is_none() {
                row["tool_calls"] = serde_json::json!([]);
            }
            if row["provider_response_id"].is_null() {
                row["provider_response_id"] = transport["wire_responses"]
                    .as_array()
                    .and_then(|chunks| chunks.iter().rev().find_map(|v| v.get("id").cloned()))
                    .unwrap_or(Value::Null);
            }
            if row["provider_response_id"].is_null() {
                row["provider_response_id"] = row["error"]["provider_response_id"].clone();
            }
            if row["provider_response_id"].is_null() {
                row["provider_response_id"] = transport["http_response_json"]["id"].clone();
            }
            row["cost_unknown"] = row["cost_usd"].is_null().into();
            row["capture_errors"] = serde_json::json!(*self.capture.dump_errors.lock().unwrap());
        }
        rows.into_iter()
            .map(|row| self.capture.redact(row))
            .collect()
    }
}

fn interrupted_error() -> Value {
    serde_json::json!({"kind":"interrupted", "status":null, "message":"provider call interrupted before ledger sealed", "body_excerpt":null,"provider_request_id":null,"provider_response_id":null,"retry_after":null})
}

fn has_unsealed_request(activities: &[TurnActivity]) -> bool {
    activities
        .iter()
        .rev()
        .find_map(|activity| match activity.event {
            TurnEvent::ModelRequestStarted { .. } => Some(true),
            TurnEvent::ModelCallRecorded { .. } => Some(false),
            _ => None,
        })
        .unwrap_or(false)
}

// ModelCallRecorded precedes execution; only the completed provider attempt
// owns the activities before the next call. Retries never inherit its cell.
fn execution_fields(activities: &[TurnActivity], standard: bool) -> Value {
    let mut code = Vec::new();
    let mut observations = Vec::new();
    let mut tool_calls = Vec::new();
    for activity in activities {
        match &activity.event {
            TurnEvent::CodeBlockStarted { code: source, .. } if !standard => {
                code.push(source.as_str())
            }
            TurnEvent::CodeBlockCompleted { output, error, .. } if !standard => {
                observations.push(output.clone());
                if let Some(error) = error {
                    // Runtime errors can be separate from the rendered stdout.
                    if !output.contains(&error.message) {
                        observations.push(error.message.clone());
                    }
                }
            }
            TurnEvent::ToolCallStarted { name, args, .. } if standard => {
                tool_calls.push(serde_json::json!({"name":name,"arguments":args}));
            }
            TurnEvent::ToolCallCompleted { output, .. } if standard => {
                let value = output.value_for_projection();
                observations.push(match value {
                    Value::String(text) => text,
                    value => value.to_string(),
                });
            }
            _ => {}
        }
    }
    let observation = observations.join("\n");
    let mut chars = observation.chars();
    let rendered = chars.by_ref().take(2_000).collect::<String>();
    let mut fields = serde_json::json!({
        "observation": if observations.is_empty() { None } else { Some(rendered) },
        "observation_truncated": chars.next().is_some(),
    });
    if standard {
        fields["tool_calls"] = tool_calls.into();
    } else {
        fields["code"] = if code.is_empty() {
            Value::Null
        } else {
            code.join("\n").into()
        };
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    fn activity(event: TurnEvent) -> TurnActivity {
        TurnActivity::new(lash::TurnActivityId::new("test"), event)
    }

    #[test]
    fn cells_keep_source_and_unicode_observations_with_explicit_truncation() {
        let source = "let x = kv.get({key: \"project\"}); x";
        let fields = execution_fields(
            &[
                activity(TurnEvent::CodeBlockStarted {
                    language: "lashlang".into(),
                    code: source.into(),
                    graph_key: None,
                }),
                activity(TurnEvent::CodeBlockCompleted {
                    language: "lashlang".into(),
                    output: "é".repeat(2_001),
                    error: None,
                    success: true,
                    duration_ms: 1,
                    tool_call_ids: vec![],
                    graph_key: None,
                }),
            ],
            false,
        );
        assert_eq!(fields["code"], source);
        assert_eq!(
            fields["observation"].as_str().unwrap().chars().count(),
            2_000
        );
        assert_eq!(fields["observation_truncated"], true);
        let empty = execution_fields(&[], false);
        assert!(empty["code"].is_null());
        assert!(empty["observation"].is_null());
        assert_eq!(empty["observation_truncated"], false);
    }

    #[test]
    fn standard_records_actual_tool_arguments_instead_of_code() {
        let fields = execution_fields(
            &[activity(TurnEvent::ToolCallStarted {
                call_id: Some("call".into()),
                name: "kv_get".into(),
                args: serde_json::json!({"key":"project"}),
                graph_key: None,
                parent_call_id: None,
            })],
            true,
        );
        assert!(fields.get("code").is_none());
        assert_eq!(
            fields["tool_calls"],
            serde_json::json!([{"name":"kv_get","arguments":{"key":"project"}}])
        );
    }

    #[test]
    fn interrupted_provider_call_keeps_cost_unknown() {
        let telemetry = Telemetry::default();
        telemetry
            .activities
            .lock()
            .unwrap()
            .push(activity(TurnEvent::ModelRequestStarted {
                protocol_iteration: 1,
            }));
        telemetry
            .capture
            .entries
            .lock()
            .unwrap()
            .push(serde_json::json!({"cost":null}));
        let rows = telemetry.rows(&[], false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["outcome"], "interrupted");
        assert_eq!(rows[0]["cost_unknown"], true);
        assert!(rows[0]["code"].is_null());
        assert_eq!(crate::summary::Usage::from_attempts(&rows).cost, None);
    }

    #[test]
    fn duplicate_submit_is_counted_before_argument_or_id_validation() {
        let telemetry = Telemetry::default();
        let call = lash::direct::LlmOutputPart::ToolCall {
            call_id: "duplicate".into(),
            tool_name: "submit".into(),
            input_json: "{}".into(),
            replay: None,
        };
        telemetry.record_submits(&[call.clone(), call]);
        assert_eq!(telemetry.submit_count(), 2);
    }
}
