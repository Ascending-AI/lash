use super::{
    TriggerOwnerScope, TriggerSourceCapture, TriggerSubscriptionLifecycle,
    TriggerSubscriptionRecord,
};
use serde::{Deserialize, Serialize};

/// Position in one trigger store's subscription change feed. Never compare
/// cursors issued by different stores.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct TriggerSubscriptionChangeCursor(u64);

impl TriggerSubscriptionChangeCursor {
    #[must_use]
    pub const fn initial() -> Self {
        Self(0)
    }
    #[must_use]
    pub const fn from_store_sequence(sequence: u64) -> Self {
        Self(sequence)
    }
    #[must_use]
    pub const fn store_sequence(self) -> u64 {
        self.0
    }
}

/// The latest desired source state for one subscription. A tombstoned
/// lifecycle means remove the source, including after retention removes the
/// subscription itself. Incarnation distinguishes revivals of the same id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerSubscriptionChange {
    pub subscription_id: String,
    pub incarnation: String,
    pub revision: u64,
    pub owner_scope: TriggerOwnerScope,
    pub lifecycle: TriggerSubscriptionLifecycle,
    pub source_type: String,
    pub source_key: String,
    pub source: serde_json::Value,
    pub source_capture: TriggerSourceCapture,
}

impl From<&TriggerSubscriptionRecord> for TriggerSubscriptionChange {
    fn from(record: &TriggerSubscriptionRecord) -> Self {
        Self {
            subscription_id: record.subscription_id.clone(),
            incarnation: record.incarnation.clone(),
            revision: record.revision,
            owner_scope: record.owner_scope.clone(),
            lifecycle: record.lifecycle,
            source_type: record.source_type.clone(),
            source_key: record.source_key.clone(),
            source: record.source.clone(),
            source_capture: record.source_capture.clone(),
        }
    }
}
