use lash::{TurnActivity, TurnActivitySink, TurnEvent};
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub(crate) struct Telemetry {
    activities: Mutex<Vec<TurnActivity>>,
    costs: Mutex<Vec<Option<Value>>>,
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
    pub(crate) fn rows(&self, decisions: &[String]) -> Vec<Value> {
        let costs = self.costs.lock().unwrap_or_else(|error| error.into_inner());
        let activities = self.activities();
        activities.iter().filter_map(|activity| match &activity.event { TurnEvent::ModelCallRecorded { record } => Some(record), _ => None }).enumerate().flat_map(|(index, record)| {
            let costs = &costs;
            record.attempts.iter().map(move |attempt| {
                let completed = matches!(attempt.outcome, lash::provider::AttemptOutcome::Completed);
                serde_json::json!({
                    "call_index":index, "attempt_index":attempt.ordinal,
                    "decision": if completed { decisions.get(index).map(String::as_str).unwrap_or("empty_or_unclassified") } else { "transport_retry_or_failure" },
                    "tokens":attempt.usage.as_ref().map(|usage| serde_json::json!({"input":usage.input_tokens,"output":usage.output_tokens,"cache_read":usage.cache_read_input_tokens,"cache_write":usage.cache_write_input_tokens})),
                    "wall_ms":attempt.duration.as_millis(),
                    "cost": if completed { costs.get(index).cloned().flatten() } else { None },
                    "evidence":attempt.evidence, "outcome":attempt.outcome,
                })
            })
        }).collect()
    }
}
