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
///
/// version_guard(
///     file(
///         cover(
///             PROCESS_EFFECT_OUTCOME_EVENT_TYPE, PROCESS_EFFECT_OMISSIONS_EVENT_TYPE,
///             PROCESS_EFFECT_OCCURRENCE_CAP, "struct ProcessEffectOccurrence",
///             "struct ProcessEffectOmissions", "fn append_request", "fn decode",
///             "fn tool_failure_code", "fn effect_outcome_payload_schema",
///             "fn effect_omissions_payload_schema",
///         ),
///     ),
///     items(path = "crates/lash-core-execution/src/runtime/process/events.rs", ProcessLifecycleFact),
/// )
#[cfg(not(feature = "synthetic-next"))]
/// version_surface = "migrate"
/// format_manifest = "ProcessEffectReport"
pub const PROCESS_EVENT_VOCABULARY_VERSION: u32 = 1;

/// Phase A's synthetic N+1 (ADR 0115 §6) moves the surface one version on
/// with version 1's shape; its registered lift reads what N wrote.
#[cfg(feature = "synthetic-next")]
/// version_surface = "migrate"
/// format_manifest = "ProcessEffectReport"
pub const PROCESS_EVENT_VOCABULARY_VERSION: u32 = 2;

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
/// A crash before a boundary commits loses nothing committed state cannot
/// rebuild: the resumed process re-derives the same pending occurrences from
/// its committed state without re-running their effects, and commits them
/// once. Re-committing a written occurrence is
/// a replay-key no-op; a different payload under the same key is refused and
/// its batch commits nothing. A failed boundary write carrying a summary is an
/// incorporation failure: the run aborts for redrive and the program never
/// observes it. Nothing here promises exactly-once external I/O before the
/// journal settles.
pub const PROCESS_EFFECT_OUTCOME_EVENT_TYPE: &str =
    super::events::ProcessEventKind::EffectOutcome.as_str();

/// Runtime-owned event counting, per node and outcome class, the occurrences
/// settled after the node's first [`PROCESS_EFFECT_OCCURRENCE_CAP`], which
/// were not recorded one by one.
/// Committed once, as the penultimate event of the run's terminal batch (after
/// its pending occurrences, before its terminal event), under the same
/// recovery contract as [`PROCESS_EFFECT_OUTCOME_EVENT_TYPE`].
pub const PROCESS_EFFECT_OMISSIONS_EVENT_TYPE: &str =
    super::events::ProcessEventKind::EffectOmissions.as_str();

/// How many effect occurrences of one node are recorded one by one, across
/// every site of the node: the fixed validated wire ceiling.
/// The host may choose a smaller evidence cut through `TraceLimits`; the
/// driver pins that cut in its first transition and omissions record it.
pub const PROCESS_EFFECT_OCCURRENCE_CAP: u64 = 8;

/// Terminal class of a recorded effect occurrence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessEffectOutcomeClass {
    Success,
    Failure,
    Cancelled,
}

/// Strict durable payload for one effect occurrence. Only a failure may
/// carry a failure code; success and cancellation carry none.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ProcessEffectOccurrenceFields")]
pub struct ProcessEffectOccurrence {
    pub vocabulary_version: u32,
    pub node_id: String,
    /// Which occurrence of its site this is, from 1, counted per site.
    pub occurrence: u64,
    /// The exact site inside the node and the loop activations around the
    /// occurrence: two calls of one node stay apart after the live window.
    #[serde(
        default,
        skip_serializing_if = "lash_sansio::WorkflowOccurrenceContext::is_default"
    )]
    pub context: lash_sansio::WorkflowOccurrenceContext,
    pub operation: String,
    pub outcome_class: ProcessEffectOutcomeClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<lash_sansio::FailureCode>,
    /// The tool call this effect records; engine-only effects have no tool call.
    pub call_id: Option<crate::ToolCallId>,
    pub replay_key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessEffectOccurrenceFields {
    vocabulary_version: u32,
    node_id: String,
    occurrence: u64,
    #[serde(default)]
    context: lash_sansio::WorkflowOccurrenceContext,
    operation: String,
    outcome_class: ProcessEffectOutcomeClass,
    #[serde(default, deserialize_with = "nonempty_failure_code")]
    code: Option<lash_sansio::FailureCode>,
    #[serde(deserialize_with = "Option::deserialize")]
    call_id: Option<crate::ToolCallId>,
    replay_key: String,
}

const CODE_ON_A_NON_FAILURE: &str = "only a failed effect occurrence may carry a failure code";
const OCCURRENCES_COUNT_FROM_ONE: &str = "an effect occurrence counts from 1";

impl TryFrom<ProcessEffectOccurrenceFields> for ProcessEffectOccurrence {
    type Error = ProcessEffectReportError;

    fn try_from(fields: ProcessEffectOccurrenceFields) -> Result<Self, Self::Error> {
        let outcome = Self {
            vocabulary_version: fields.vocabulary_version,
            node_id: fields.node_id,
            occurrence: fields.occurrence,
            context: fields.context,
            operation: fields.operation,
            outcome_class: fields.outcome_class,
            code: fields.code,
            call_id: fields.call_id,
            replay_key: fields.replay_key,
        };
        outcome.check()?;
        Ok(outcome)
    }
}

fn nonempty_failure_code<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<lash_sansio::FailureCode>, D::Error> {
    let code = String::deserialize(deserializer)?;
    if code.is_empty() {
        return Err(serde::de::Error::custom(
            "an effect failure code must be nonempty",
        ));
    }
    Ok(Some(lash_sansio::FailureCode::from_wire(&code)))
}

fn nonempty_identifier(
    identifier: &str,
    field: &'static str,
) -> Result<(), ProcessEffectReportError> {
    if identifier.is_empty() {
        return Err(ProcessEffectReportError::EmptyIdentifier { field });
    }
    Ok(())
}

impl ProcessEffectOccurrence {
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
            context: lash_sansio::WorkflowOccurrenceContext::default(),
            operation: operation.into(),
            outcome_class,
            code,
            call_id: None,
            replay_key: replay_key.into(),
        }
    }

    /// This occurrence at the exact site and loop context `context`.
    #[must_use]
    pub fn at(mut self, context: lash_sansio::WorkflowOccurrenceContext) -> Self {
        self.context = context;
        self
    }

    /// `fleet_format` is the `F` the bound store recorded: the payload admits
    /// the pair `{fleet's writer version, this build's newest}` — ADR 0106 §2's
    /// `[N-1, N]` window (FIG-3796) — and an admitted older payload climbs to
    /// the newest through the surface's `RecordUpcaster` hooks.
    pub fn decode(
        mut payload: serde_json::Value,
        fleet_format: crate::FleetFormat,
    ) -> Result<Self, ProcessEffectReportError> {
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
            .map_err(|_| ProcessEffectReportError::UnsupportedVocabularyVersion {
                expected: window.newest(),
                actual: u64::from(version),
            })?;
        }
        let fields = serde_json::from_value::<ProcessEffectOccurrenceFields>(payload)
            .map_err(ProcessEffectReportError::InvalidPayload)?;
        Self::try_from(fields)
    }

    /// Admit an occurrence this build holds typed, as [`Self::decode`]
    /// admits a stored one: its vocabulary version is in `fleet_format`'s
    /// read window and it is one the runtime records.
    pub fn admit(&self, fleet_format: crate::FleetFormat) -> Result<(), ProcessEffectReportError> {
        admit_vocabulary_version(self.vocabulary_version, fleet_format)?;
        self.check()
    }

    fn check(&self) -> Result<(), ProcessEffectReportError> {
        nonempty_identifier(&self.node_id, "node_id")?;
        nonempty_identifier(&self.operation, "operation")?;
        nonempty_identifier(&self.replay_key, "replay_key")?;
        if self.outcome_class != ProcessEffectOutcomeClass::Failure && self.code.is_some() {
            return Err(ProcessEffectReportError::InvalidPayload(
                <serde_json::Error as serde::de::Error>::custom(CODE_ON_A_NON_FAILURE),
            ));
        }
        if self.occurrence == 0 {
            return Err(ProcessEffectReportError::InvalidPayload(
                <serde_json::Error as serde::de::Error>::custom(OCCURRENCES_COUNT_FROM_ONE),
            ));
        }
        Ok(())
    }

    /// The runtime append for this occurrence, keyed by the effect's replay
    /// key.
    pub fn append_request(&self) -> ProcessEventAppendRequest {
        ProcessEventAppendRequest::effect_outcome(self.clone())
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
#[serde(try_from = "ProcessEffectOmissionsFields")]
pub struct ProcessEffectOmissions {
    pub vocabulary_version: u32,
    pub occurrence_cap: u64,
    pub nodes: BTreeMap<String, ProcessEffectOmittedCounts>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessEffectOmissionsFields {
    vocabulary_version: u32,
    occurrence_cap: u64,
    nodes: BTreeMap<String, ProcessEffectOmittedCounts>,
}

impl TryFrom<ProcessEffectOmissionsFields> for ProcessEffectOmissions {
    type Error = ProcessEffectReportError;

    fn try_from(fields: ProcessEffectOmissionsFields) -> Result<Self, Self::Error> {
        let omissions = Self {
            vocabulary_version: fields.vocabulary_version,
            occurrence_cap: fields.occurrence_cap,
            nodes: fields.nodes,
        };
        omissions.check()?;
        Ok(omissions)
    }
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
    /// [`ProcessEffectOccurrence::decode`].
    pub fn decode(
        mut payload: serde_json::Value,
        fleet_format: crate::FleetFormat,
    ) -> Result<Self, ProcessEffectReportError> {
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
            .map_err(|_| ProcessEffectReportError::UnsupportedVocabularyVersion {
                expected: window.newest(),
                actual: u64::from(version),
            })?;
        }
        let fields = serde_json::from_value::<ProcessEffectOmissionsFields>(payload)
            .map_err(ProcessEffectReportError::InvalidPayload)?;
        Self::try_from(fields)
    }

    /// The counterpart of [`ProcessEffectOccurrence::admit`].
    pub fn admit(&self, fleet_format: crate::FleetFormat) -> Result<(), ProcessEffectReportError> {
        admit_vocabulary_version(self.vocabulary_version, fleet_format)?;
        self.check()
    }

    fn check(&self) -> Result<(), ProcessEffectReportError> {
        if self.occurrence_cap > PROCESS_EFFECT_OCCURRENCE_CAP {
            return Err(ProcessEffectReportError::UnsupportedOccurrenceCap {
                expected: PROCESS_EFFECT_OCCURRENCE_CAP,
                actual: self.occurrence_cap,
            });
        }
        for node in self.nodes.keys() {
            nonempty_identifier(node, "node_id")?;
        }
        if self.nodes.is_empty() || self.nodes.values().any(|counts| counts.total() == 0) {
            return Err(ProcessEffectReportError::EmptyOmissions);
        }
        Ok(())
    }

    /// The runtime append for this record, keyed by `replay_key`: one key per
    /// process run, so a redrive recovers the same event.
    pub fn append_request(&self, replay_key: impl Into<String>) -> ProcessEventAppendRequest {
        ProcessEventAppendRequest::effect_omissions(self.clone(), replay_key)
    }
}

fn require_vocabulary_version(
    payload: &serde_json::Value,
    window: lash_core_store::store::ReadWindow,
) -> Result<u32, ProcessEffectReportError> {
    let version = payload
        .as_object()
        .and_then(|object| object.get("vocabulary_version"))
        .and_then(serde_json::Value::as_u64)
        .ok_or(ProcessEffectReportError::MissingVocabularyVersion)?;
    let version = u32::try_from(version).map_err(|_| {
        ProcessEffectReportError::UnsupportedVocabularyVersion {
            expected: window.newest(),
            actual: version,
        }
    })?;
    if !window.admits(version) {
        return Err(ProcessEffectReportError::UnsupportedVocabularyVersion {
            expected: window.newest(),
            actual: u64::from(version),
        });
    }
    Ok(version)
}

fn admit_vocabulary_version(
    version: u32,
    fleet_format: crate::FleetFormat,
) -> Result<(), ProcessEffectReportError> {
    let window = fleet_format.read_window(lash_core_store::surface_format!(
        PROCESS_EVENT_VOCABULARY_VERSION
    ));
    if !window.admits(version) {
        return Err(ProcessEffectReportError::UnsupportedVocabularyVersion {
            expected: window.newest(),
            actual: u64::from(version),
        });
    }
    Ok(())
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
        crate::ToolFailureSource::Tool | crate::ToolFailureSource::Plugin => {
            lash_sansio::FailureCode::from_foreign_wire(&failure.code)
        }
    }
}

/// One node's recorded occurrences and its omitted counts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessEffectNodeReport {
    pub node_id: String,
    pub occurrences: Vec<ProcessEffectOccurrence>,
    pub omitted: ProcessEffectOmittedCounts,
}

/// The per-node effect table a reader rebuilds from a process's event log.
///
/// The bound is the writer's: the log holds at most
/// [`PROCESS_EFFECT_OCCURRENCE_CAP`] occurrence records per node, across all
/// of the node's sites, plus one omission record, and this fold only folds
/// them. Its input is the log
/// itself, whose replay keys are unique; the result does not depend on page
/// boundaries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessEffectReport {
    nodes: BTreeMap<String, ProcessEffectNodeReport>,
}

impl ProcessEffectReport {
    pub fn nodes(&self) -> impl ExactSizeIterator<Item = &ProcessEffectNodeReport> {
        self.nodes.values()
    }

    pub fn node(&self, node_id: &str) -> Option<&ProcessEffectNodeReport> {
        self.nodes.get(node_id)
    }

    /// Folds one fact of the process's log, as a page of
    /// `Processes::events` or the registry returns it. Facts of other kinds
    /// are ignored. `fleet_format` is the `F` the bound store recorded: the
    /// fact's read window comes from it (FIG-3796).
    pub fn fold_event(
        &mut self,
        fact: &super::ProcessLifecycleFact,
        fleet_format: crate::FleetFormat,
    ) -> Result<(), ProcessEffectReportError> {
        match fact {
            super::ProcessLifecycleFact::EffectOutcome(outcome) => {
                outcome.admit(fleet_format)?;
                let node = self.node_entry(&outcome.node_id);
                // Site order, then occurrence: one order whatever page the
                // facts arrived in.
                let key = |occurrence: &ProcessEffectOccurrence| {
                    (occurrence.context.site_path.clone(), occurrence.occurrence)
                };
                let position = node
                    .occurrences
                    .partition_point(|existing| key(existing) < key(outcome));
                node.occurrences.insert(position, outcome.clone());
            }
            super::ProcessLifecycleFact::EffectOmissions(omissions) => {
                omissions.admit(fleet_format)?;
                for (node_id, counts) in &omissions.nodes {
                    self.node_entry(node_id).omitted = *counts;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn node_entry(&mut self, node_id: &str) -> &mut ProcessEffectNodeReport {
        self.nodes
            .entry(node_id.to_string())
            .or_insert_with(|| ProcessEffectNodeReport {
                node_id: node_id.to_string(),
                occurrences: Vec::new(),
                omitted: ProcessEffectOmittedCounts::default(),
            })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessEffectReportError {
    #[error("effect summary {field} must be nonempty")]
    EmptyIdentifier { field: &'static str },
    #[error("effect summary payload is missing its vocabulary_version")]
    MissingVocabularyVersion,
    #[error("effect summary vocabulary version {actual} is unsupported; expected {expected}")]
    UnsupportedVocabularyVersion { expected: u32, actual: u64 },
    #[error("invalid effect summary payload: {0}")]
    InvalidPayload(serde_json::Error),
    #[error("effect omission occurrence cap {actual} exceeds the wire ceiling {expected}")]
    UnsupportedOccurrenceCap { expected: u64, actual: u64 },
    #[error("effect omission record names no omitted occurrence")]
    EmptyOmissions,
}

/// The `vocabulary_version` an appended payload may carry: the version each
/// fleet epoch this build writes under assigns the vocabulary (ADR 0115
/// §2.1). A build writing under one epoch writes one version; a compatibility
/// release writes `F_prev`'s version until finalize and `F_self`'s after, so
/// its validator admits both.
fn vocabulary_version_schema() -> serde_json::Value {
    let writable = crate::FleetFormat::writable();
    let mut versions = (writable.min()..=writable.max())
        .map(|fleet| {
            crate::FleetFormat::from_version(fleet).writer_version(
                lash_core_store::surface_format!(PROCESS_EVENT_VOCABULARY_VERSION),
            )
        })
        .collect::<Vec<_>>();
    versions.sort_unstable();
    versions.dedup();
    match versions.as_slice() {
        [version] => serde_json::json!({ "const": version }),
        _ => serde_json::json!({ "enum": versions }),
    }
}

/// The shape of an occurrence's site context. The typed decode is strict
/// about its segments; admission only bounds the envelope.
fn occurrence_context_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "site_path": { "type": "array", "items": { "type": ["object", "string"] } },
            "loops": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["site", "activation", "position"],
                    "properties": {
                        "site": { "type": "object" },
                        "activation": { "type": "integer", "minimum": 1, "maximum": u64::MAX },
                        "position": { "type": "object" }
                    }
                }
            }
        }
    })
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
pub(super) fn effect_outcome_payload_schema() -> crate::JsonSchema {
    crate::JsonSchema::admit(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "vocabulary_version", "node_id", "occurrence", "operation",
            "outcome_class", "call_id", "replay_key"
        ],
        "if": { "properties": { "outcome_class": { "const": "failure" } } },
        "else": { "not": { "required": ["code"] } },
        "properties": {
            "vocabulary_version": vocabulary_version_schema(),
            "node_id": { "type": "string", "minLength": 1 },
            "occurrence": { "type": "integer", "minimum": 1, "maximum": u64::MAX },
            "context": occurrence_context_schema(),
            "operation": { "type": "string", "minLength": 1 },
            "outcome_class": {
                "type": "string",
                "enum": ["success", "failure", "cancelled"]
            },
            "code": { "type": "string", "minLength": 1 },
            "call_id": { "type": ["string", "null"], "pattern": "^tc_[0-9a-f]{64}$" },
            "replay_key": { "type": "string", "minLength": 1 }
        }
    }))
    .expect("valid declared payload schema")
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
pub(super) fn effect_omissions_payload_schema() -> crate::JsonSchema {
    let count = serde_json::json!({ "type": "integer", "minimum": 0, "maximum": u64::MAX });
    crate::JsonSchema::admit(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["vocabulary_version", "occurrence_cap", "nodes"],
        "properties": {
            "vocabulary_version": vocabulary_version_schema(),
            "occurrence_cap": { "type": "integer", "minimum": 0, "maximum": PROCESS_EFFECT_OCCURRENCE_CAP },
            "nodes": {
                "type": "object",
                "minProperties": 1,
                "propertyNames": { "minLength": 1 },
                "additionalProperties": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["success", "failure", "cancelled"],
                    "anyOf": [
                        { "properties": { "success": { "minimum": 1 } } },
                        { "properties": { "failure": { "minimum": 1 } } },
                        { "properties": { "cancelled": { "minimum": 1 } } }
                    ],
                    "properties": {
                        "success": count,
                        "failure": count,
                        "cancelled": count
                    }
                }
            }
        }
    }))
    .expect("valid declared payload schema")
}

impl lash_core_store::store::DurableRecord for super::events::ProcessEventKind {
    const SURFACE: lash_core_store::store::SurfaceFormat = lash_core_store::surface_format!(
        crate::runtime::process::effect_summary::PROCESS_EVENT_VOCABULARY_VERSION
    );
}

#[cfg(test)]
#[path = "effect_summary_tests.rs"]
mod tests;
