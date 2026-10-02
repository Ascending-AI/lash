use crate::ProcessId;
use serde::{Deserialize, Serialize};

/// How one delivery of an emitted occurrence ended.
///
/// The statement is the delivery's settled one, identical on the first
/// emission and on every replay of it: a replayed emission finds the
/// occurrence and its delivery already recorded and starts the same process
/// under the same journal key, so it reports `Started` as the first did
/// (FIG-4272). Whether a call coalesced onto an occurrence the store already
/// held is the call's own fact, reported beside the report by
/// [`crate::StoreRealization`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TriggerDeliveryEmitOutcome {
    Started {
        process_id: ProcessId,
    },
    /// A recorded refusal: typed `code` for classification, `reason` for diagnostics.
    Failed {
        code: crate::RuntimeErrorCode,
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerDeliveryEmitReceipt {
    pub occurrence_id: String,
    pub subscription_id: String,
    pub outcome: TriggerDeliveryEmitOutcome,
}

impl TriggerDeliveryEmitReceipt {
    /// The process named by a successfully started delivery.
    pub fn process_id(&self) -> Option<&ProcessId> {
        match &self.outcome {
            TriggerDeliveryEmitOutcome::Started { process_id } => Some(process_id),
            TriggerDeliveryEmitOutcome::Failed { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerEmitReport {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub occurrence_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deliveries: Vec<TriggerDeliveryEmitReceipt>,
}

impl TriggerEmitReport {
    pub fn empty() -> Self {
        Self::default()
    }

    pub(super) fn new(occurrence_id: String, deliveries: Vec<TriggerDeliveryEmitReceipt>) -> Self {
        Self {
            occurrence_id,
            deliveries,
        }
    }

    pub fn started_process_ids(&self) -> Vec<ProcessId> {
        self.deliveries
            .iter()
            .filter_map(|delivery| match &delivery.outcome {
                TriggerDeliveryEmitOutcome::Started { process_id } => Some(process_id.clone()),
                TriggerDeliveryEmitOutcome::Failed { .. } => None,
            })
            .collect()
    }
}
