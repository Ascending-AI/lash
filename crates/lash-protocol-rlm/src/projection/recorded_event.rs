//! The durable event representation shares its fixed-width readings with the host root.

use lash_protocol_types::{
    AttachmentRef, BindingChanges, CellOutcome, CellPrint, CellRecord, ExecutedCall, RetainedValue,
};
use lash_rlm_types::{
    RlmDiagnosticEvent, RlmGlobalsPatchPluginBody, RlmProtocolEvent, RlmSeedPluginBody,
};
use serde::{Deserializer, Serializer};
use std::sync::Arc;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(remote = "RlmProtocolEvent")]
#[expect(
    clippy::enum_variant_names,
    reason = "serde remote derive must match the public RlmProtocolEvent variant names"
)]
enum Event {
    RlmTrajectoryEntry(#[serde(with = "cell")] Arc<CellRecord>),
    RlmGlobalsPatch(RlmGlobalsPatchPluginBody),
    RlmSeed(RlmSeedPluginBody),
    RlmDiagnostic(RlmDiagnosticEvent),
}

pub(super) fn serialize<S: Serializer>(
    event: &RlmProtocolEvent,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    Event::serialize(event, serializer)
}

pub(super) fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<RlmProtocolEvent, D::Error> {
    Event::deserialize(deserializer)
}

mod cell {
    use super::*;

    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(remote = "CellRecord", deny_unknown_fields)]
    struct Body {
        #[serde(with = "crate::consumption_record")]
        consumption: Vec<lash_protocol_types::CellConsumption>,
        id: String,
        protocol_iteration: usize,
        language: String,
        code: String,
        prints: Vec<CellPrint>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prints_retained: Option<RetainedValue>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<AttachmentRef>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        calls: Vec<ExecutedCall>,
        #[serde(default, skip_serializing_if = "BindingChanges::is_empty")]
        bindings: BindingChanges,
        result: CellOutcome,
    }

    pub(super) fn serialize<S: Serializer>(
        cell: &Arc<CellRecord>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        Body::serialize(cell, serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Arc<CellRecord>, D::Error> {
        Body::deserialize(deserializer).map(Arc::new)
    }
}
