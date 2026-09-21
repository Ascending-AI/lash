use lash_sansio::{AttachmentRef, TurnProtocol};

/// Read-only legacy protocol-owned assistant context paired with an RLM
/// trajectory entry.
///
/// New sessions persist this context as ordinary durable assistant messages so
/// provider reasoning replay metadata survives. This event must keep decoding
/// for old session histories, but producers must not write new instances.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct RlmAssistantContent {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prose: String,
}

/// What an RLM cell resolved to: still running, failed, or finished with a
/// driver-adjudicated value. One of three — a cell can never carry an error
/// and a terminal value at once.
///
/// `E` is each layer's own error representation: `String` in durable
/// trajectory and history records, `CellFailure` inside a parked driver
/// state.
///
/// The durable spelling stays the `error` / `final_output` key pair stored
/// trajectories already carry — the value flattens into the entry's object,
/// writes at most one of the two keys, and refuses to decode a record that
/// sets both.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum CellOutcome<E> {
    /// The cell produced neither an error nor a terminal value.
    #[default]
    Running,
    /// The cell failed.
    Failed(E),
    /// The cell produced a driver-adjudicated terminal value.
    Finished(serde_json::Value),
}

impl<E> CellOutcome<E> {
    /// Fold an `error` / `terminal value` pair into the single outcome it
    /// A pair carrying both resolves to the failure: a finished cell must not silently discard
    /// its error.
    pub fn from_parts(error: Option<E>, terminal: Option<serde_json::Value>) -> Self {
        match (error, terminal) {
            (Some(error), _) => Self::Failed(error),
            (None, Some(value)) => Self::Finished(value),
            (None, None) => Self::Running,
        }
    }

    /// The failure, when the cell failed.
    pub fn error(&self) -> Option<&E> {
        match self {
            Self::Failed(error) => Some(error),
            _ => None,
        }
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed(_))
    }

    /// The terminal value, when the cell finished.
    pub fn terminal_value(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Finished(value) => Some(value),
            _ => None,
        }
    }

    /// A borrowed view, for remapping the error without cloning.
    pub fn as_ref(&self) -> CellOutcome<&E> {
        match self {
            Self::Running => CellOutcome::Running,
            Self::Failed(error) => CellOutcome::Failed(error),
            Self::Finished(value) => CellOutcome::Finished(value.clone()),
        }
    }

    pub fn map_error<F>(self, op: impl FnOnce(E) -> F) -> CellOutcome<F> {
        match self {
            Self::Running => CellOutcome::Running,
            Self::Failed(error) => CellOutcome::Failed(op(error)),
            Self::Finished(value) => CellOutcome::Finished(value),
        }
    }
}

impl<E: serde::Serialize> serde::Serialize for CellOutcome<E> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        struct Fields<'a, E> {
            #[serde(skip_serializing_if = "Option::is_none")]
            error: Option<&'a E>,
            #[serde(skip_serializing_if = "Option::is_none")]
            final_output: Option<&'a serde_json::Value>,
        }
        let (error, final_output) = match self {
            Self::Running => (None, None),
            Self::Failed(error) => (Some(error), None),
            Self::Finished(value) => (None, Some(value)),
        };
        Fields {
            error,
            final_output,
        }
        .serialize(serializer)
    }
}

impl<'de, E: serde::Deserialize<'de>> serde::Deserialize<'de> for CellOutcome<E> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Fields<E> {
            error: Option<E>,
            final_output: Option<serde_json::Value>,
        }
        let fields = Fields::deserialize(deserializer)?;
        match (fields.error, fields.final_output) {
            (Some(_), Some(_)) => Err(serde::de::Error::custom(
                "a cell outcome cannot carry both `error` and `final_output`",
            )),
            (error, terminal) => Ok(Self::from_parts(error, terminal)),
        }
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RlmTrajectoryEntry {
    pub id: String,
    pub protocol_iteration: usize,
    pub code: String,
    /// One entry per `print` (and any raw stdout-style emission from the
    /// lashlang executor). Replaces the old split between a combined
    /// `output: String` and `observations: Vec<String>` — those carried
    /// the same content twice, wasting tokens on every history-bearing
    /// iteration.
    pub output: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<AttachmentRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<RlmExecutedCall>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub calls_omitted: usize,
    /// What the cell resolved to. Serialized flat as the `error` /
    /// `final_output` key pair stored trajectories already carry; at most
    /// one key is ever written, and decoding refuses a record that sets
    /// both.
    #[serde(flatten)]
    pub outcome: CellOutcome<String>,
}

pub type RlmExecutedCall = lash_sansio::ExecutedCallRecord;
pub type RlmExecutedCallOutcome = lash_sansio::ExecutedCallOutcome;

fn is_zero(value: &usize) -> bool {
    *value == 0
}

impl RlmTrajectoryEntry {
    pub fn output_chars(&self) -> usize {
        self.output.iter().map(|s| s.chars().count()).sum()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RlmHistoryRole {
    User,
    System,
    Assistant,
    Event,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RlmAttachmentRef {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<lash_sansio::MediaType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub source: String,
    pub reference: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RlmImageRef {
    pub id: String,
    pub media_type: lash_sansio::MediaType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    pub bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl RlmImageRef {
    /// Build the model-visible image metadata from its durable attachment reference.
    pub fn from_attachment(attachment: &lash_sansio::AttachmentRef) -> Self {
        let (width, height) = match attachment.type_metadata.as_ref() {
            Some(lash_sansio::AttachmentTypeMetadata::Image { width, height }) => (*width, *height),
            None => (None, None),
        };
        Self {
            id: attachment.id.to_string(),
            media_type: attachment.media_type.clone(),
            width,
            height,
            bytes: attachment.byte_len as usize,
            label: attachment.label.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RlmHistoryItem {
    Message {
        id: String,
        role: RlmHistoryRole,
        content: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<RlmAttachmentRef>,
    },
    LashlangStep {
        id: String,
        protocol_iteration: usize,
        code: String,
        output: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<RlmImageRef>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        calls: Vec<RlmExecutedCall>,
        #[serde(default, skip_serializing_if = "is_zero")]
        calls_omitted: usize,
        /// What the cell resolved to; same flat `error` / `final_output`
        /// spelling the trajectory entry carries.
        #[serde(flatten)]
        outcome: CellOutcome<String>,
    },
}

impl RlmHistoryItem {
    /// The image representations intentionally remain distinct: persisted
    /// entries retain attachment references, while history items expose the
    /// compact image metadata used by the model.
    pub fn from_trajectory_entry(entry: &RlmTrajectoryEntry) -> Self {
        Self::LashlangStep {
            id: entry.id.clone(),
            protocol_iteration: entry.protocol_iteration,
            code: entry.code.clone(),
            output: entry.output.clone(),
            images: entry
                .images
                .iter()
                .map(RlmImageRef::from_attachment)
                .collect(),
            calls: entry.calls.clone(),
            calls_omitted: entry.calls_omitted,
            outcome: entry.outcome.clone(),
        }
    }
}

#[cfg(test)]
mod rlm_step_serde_tests {
    use std::fmt;

    use serde::de::{IgnoredAny, MapAccess, Visitor};
    use serde::{Deserializer as _, Serialize};

    use super::{RlmHistoryItem, RlmTrajectoryEntry};

    fn serialized_field_order<T: Serialize>(value: &T) -> Vec<String> {
        struct FieldOrderVisitor;

        impl<'de> Visitor<'de> for FieldOrderVisitor {
            type Value = Vec<String>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a serialized JSON object")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut fields = Vec::new();
                while let Some(field) = map.next_key::<String>()? {
                    fields.push(field);
                    map.next_value::<IgnoredAny>()?;
                }
                Ok(fields)
            }
        }

        let encoded = serde_json::to_string(value).expect("step serializes");
        let mut deserializer = serde_json::Deserializer::from_str(&encoded);
        let fields = deserializer
            .deserialize_map(FieldOrderVisitor)
            .expect("step is a JSON object");
        deserializer
            .end()
            .expect("serialized step has one JSON value");
        fields
    }

    fn shared_field_order<T: Serialize>(value: &T) -> Vec<String> {
        serialized_field_order(value)
            .into_iter()
            .filter(|field| field != "images" && field != "kind")
            .collect()
    }

    fn populated_entry() -> RlmTrajectoryEntry {
        RlmTrajectoryEntry {
            id: "step-1".to_string(),
            protocol_iteration: 3,
            code: "print('hello')".to_string(),
            output: vec!["hello".to_string()],
            images: vec![lash_sansio::AttachmentRef {
                id: "image-1".parse().expect("valid attachment id"),
                media_type: "image/png".parse().expect("valid media type"),
                byte_len: 42,
                type_metadata: Some(lash_sansio::AttachmentTypeMetadata::image(
                    Some(640),
                    Some(480),
                )),
                label: Some("plot".to_string()),
            }],
            calls: vec![lash_sansio::ExecutedCallRecord {
                operation: "math.add".to_string(),
                outcome: lash_sansio::ExecutedCallOutcome::Ok,
            }],
            calls_omitted: 2,
            outcome: super::CellOutcome::Finished(serde_json::json!({"answer": 42})),
        }
    }

    #[test]
    fn trajectory_and_history_step_serde_shapes_stay_in_parity() {
        let entry = populated_entry();
        let history = RlmHistoryItem::from_trajectory_entry(&entry);

        assert!(matches!(
            &history,
            RlmHistoryItem::LashlangStep { images, .. }
                if matches!(
                    images.as_slice(),
                    [image]
                        if image.id == "image-1"
                            && image.width == Some(640)
                            && image.height == Some(480)
                            && image.bytes == 42
                            && image.label.as_deref() == Some("plot")
                )
        ));

        let expected_shared_fields = [
            "id",
            "protocol_iteration",
            "code",
            "output",
            "calls",
            "calls_omitted",
            "final_output",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        assert_eq!(shared_field_order(&entry), expected_shared_fields);
        assert_eq!(shared_field_order(&history), expected_shared_fields);

        // A failed step spells the same outcome slot `error`-keyed, again in
        // parity between the two durable forms.
        let failed_entry = RlmTrajectoryEntry {
            outcome: super::CellOutcome::Failed("boom".to_string()),
            ..populated_entry()
        };
        let expected_failed_fields = [
            "id",
            "protocol_iteration",
            "code",
            "output",
            "calls",
            "calls_omitted",
            "error",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        assert_eq!(shared_field_order(&failed_entry), expected_failed_fields);
        assert_eq!(
            shared_field_order(&RlmHistoryItem::from_trajectory_entry(&failed_entry)),
            expected_failed_fields
        );

        let entry_fields = serialized_field_order(&entry);
        let history_fields = serialized_field_order(&history);
        assert!(entry_fields.contains(&"images".to_string()));
        assert!(history_fields.contains(&"images".to_string()));
        assert!(history_fields.contains(&"kind".to_string()));

        let sparse_entry = RlmTrajectoryEntry {
            id: "step-empty".to_string(),
            protocol_iteration: 0,
            code: "".to_string(),
            output: Vec::new(),
            images: Vec::new(),
            calls: Vec::new(),
            calls_omitted: 0,
            outcome: super::CellOutcome::Running,
        };
        let sparse_history = RlmHistoryItem::from_trajectory_entry(&sparse_entry);
        let expected_sparse_fields = ["id", "protocol_iteration", "code", "output"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(shared_field_order(&sparse_entry), expected_sparse_fields);
        assert_eq!(shared_field_order(&sparse_history), expected_sparse_fields);
        assert!(!serialized_field_order(&sparse_entry).contains(&"images".to_string()));
        assert!(!serialized_field_order(&sparse_history).contains(&"images".to_string()));
    }

    #[test]
    fn trajectory_entry_refuses_an_outcome_both_failed_and_finished() {
        for (label, decoded) in [
            (
                "trajectory entry",
                serde_json::from_value::<RlmTrajectoryEntry>(serde_json::json!({
                    "id": "step-1",
                    "protocol_iteration": 0,
                    "code": "print('x')",
                    "output": [],
                    "error": "boom",
                    "final_output": {"answer": 42},
                }))
                .map(|_| ()),
            ),
            (
                "history step",
                serde_json::from_value::<RlmHistoryItem>(serde_json::json!({
                    "kind": "lashlang_step",
                    "id": "step-1",
                    "protocol_iteration": 0,
                    "code": "print('x')",
                    "output": [],
                    "error": "boom",
                    "final_output": {"answer": 42},
                }))
                .map(|_| ()),
            ),
        ] {
            assert!(
                decoded.is_err(),
                "{label}: a step cannot be failed and finished at once"
            );
        }
    }

    #[test]
    fn legacy_observations_alias_is_rejected() {
        let entry_error = serde_json::from_value::<RlmTrajectoryEntry>(serde_json::json!({
            "id": "legacy-step",
            "protocol_iteration": 4,
            "code": "print('legacy')",
            "observations": ["legacy output"],
        }))
        .expect_err("legacy trajectory alias must be rejected");
        assert!(entry_error.to_string().contains("missing field `output`"));

        let history_error = serde_json::from_value::<RlmHistoryItem>(serde_json::json!({
            "kind": "lashlang_step",
            "id": "legacy-step",
            "protocol_iteration": 4,
            "code": "print('legacy')",
            "observations": ["legacy output"],
        }))
        .expect_err("legacy history alias must be rejected");
        assert!(history_error.to_string().contains("missing field `output`"));
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct RlmGlobalsPatchPluginBody {
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub set_default: serde_json::Map<String, serde_json::Value>,
}

impl RlmGlobalsPatchPluginBody {
    pub fn is_empty(&self) -> bool {
        self.set_default.is_empty()
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub enum RlmProtocolEvent {
    RlmAssistantContent(RlmAssistantContent),
    RlmTrajectoryEntry(RlmTrajectoryEntry),
    RlmGlobalsPatch(RlmGlobalsPatchPluginBody),
    RlmSeed(RlmSeedPluginBody),
    RlmDiagnostic(RlmDiagnosticEvent),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RlmDiagnosticEvent {
    pub phase: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RlmTermination {
    FinishRequired {
        schema: Option<serde_json::Value>,
    },
    #[default]
    Natural,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RlmFinalAnswerFormat {
    Markdown,
    Custom { guidance: String },
    RawFinalValue,
}

/// RLM protocol session config. Natural turns finish with prose-only model
/// responses or the RLM language's explicit `finish` operation. Programmatic
/// turns can require an explicit finish value, optionally validated against a schema.
/// `final_answer_format` is a session presentation preference; schema-required
/// turns ignore it.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RlmCreateExtras {
    /// Session-wide termination requirement. Absence is the `Natural` default.
    ///
    /// Absence is a distinct statement from an explicit `Natural`: options that
    /// say nothing about termination must leave a recorded `FinishRequired`
    /// alone, and only a value that is genuinely stated participates in the
    /// set-if-unset guard (ADR 0066).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination: Option<RlmTermination>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_answer_format: Option<RlmFinalAnswerFormat>,
}

/// The RLM options a *single turn* may restate (FIG-1979).
///
/// There is no language choice to restate: TypeScript is the sole RLM dialect
/// and nothing — a turn bag, a session bag, a create contract — names one.
///
/// Unstated fields are omitted from the wire, not written as `null`: the
/// per-turn bag is merged over the session bag key by key, so a serialized
/// absence would clobber a recorded session value.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RlmTurnOptions {
    /// Termination requirement for this turn. Absence is the `Natural`
    /// default, and leaves whatever the session recorded alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination: Option<RlmTermination>,
    /// Presentation preference for this turn's final answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_answer_format: Option<RlmFinalAnswerFormat>,
}

impl RlmTurnOptions {
    /// The termination this bag means, resolving absence to the default.
    pub fn effective_termination(&self) -> RlmTermination {
        self.termination.clone().unwrap_or_default()
    }
}

/// The durable RLM facts a session has recorded, read as recorded.
///
/// Every field is `Option`-shaped on purpose: `None` means the session has
/// stated nothing yet, which is a different answer from the value the default
/// resolves to. A host that cannot tell those apart has to peek at raw payload
/// keys to label a fresh session honestly — the hack this type replaces.
///
/// This is the read half of the ADR 0066 pair; the write half is a guarded
/// set-if-unset that refuses with [`RlmSessionConfigConflict`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmSessionConfig {
    pub final_answer_format: Option<RlmFinalAnswerFormat>,
    pub termination: Option<RlmTermination>,
}

impl RlmSessionConfig {
    /// An empty request, stating nothing.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn final_answer_format(mut self, format: RlmFinalAnswerFormat) -> Self {
        self.final_answer_format = Some(format);
        self
    }

    pub fn termination(mut self, termination: RlmTermination) -> Self {
        self.termination = Some(termination);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.final_answer_format.is_none() && self.termination.is_none()
    }
}

impl From<&RlmCreateExtras> for RlmSessionConfig {
    fn from(extras: &RlmCreateExtras) -> Self {
        Self {
            final_answer_format: extras.final_answer_format.clone(),
            termination: extras.termination.clone(),
        }
    }
}

impl From<&RlmSessionConfig> for RlmCreateExtras {
    fn from(config: &RlmSessionConfig) -> Self {
        Self {
            termination: config.termination.clone(),
            final_answer_format: config.final_answer_format.clone(),
        }
    }
}

/// A guarded write refused because the session already recorded a different
/// value for that fact.
///
/// This is the one typed refusal for the durable RLM bag. Hosts match the
/// variant and its `recorded` / `requested` values; the message below is the
/// single place any prose for it is produced, so no caller ever has to match on
/// a string to tell a pin conflict from an unrelated failure.
#[derive(Clone, Debug, PartialEq)]
pub enum RlmSessionConfigConflict {
    FinalAnswerFormat {
        recorded: RlmFinalAnswerFormat,
        requested: RlmFinalAnswerFormat,
    },
    Termination {
        recorded: Box<RlmTermination>,
        requested: Box<RlmTermination>,
    },
}

impl RlmSessionConfigConflict {
    /// The durable fact that was already pinned.
    pub fn field(&self) -> &'static str {
        match self {
            Self::FinalAnswerFormat { .. } => "final_answer_format",
            Self::Termination { .. } => "termination",
        }
    }
}

impl std::fmt::Display for RlmSessionConfigConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (recorded, requested) = match self {
            Self::FinalAnswerFormat {
                recorded,
                requested,
            } => (format!("{recorded:?}"), format!("{requested:?}")),
            Self::Termination {
                recorded,
                requested,
            } => (format!("{recorded:?}"), format!("{requested:?}")),
        };
        write!(
            f,
            "RLM session {} is durably pinned to `{recorded}` and cannot be set to `{requested}`",
            self.field()
        )
    }
}

impl std::error::Error for RlmSessionConfigConflict {}

/// Durable identity for a host projection that can be resolved in another
/// process-local RLM runtime.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProjectionRef {
    pub kind: String,
    pub key: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descriptor_type: Option<String>,
}

impl Eq for ProjectionRef {}

impl ProjectionRef {
    pub fn new(kind: impl Into<String>, key: serde_json::Value) -> Self {
        Self {
            kind: kind.into(),
            key,
            descriptor_type: None,
        }
    }

    pub fn with_descriptor_type(mut self, descriptor_type: impl Into<String>) -> Self {
        self.descriptor_type = Some(descriptor_type.into());
        self
    }
}

/// One durable projected seed binding.
///
/// The explicit tag makes projection references disjoint from ordinary JSON:
/// materialized data can spell any object keys without acquiring reference
/// semantics during restore.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RlmProjectedSeedEntry {
    Materialized(serde_json::Value),
    Ref(ProjectionRef),
}

/// Wire-format snapshot of a set of projected bindings. Pairs of
/// `(name, entry)` get re-projected as host bindings on the child session at
/// creation time. This is the serializable form of
/// `lash_protocol_rlm::RlmProjectedBindings`; lash-rlm-types stays free of any
/// runtime dependency on lashlang itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RlmProjectedSeedSnapshot {
    pub entries: Vec<(String, RlmProjectedSeedEntry)>,
}

impl RlmProjectedSeedSnapshot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, name: impl Into<String>, entry: RlmProjectedSeedEntry) {
        self.entries.push((name.into(), entry));
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RlmSeedPluginBody {
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub globals: serde_json::Map<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "RlmProjectedSeedSnapshot::is_empty")]
    pub projected: RlmProjectedSeedSnapshot,
}

impl RlmSeedPluginBody {
    pub fn is_empty(&self) -> bool {
        self.globals.is_empty() && self.projected.is_empty()
    }
}

/// Reserved JSON key used as the canonical wire encoding for
/// `lashlang::Value::Projected` across the lashlang→host bridge. When the
/// model passes a projected source as a tool argument, lashlang serializes it
/// as `{"__projected__": <tagged seed entry>}`.
pub const PROJECTED_JSON_TAG: &str = "__projected__";

#[cfg(test)]
mod projected_seed_tests {
    use super::*;

    #[test]
    fn projected_seed_entry_serde_is_tagged_and_rejects_the_legacy_shape() {
        let entry = RlmProjectedSeedEntry::Materialized(serde_json::json!({
            "__projection_ref__": {
                "kind": "memory",
                "key": "data",
            }
        }));

        assert_eq!(
            serde_json::to_value(&entry).expect("serialize seed entry"),
            serde_json::json!({
                "kind": "materialized",
                "value": {
                    "__projection_ref__": {
                        "kind": "memory",
                        "key": "data",
                    }
                }
            })
        );
        assert!(
            serde_json::from_value::<RlmProjectedSeedEntry>(serde_json::json!({
                "__projection_ref__": {
                    "kind": "memory",
                    "key": "data",
                }
            }))
            .is_err(),
            "the untagged legacy seed entry must not decode"
        );
    }
}

#[derive(Clone, Debug)]
pub struct RlmTurnProtocol;

impl TurnProtocol for RlmTurnProtocol {
    type Event = RlmProtocolEvent;
    type Termination = RlmTermination;
    type DriverState = serde_json::Value;
}

#[cfg(test)]
mod turn_options_tests {
    use super::{RlmTermination, RlmTurnOptions};

    /// No options bag writes a language key: there is one RLM dialect, so
    /// there is nothing for a turn or a session to state.
    #[test]
    fn a_turn_bag_never_writes_a_dialect_key() {
        let encoded = serde_json::to_string(&RlmTurnOptions {
            termination: Some(RlmTermination::Natural),
            final_answer_format: None,
        })
        .expect("encode");
        assert!(!encoded.contains("dialect"), "{encoded}");
        assert!(!encoded.contains("final_answer_format"), "{encoded}");
    }

    #[test]
    fn an_unstated_termination_is_the_natural_default() {
        assert_eq!(
            RlmTurnOptions::default().effective_termination(),
            RlmTermination::Natural
        );
    }
}
