//! The runtime-owned durable effect summary: one bounded record per effect
//! node, written at result incorporation (ADR 0100 R4).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::events::ProcessEventAppendRequest;

/// Version of the runtime-owned durable process-event vocabulary: the
/// `process.effect_*` kinds, their strict payloads, the per-node occurrence
/// cap and the failure-code mappings a recorded outcome carries. Changing any
/// of them changes what a redrive re-derives for an already-written record, so
/// it takes a new version.
pub const PROCESS_EVENT_VOCABULARY_VERSION: u32 = 1;

/// Runtime-owned event recording one effect occurrence of one runtime node.
///
/// Written only by the runtime, under its execution authority. The append key
/// is the effect's own replay key, so the journaled effect and the event name
/// the same logical effect.
///
/// Recovery contract (ADR 0100 R4): an incorporated occurrence stays pending
/// in the run and commits at the run's next boundary, as the prelude of that
/// boundary's own write and in its transaction — a wait's enter or clear, an
/// event the body appends, or the terminal completion. A segment boundary
/// commits nothing: the pending occurrences ride segment state and the
/// successor commits them with its first boundary. There is no periodic flush.
/// The journal and this event live in different stores; a crash before a
/// boundary commits loses nothing the journal cannot rebuild. On Restate the
/// redrive replays the invocation's journal (and, across segments, restores
/// segment state), re-derives the same pending occurrences without re-running
/// their effects, and commits them once. Re-committing a written occurrence is
/// a replay-key no-op; a different payload under the same key is refused and
/// its batch commits nothing. A failed boundary write carrying a summary is an
/// incorporation failure: the run aborts for redrive and the program never
/// observes it. Nothing here promises exactly-once external I/O before the
/// journal settles.
pub const PROCESS_EFFECT_OUTCOME_EVENT_TYPE: &str = "process.effect_outcome";

/// Runtime-owned event counting, per node and outcome class, the occurrences
/// beyond [`PROCESS_EFFECT_OCCURRENCE_CAP`] that were not recorded one by one.
/// Committed once, as the penultimate event of the run's terminal batch (after
/// its pending occurrences, before its terminal event), under the same
/// recovery contract as [`PROCESS_EFFECT_OUTCOME_EVENT_TYPE`].
pub const PROCESS_EFFECT_OMISSIONS_EVENT_TYPE: &str = "process.effect_omissions";

/// Occurrences of one effect node the runtime records individually. Later
/// occurrences are counted in the node's omission record instead. Pinned by
/// [`PROCESS_EVENT_VOCABULARY_VERSION`] so every attempt of a run applies the
/// same bound.
pub const PROCESS_EFFECT_OCCURRENCE_CAP: u64 = 8;

/// Terminal class of a recorded effect occurrence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessEffectOutcomeClass {
    Success,
    Failure,
    Cancelled,
}

/// Strict durable payload for one effect occurrence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessEffectSummaryOccurrence {
    pub vocabulary_version: u32,
    pub node_id: String,
    pub occurrence: u64,
    pub operation: String,
    pub outcome_class: ProcessEffectOutcomeClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<lash_sansio::FailureCode>,
    pub replay_key: String,
}

impl ProcessEffectSummaryOccurrence {
    pub fn new(
        node_id: impl Into<String>,
        occurrence: u64,
        operation: impl Into<String>,
        outcome_class: ProcessEffectOutcomeClass,
        code: Option<lash_sansio::FailureCode>,
        replay_key: impl Into<String>,
        fleet_format: crate::FleetFormat,
    ) -> Self {
        Self {
            vocabulary_version: fleet_format.writer_version(lash_core_store::surface_format!(
                PROCESS_EVENT_VOCABULARY_VERSION
            )),
            node_id: node_id.into(),
            occurrence,
            operation: operation.into(),
            outcome_class,
            code,
            replay_key: replay_key.into(),
        }
    }

    /// Whether the runtime records this occurrence one by one, rather than
    /// counting it in its node's omission record.
    pub fn is_within_cap(occurrence: u64) -> bool {
        (1..=PROCESS_EFFECT_OCCURRENCE_CAP).contains(&occurrence)
    }

    /// `fleet_format` is the `F` the bound store recorded: the payload admits
    /// the pair `{fleet's writer version, this build's newest}` — ADR 0106 §2's
    /// `[N-1, N]` window (FIG-3796) — and an admitted older payload climbs to
    /// the newest through the surface's `RecordUpcaster` hooks.
    pub fn decode(
        mut payload: serde_json::Value,
        fleet_format: crate::FleetFormat,
    ) -> Result<Self, ProcessEffectSummaryError> {
        let window = fleet_format.read_window(lash_core_store::surface_format!(
            PROCESS_EVENT_VOCABULARY_VERSION
        ));
        let version = require_vocabulary_version(&payload, window)?;
        if version != window.newest() {
            lash_core_store::store::upcast_json_record(
                "process effect outcome",
                lash_core_store::surface_format!(PROCESS_EVENT_VOCABULARY_VERSION),
                version,
                window.newest(),
                &mut payload,
            )
            .map_err(
                |_| ProcessEffectSummaryError::UnsupportedVocabularyVersion {
                    expected: window.newest(),
                    actual: u64::from(version),
                },
            )?;
        }
        let outcome: Self =
            serde_json::from_value(payload).map_err(ProcessEffectSummaryError::InvalidPayload)?;
        if !Self::is_within_cap(outcome.occurrence) {
            return Err(ProcessEffectSummaryError::OccurrenceOutsideCap {
                occurrence: outcome.occurrence,
            });
        }
        Ok(outcome)
    }

    /// The runtime append for this occurrence, keyed by the effect's replay
    /// key.
    pub fn append_request(&self) -> ProcessEventAppendRequest {
        ProcessEventAppendRequest::new(PROCESS_EFFECT_OUTCOME_EVENT_TYPE, vocabulary_payload(self))
            .with_replay_key(self.replay_key.clone())
    }
}

/// Omitted occurrences of one node, by outcome class.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessEffectOmittedCounts {
    pub success: u64,
    pub failure: u64,
    pub cancelled: u64,
}

impl ProcessEffectOmittedCounts {
    pub fn record(&mut self, class: ProcessEffectOutcomeClass) {
        let count = match class {
            ProcessEffectOutcomeClass::Success => &mut self.success,
            ProcessEffectOutcomeClass::Failure => &mut self.failure,
            ProcessEffectOutcomeClass::Cancelled => &mut self.cancelled,
        };
        *count = count.saturating_add(1);
    }

    pub fn total(&self) -> u64 {
        self.success
            .saturating_add(self.failure)
            .saturating_add(self.cancelled)
    }
}

/// Strict durable payload counting every node's omitted occurrences.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessEffectOmissions {
    pub vocabulary_version: u32,
    pub occurrence_cap: u64,
    pub nodes: BTreeMap<String, ProcessEffectOmittedCounts>,
}

impl ProcessEffectOmissions {
    pub fn new(
        nodes: BTreeMap<String, ProcessEffectOmittedCounts>,
        fleet_format: crate::FleetFormat,
    ) -> Self {
        Self {
            vocabulary_version: fleet_format.writer_version(lash_core_store::surface_format!(
                PROCESS_EVENT_VOCABULARY_VERSION
            )),
            occurrence_cap: PROCESS_EFFECT_OCCURRENCE_CAP,
            nodes,
        }
    }

    /// The fleet-window counterpart of
    /// [`ProcessEffectSummaryOccurrence::decode`].
    pub fn decode(
        mut payload: serde_json::Value,
        fleet_format: crate::FleetFormat,
    ) -> Result<Self, ProcessEffectSummaryError> {
        let window = fleet_format.read_window(lash_core_store::surface_format!(
            PROCESS_EVENT_VOCABULARY_VERSION
        ));
        let version = require_vocabulary_version(&payload, window)?;
        if version != window.newest() {
            lash_core_store::store::upcast_json_record(
                "process effect omissions",
                lash_core_store::surface_format!(PROCESS_EVENT_VOCABULARY_VERSION),
                version,
                window.newest(),
                &mut payload,
            )
            .map_err(
                |_| ProcessEffectSummaryError::UnsupportedVocabularyVersion {
                    expected: window.newest(),
                    actual: u64::from(version),
                },
            )?;
        }
        let omissions: Self =
            serde_json::from_value(payload).map_err(ProcessEffectSummaryError::InvalidPayload)?;
        if omissions.occurrence_cap != PROCESS_EFFECT_OCCURRENCE_CAP {
            return Err(ProcessEffectSummaryError::UnsupportedOccurrenceCap {
                expected: PROCESS_EFFECT_OCCURRENCE_CAP,
                actual: omissions.occurrence_cap,
            });
        }
        if omissions.nodes.is_empty() || omissions.nodes.values().any(|counts| counts.total() == 0)
        {
            return Err(ProcessEffectSummaryError::EmptyOmissions);
        }
        Ok(omissions)
    }

    /// The runtime append for this record, keyed by `replay_key`: one key per
    /// process run, so a redrive recovers the same event.
    pub fn append_request(&self, replay_key: impl Into<String>) -> ProcessEventAppendRequest {
        ProcessEventAppendRequest::new(
            PROCESS_EFFECT_OMISSIONS_EVENT_TYPE,
            vocabulary_payload(self),
        )
        .with_replay_key(replay_key)
    }
}

fn require_vocabulary_version(
    payload: &serde_json::Value,
    window: lash_core_store::store::ReadWindow,
) -> Result<u32, ProcessEffectSummaryError> {
    let version = payload
        .as_object()
        .and_then(|object| object.get("vocabulary_version"))
        .and_then(serde_json::Value::as_u64)
        .ok_or(ProcessEffectSummaryError::MissingVocabularyVersion)?;
    let version = u32::try_from(version).map_err(|_| {
        ProcessEffectSummaryError::UnsupportedVocabularyVersion {
            expected: window.newest(),
            actual: version,
        }
    })?;
    if !window.admits(version) {
        return Err(ProcessEffectSummaryError::UnsupportedVocabularyVersion {
            expected: window.newest(),
            actual: u64::from(version),
        });
    }
    Ok(version)
}

#[expect(
    clippy::expect_used,
    reason = "the vocabulary payloads are plain structs of strings, integers and maps, which always serialize"
)]
fn vocabulary_payload(payload: &impl Serialize) -> serde_json::Value {
    serde_json::to_value(payload).expect("vocabulary payloads always serialize")
}

/// The code a recorded tool failure carries. A failure the runtime, a policy
/// or a cancellation produced is Lash vocabulary; a spelling the tool or a
/// plugin authored stays in the author's namespace.
pub fn tool_failure_code(failure: &crate::ToolFailure) -> lash_sansio::FailureCode {
    match failure.source {
        crate::ToolFailureSource::Runtime
        | crate::ToolFailureSource::Policy
        | crate::ToolFailureSource::Cancellation => {
            lash_sansio::FailureCode::lash(lash_sansio::TurnFailureCode::from_wire(&failure.code))
        }
        crate::ToolFailureSource::Tool
        | crate::ToolFailureSource::Plugin
        | crate::ToolFailureSource::UnknownLegacy => {
            lash_sansio::FailureCode::from_foreign_wire(&failure.code)
        }
    }
}

/// One node's recorded occurrences and its omitted counts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessEffectNodeSummary {
    pub node_id: String,
    pub occurrences: Vec<ProcessEffectSummaryOccurrence>,
    pub omitted: ProcessEffectOmittedCounts,
}

/// The per-node effect table a reader rebuilds from a process's event log.
///
/// The bound is the writer's: the log holds at most
/// [`PROCESS_EFFECT_OCCURRENCE_CAP`] occurrence records per node plus one
/// omission record, and this fold only folds them. Its input is the log
/// itself, whose replay keys are unique; the result does not depend on page
/// boundaries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessEffectSummary {
    nodes: BTreeMap<String, ProcessEffectNodeSummary>,
}

impl ProcessEffectSummary {
    pub fn nodes(&self) -> impl ExactSizeIterator<Item = &ProcessEffectNodeSummary> {
        self.nodes.values()
    }

    pub fn node(&self, node_id: &str) -> Option<&ProcessEffectNodeSummary> {
        self.nodes.get(node_id)
    }

    /// Folds one event of the process's log, as a page of
    /// `Processes::events` or the registry returns it. Events of other kinds
    /// are ignored. `fleet_format` is the `F` the bound store recorded: the
    /// payload's read window comes from it (FIG-3796).
    pub fn fold_event(
        &mut self,
        event_type: &str,
        payload: &serde_json::Value,
        fleet_format: crate::FleetFormat,
    ) -> Result<(), ProcessEffectSummaryError> {
        match event_type {
            PROCESS_EFFECT_OUTCOME_EVENT_TYPE => {
                let outcome =
                    ProcessEffectSummaryOccurrence::decode(payload.clone(), fleet_format)?;
                let node = self.node_entry(&outcome.node_id);
                let position = node
                    .occurrences
                    .partition_point(|existing| existing.occurrence < outcome.occurrence);
                node.occurrences.insert(position, outcome);
            }
            PROCESS_EFFECT_OMISSIONS_EVENT_TYPE => {
                let omissions = ProcessEffectOmissions::decode(payload.clone(), fleet_format)?;
                for (node_id, counts) in omissions.nodes {
                    self.node_entry(&node_id).omitted = counts;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn node_entry(&mut self, node_id: &str) -> &mut ProcessEffectNodeSummary {
        self.nodes
            .entry(node_id.to_string())
            .or_insert_with(|| ProcessEffectNodeSummary {
                node_id: node_id.to_string(),
                occurrences: Vec::new(),
                omitted: ProcessEffectOmittedCounts::default(),
            })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessEffectSummaryError {
    #[error("effect summary payload is missing its vocabulary_version")]
    MissingVocabularyVersion,
    #[error("effect summary vocabulary version {actual} is unsupported; expected {expected}")]
    UnsupportedVocabularyVersion { expected: u32, actual: u64 },
    #[error("invalid effect summary payload: {0}")]
    InvalidPayload(serde_json::Error),
    #[error("effect occurrence {occurrence} is outside the recorded cap")]
    OccurrenceOutsideCap { occurrence: u64 },
    #[error("effect omission occurrence cap {actual} is unsupported; expected {expected}")]
    UnsupportedOccurrenceCap { expected: u64, actual: u64 },
    #[error("effect omission record names no omitted occurrence")]
    EmptyOmissions,
}

pub(super) fn effect_outcome_payload_schema() -> crate::LashSchema {
    crate::LashSchema::new(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "vocabulary_version", "node_id", "occurrence", "operation",
            "outcome_class", "replay_key"
        ],
        "properties": {
            "vocabulary_version": { "const": PROCESS_EVENT_VOCABULARY_VERSION },
            "node_id": { "type": "string", "minLength": 1 },
            "occurrence": {
                "type": "integer",
                "minimum": 1,
                "maximum": PROCESS_EFFECT_OCCURRENCE_CAP
            },
            "operation": { "type": "string", "minLength": 1 },
            "outcome_class": {
                "type": "string",
                "enum": ["success", "failure", "cancelled"]
            },
            "code": { "type": "string", "minLength": 1 },
            "replay_key": { "type": "string", "minLength": 1 }
        }
    }))
}

pub(super) fn effect_omissions_payload_schema() -> crate::LashSchema {
    let count = serde_json::json!({ "type": "integer", "minimum": 0 });
    crate::LashSchema::new(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["vocabulary_version", "occurrence_cap", "nodes"],
        "properties": {
            "vocabulary_version": { "const": PROCESS_EVENT_VOCABULARY_VERSION },
            "occurrence_cap": { "const": PROCESS_EFFECT_OCCURRENCE_CAP },
            "nodes": {
                "type": "object",
                "minProperties": 1,
                "additionalProperties": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["success", "failure", "cancelled"],
                    "properties": {
                        "success": count,
                        "failure": count,
                        "cancelled": count
                    }
                }
            }
        }
    }))
}

#[cfg(test)]
#[path = "effect_summary_tests.rs"]
mod tests;
