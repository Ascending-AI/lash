//! The runtime-owned `process.ran` record (FIG-6370): the statement starts
//! one committed run segment of an observed process counted, and the
//! effect occurrences whose failure a `catch` in it recovered from
//! (FIG-6817).

use serde::{Deserialize, Serialize};

use super::effect_summary::{
    PROCESS_EVENT_VOCABULARY_VERSION, ProcessEffectReportError, admit_vocabulary_version,
    occurrence_schema, require_vocabulary_version, vocabulary_version_schema,
};
use super::events::ProcessEventAppendRequest;

/// Runtime-owned event recording one committed run segment's document
/// statement starts and empty-block entries, by document site ordinal, and
/// the effect occurrences whose failed outcome a `catch` in the segment
/// recovered from: the `catch` took the error and its block was not left
/// by a raise before the segment parked or ended. The occurrence's own
/// `process.effect_outcome`, committed before the segment ran, stays a
/// failure.
///
/// Written only by the runtime, in the transaction that hands the settled
/// segment to its engine, under the replay key
/// `process:{id}:ran:{step}`: a recomputed pass appends nothing new, and a
/// segment that never committed is never counted. Their sum is the count
/// of every statement on the process's committed path. Ordinals refer to
/// the document the process runs, so a reader restarts its sum at
/// `process.succeeded`.
pub const PROCESS_RAN_EVENT_TYPE: &str = super::events::ProcessEventKind::Ran.as_str();

/// Strict durable payload of one segment's counts: ordinals strictly
/// ascending, every count nonzero, at least one. `caught` is absent when
/// the segment recovered from nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ProcessRanFields")]
pub struct ProcessRan {
    pub vocabulary_version: u32,
    pub ran: Vec<(u32, u64)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caught: Vec<lash_protocol_types::EffectId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessRanFields {
    vocabulary_version: u32,
    ran: Vec<(u32, u64)>,
    #[serde(default)]
    caught: Vec<lash_protocol_types::EffectId>,
}

impl TryFrom<ProcessRanFields> for ProcessRan {
    type Error = ProcessEffectReportError;

    fn try_from(fields: ProcessRanFields) -> Result<Self, Self::Error> {
        let ran = Self {
            vocabulary_version: fields.vocabulary_version,
            ran: fields.ran,
            caught: fields.caught,
        };
        ran.check()?;
        Ok(ran)
    }
}

impl ProcessRan {
    /// The record of `ran`, written under `fleet_format`.
    pub fn new(ran: Vec<(u32, u64)>, fleet_format: crate::FleetFormat) -> Self {
        Self {
            vocabulary_version: fleet_format.writer_version(lash_core_store::surface_format!(
                PROCESS_EVENT_VOCABULARY_VERSION
            )),
            ran,
            caught: Vec::new(),
        }
    }

    /// This record with the occurrences its segment recovered from.
    #[must_use]
    pub fn with_caught(mut self, caught: Vec<lash_protocol_types::EffectId>) -> Self {
        self.caught = caught;
        self
    }

    /// The fleet-window counterpart of
    /// [`ProcessEffectOccurrence::decode`](super::ProcessEffectOccurrence::decode).
    pub fn decode(
        payload: serde_json::Value,
        fleet_format: crate::FleetFormat,
    ) -> Result<Self, ProcessEffectReportError> {
        let window = fleet_format.read_window(lash_core_store::surface_format!(
            PROCESS_EVENT_VOCABULARY_VERSION
        ));
        let version = require_vocabulary_version(&payload, window)?;
        if version != window.newest() {
            return Err(ProcessEffectReportError::UnsupportedVocabularyVersion {
                expected: window.newest(),
                actual: u64::from(version),
            });
        }
        let fields = serde_json::from_value::<ProcessRanFields>(payload)
            .map_err(ProcessEffectReportError::InvalidPayload)?;
        Self::try_from(fields)
    }

    /// The counterpart of
    /// [`ProcessEffectOccurrence::admit`](super::ProcessEffectOccurrence::admit).
    pub fn admit(&self, fleet_format: crate::FleetFormat) -> Result<(), ProcessEffectReportError> {
        admit_vocabulary_version(self.vocabulary_version, fleet_format)?;
        self.check()
    }

    fn check(&self) -> Result<(), ProcessEffectReportError> {
        let ascending = self.ran.windows(2).all(|pair| pair[0].0 < pair[1].0);
        if self.ran.is_empty() || !ascending || self.ran.iter().any(|(_, count)| *count == 0) {
            return Err(ProcessEffectReportError::InvalidRan);
        }
        Ok(())
    }

    /// The runtime append for this record, keyed by `replay_key`: one key
    /// per run segment, so a redrive recovers the same event.
    pub fn append_request(&self, replay_key: impl Into<String>) -> ProcessEventAppendRequest {
        ProcessEventAppendRequest::ran(self.clone(), replay_key)
    }
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the payload schema and admission checks its invariant"
)]
pub(super) fn ran_payload_schema() -> crate::JsonSchema {
    crate::JsonSchema::admit(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["vocabulary_version", "ran"],
        "properties": {
            "vocabulary_version": vocabulary_version_schema(),
            "caught": {
                "type": "array",
                "minItems": 1,
                "items": occurrence_schema()
            },
            "ran": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "array",
                    "minItems": 2,
                    "maxItems": 2,
                    "items": [
                        { "type": "integer", "minimum": 0, "maximum": u32::MAX },
                        { "type": "integer", "minimum": 1, "maximum": u64::MAX }
                    ],
                    "additionalItems": false
                }
            }
        }
    }))
    .expect("valid declared payload schema")
}
