use lash::{TurnActivity, TurnActivitySink, TurnEvent};
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub(crate) struct Telemetry {
    activities: Mutex<Vec<TurnActivity>>,
    costs: Mutex<Vec<Option<Value>>>,
    submit_calls: std::sync::atomic::AtomicUsize,
}
#[async_trait::async_trait]
impl TurnActivitySink for Telemetry {
    async fn emit(&self, activity: TurnActivity) {
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
        let count = parts.iter().filter(|part| matches!(part, lash::direct::LlmOutputPart::ToolCall { tool_name, .. } if tool_name == "submit")).count();
        self.submit_calls
            .fetch_add(count, std::sync::atomic::Ordering::Relaxed);
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
                    let cost = ctx
                        .response
                        .provider_usage
                        .as_ref()
                        .and_then(|usage| usage.get("cost"))
                        .filter(|value| value.is_number())
                        .cloned();
                    telemetry
                        .costs
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push(cost);
                    Ok(lash::plugins::AssistantResponseTransform {
                        response: ctx.response,
                        events: Vec::new(),
                    })
                })
            })),
        ))
    }
    pub(crate) fn rows(&self, decisions: &[String], standard: bool) -> Vec<Value> {
        let costs = self.costs.lock().unwrap_or_else(|error| error.into_inner());
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
            for attempt in &record.attempts {
                let completed =
                    matches!(attempt.outcome, lash::provider::AttemptOutcome::Completed);
                let mut row = serde_json::json!({
                    "call_index":index, "attempt_index":attempt.ordinal,
                    "decision": if completed { decisions.get(response_index).map(String::as_str).unwrap_or(if standard { "standard_tools_or_prose" } else { "empty_or_unclassified" }) } else { "transport_retry_or_failure" },
                    "tokens":attempt.usage.as_ref().map(|usage| serde_json::json!({"input":usage.input_tokens,"output":usage.output_tokens,"cache_read":usage.cache_read_input_tokens,"cache_write":usage.cache_write_input_tokens})),
                    "wall_ms":attempt.duration.as_millis(),
                    "cost": if completed { costs.get(response_index).cloned().flatten() } else { None },
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
            // Cancellation can drop the provider future before its ledger is
            // sealed. Preserve the unknown attempt instead of presenting the
            // preceding calls' partial token/cost totals as a complete bill.
            let mut row = serde_json::json!({
                "call_index": calls.len(), "attempt_index": null,
                "decision": "interrupted_provider_call", "tokens": null,
                "wall_ms": null, "cost": null, "cost_unknown": true,
                "evidence": null, "outcome": "interrupted",
            });
            row.as_object_mut().expect("attempt object").extend(
                execution_fields(&[], standard)
                    .as_object()
                    .expect("execution object")
                    .clone(),
            );
            rows.push(row);
        }
        rows
    }
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
