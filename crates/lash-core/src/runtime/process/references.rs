use serde::{Deserialize, Serialize};

use super::model::{ProcessExecutionEnvRef, ProcessRecord};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessLiveReferenceSummary {
    pub definition: Option<serde_json::Value>,
    pub env_ref: Option<ProcessExecutionEnvRef>,
    pub process_count: usize,
}

impl ProcessLiveReferenceSummary {
    /// Builds a `ProcessLiveReferenceSummary` from records data for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn from_records<'record>(
        records: impl IntoIterator<Item = &'record ProcessRecord>,
    ) -> Vec<Self> {
        let mut summaries: Vec<Self> = Vec::new();
        for record in records {
            if record.is_terminal() {
                continue;
            }
            if let Some(summary) = summaries.iter_mut().find(|summary| {
                summary.definition == record.identity.definition
                    && summary.env_ref == record.env_ref
            }) {
                summary.process_count += 1;
            } else {
                summaries.push(Self {
                    definition: record.identity.definition.clone(),
                    env_ref: record.env_ref.clone(),
                    process_count: 1,
                });
            }
        }
        summaries.sort_by_key(live_reference_sort_key);
        summaries
    }
}

fn live_reference_sort_key(summary: &ProcessLiveReferenceSummary) -> (String, String) {
    let definition = summary
        .definition
        .as_ref()
        .map(|definition| serde_json::to_string(definition).expect("definition serializes"))
        .unwrap_or_default();
    let env_ref = summary
        .env_ref
        .as_ref()
        .map(|env_ref| env_ref.as_str().to_string())
        .unwrap_or_default();
    (definition, env_ref)
}
