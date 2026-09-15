//! What a trigger occurrence reports: the source it carries, and the deliveries
//! it produced.

use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CronEmitReport {
    pub(crate) started_process_ids: Vec<ProcessId>,
    /// Every delivery the occurrence produced, so a reserved-then-refused
    /// delivery is visible in `cron.restate.emit_completed` instead of showing
    /// up only as an empty started list.
    #[serde(default)]
    pub(crate) deliveries: Vec<serde_json::Value>,
}

/// Every delivery a trigger occurrence produced, with the outcome and, for a
/// refusal, the reason.
///
/// `started_process_ids()` filters to `Started`, so tracing only that turned a
/// delivery the store had already reserved and then refused into silence: a
/// cron schedule registered without `tz` reserved a delivery on every tick and
/// started nothing, and no trace said so.
pub(crate) fn trigger_delivery_trace(
    report: &lash::triggers::TriggerEmitReport,
) -> serde_json::Value {
    json!(
        report
            .deliveries
            .iter()
            .map(|delivery| {
                let (outcome, reason) = match &delivery.outcome {
                    lash::triggers::TriggerDeliveryEmitOutcome::Started => ("started", None),
                    lash::triggers::TriggerDeliveryEmitOutcome::AlreadyReserved => {
                        ("already_reserved", None)
                    }
                    lash::triggers::TriggerDeliveryEmitOutcome::Failed { reason } => {
                        ("failed", Some(reason.clone()))
                    }
                };
                json!({
                    "subscription_id": delivery.subscription_id,
                    "process_id": delivery.process_id,
                    "outcome": outcome,
                    "reason": reason,
                })
            })
            .collect::<Vec<_>>()
    )
}

/// The occurrence source a cron tick reports, shaped to the source contract the
/// subscription captured at registration.
///
/// `tz` is optional in the `cron.Schedule` constructor, and the captured
/// contract types it as a string. Emitting `"tz": null` for a schedule
/// registered without one made every delivery fail the contract check in
/// `start_delivery` *after* the store had reserved it, so a cron trigger
/// without `tz` reserved a delivery on every tick and started nothing.
pub(crate) fn cron_occurrence_source(expr: &str, tz: Option<&String>) -> serde_json::Value {
    let mut source = serde_json::Map::new();
    source.insert("expr".to_string(), json!(expr));
    if let Some(tz) = tz {
        source.insert("tz".to_string(), json!(tz));
    }
    serde_json::Value::Object(source)
}
