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
    #[serde(default, alias = "observations")]
    pub output: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<AttachmentRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<RlmExecutedCall>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub calls_omitted: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_output: Option<serde_json::Value>,
}

pub type RlmExecutedCall = lash_sansio::ExecutedCallRecord;
pub type RlmExecutedCallOutcome = lash_sansio::ExecutedCallOutcome;

fn is_zero(value: &usize) -> bool {
    *value == 0
}

impl RlmTrajectoryEntry {
    /// Total characters across every `print`/output entry, summed.
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
        #[serde(default, alias = "observations")]
        output: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<RlmImageRef>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        calls: Vec<RlmExecutedCall>,
        #[serde(default, skip_serializing_if = "is_zero")]
        calls_omitted: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_output: Option<serde_json::Value>,
    },
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

/// Source language pinned to an RLM session for its entire durable lifetime.
///
/// The serialized names are the language ids registered by the first-party RLM
/// dialect registry. Keeping this an enum makes an unknown language a typed
/// create-contract error instead of a late execution failure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RlmDialect {
    /// The default RLM language when a host omits the field.
    #[default]
    Lashlang,
    /// The ECMA-exact TypeScript dialect.
    Typescript,
}

impl RlmDialect {
    /// Return the registered code-execution language id for this dialect.
    pub const fn language_id(self) -> &'static str {
        match self {
            Self::Lashlang => "lashlang",
            Self::Typescript => "typescript",
        }
    }
}

/// RLM protocol session config. Natural turns finish with prose-only model
/// responses or the active dialect's explicit `finish` operation. Programmatic
/// turns can require an explicit finish value, optionally validated against a schema.
/// `final_answer_format` is a session presentation preference; schema-required
/// turns ignore it.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RlmCreateExtras {
    /// Session-wide language choice. Absence is the ratified Lashlang default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialect: Option<RlmDialect>,
    #[serde(default)]
    pub termination: RlmTermination,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_answer_format: Option<RlmFinalAnswerFormat>,
}

/// Wire-format snapshot of a set of projected bindings. Pairs of
/// `(name, json_value)` that get re-projected as host bindings on the child
/// session at creation time. This is the serializable form of
/// `lash_protocol_rlm::RlmProjectedBindings`; lash-rlm-types stays free of any
/// runtime dependency on lashlang itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RlmProjectedSeedSnapshot {
    pub entries: Vec<(String, serde_json::Value)>,
}

impl RlmProjectedSeedSnapshot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, name: impl Into<String>, value: serde_json::Value) {
        self.entries.push((name.into(), value));
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
/// as `{"__projected__": <inner>}`.
pub const PROJECTED_JSON_TAG: &str = "__projected__";
pub const PROJECTION_REF_JSON_TAG: &str = "__projection_ref__";

/// Returns the inner JSON value if `value` is the canonical projection wrapper
/// (a single-key object whose key is [`PROJECTED_JSON_TAG`]), else `None`.
pub fn projection_inner(value: &serde_json::Value) -> Option<&serde_json::Value> {
    let obj = value.as_object()?;
    if obj.len() != 1 {
        return None;
    }
    obj.get(PROJECTED_JSON_TAG)
}

pub fn projection_ref_inner(value: &serde_json::Value) -> Option<&serde_json::Value> {
    let obj = value.as_object()?;
    if obj.len() != 1 {
        return None;
    }
    obj.get(PROJECTION_REF_JSON_TAG)
}

#[derive(Clone, Debug)]
pub struct RlmTurnProtocol;

impl TurnProtocol for RlmTurnProtocol {
    type Event = RlmProtocolEvent;
    type Termination = RlmTermination;
    type DriverState = serde_json::Value;
}
