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
    /// Every dialect the first-party registry can activate, in id order.
    ///
    /// A host that offers a dialect choice — a create form, a CLI flag, an
    /// environment variable — must offer *these*, not a list it writes itself:
    /// a hand-written list is a second source of truth that goes stale the day
    /// a dialect is added, and the failure it produces is an operator being
    /// unable to select a dialect the substrate runs.
    /// `lash-protocol-rlm` checks this array against the registry's own
    /// dialects, so adding one that is not listed here fails that crate's
    /// tests rather than shipping a menu with a hole in it.
    pub const ALL: [Self; 2] = [Self::Lashlang, Self::Typescript];

    /// Return the registered code-execution language id for this dialect.
    pub const fn language_id(self) -> &'static str {
        match self {
            Self::Lashlang => "lashlang",
            Self::Typescript => "typescript",
        }
    }

    /// Resolve a registered language id, refusing an unknown one.
    ///
    /// This is the typed create-contract refusal in the shape a host receives
    /// the choice in: a string off a form, a flag, or an environment variable.
    /// Returning `None` rather than defaulting is the whole point — a typo that
    /// silently selected Lashlang would pin the wrong dialect for the session's
    /// durable lifetime.
    pub fn from_language_id(language_id: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|dialect| dialect.language_id() == language_id)
    }

    /// The registered language ids, comma-separated, for a refusal message.
    pub fn registered_language_ids() -> String {
        Self::ALL
            .iter()
            .map(|dialect| format!("`{}`", dialect.language_id()))
            .collect::<Vec<_>>()
            .join(", ")
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
    ///
    /// An *explicit* `null` is refused rather than read as absence: see
    /// [`reject_explicit_null_dialect`].
    #[serde(
        default,
        deserialize_with = "reject_explicit_null_dialect",
        skip_serializing_if = "Option::is_none"
    )]
    pub dialect: Option<RlmDialect>,
    /// Session-wide termination requirement. Absence is the `Natural` default.
    ///
    /// Absence is a distinct statement from an explicit `Natural`: options that
    /// say nothing about termination must leave a recorded `FinishRequired`
    /// alone, and only a value that is genuinely stated participates in the
    /// set-if-unset guard (ADR 0064).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination: Option<RlmTermination>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_answer_format: Option<RlmFinalAnswerFormat>,
}

impl RlmCreateExtras {
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
/// This is the read half of the ADR 0064 pair; the write half is a guarded
/// set-if-unset that refuses with [`RlmSessionConfigConflict`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmSessionConfig {
    pub dialect: Option<RlmDialect>,
    pub final_answer_format: Option<RlmFinalAnswerFormat>,
    pub termination: Option<RlmTermination>,
}

impl RlmSessionConfig {
    /// An empty request, stating nothing.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn dialect(mut self, dialect: RlmDialect) -> Self {
        self.dialect = Some(dialect);
        self
    }

    pub fn final_answer_format(mut self, format: RlmFinalAnswerFormat) -> Self {
        self.final_answer_format = Some(format);
        self
    }

    pub fn termination(mut self, termination: RlmTermination) -> Self {
        self.termination = Some(termination);
        self
    }

    /// Whether this states nothing at all.
    pub fn is_empty(&self) -> bool {
        self.dialect.is_none()
            && self.final_answer_format.is_none()
            && self.termination.is_none()
    }
}

impl From<&RlmCreateExtras> for RlmSessionConfig {
    fn from(extras: &RlmCreateExtras) -> Self {
        Self {
            dialect: extras.dialect,
            final_answer_format: extras.final_answer_format.clone(),
            termination: extras.termination.clone(),
        }
    }
}

impl From<&RlmSessionConfig> for RlmCreateExtras {
    fn from(config: &RlmSessionConfig) -> Self {
        Self {
            dialect: config.dialect,
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
    Dialect {
        recorded: RlmDialect,
        requested: RlmDialect,
    },
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
            Self::Dialect { .. } => "dialect",
            Self::FinalAnswerFormat { .. } => "final_answer_format",
            Self::Termination { .. } => "termination",
        }
    }
}

impl std::fmt::Display for RlmSessionConfigConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (recorded, requested) = match self {
            Self::Dialect {
                recorded,
                requested,
            } => (
                recorded.language_id().to_string(),
                requested.language_id().to_string(),
            ),
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

/// Reads a present `dialect` key, refusing an explicit `null`.
///
/// Absence and `null` are the same value to serde by default, and that made
/// `null` the one tampered shape that did not fail closed: an unknown id, a
/// case-drifted id and a junk extra key are all refused, but a `null` silently
/// downgraded a recorded TypeScript session to the Lashlang default. Absence
/// has to keep meaning Lashlang — that is how every pre-layer session decodes —
/// so the two cases must be told apart rather than merged. Serde only calls
/// this when the key is present, so absence still takes the `default`.
fn reject_explicit_null_dialect<'de, D>(deserializer: D) -> Result<Option<RlmDialect>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    use serde::de::Error as _;

    match Option::<RlmDialect>::deserialize(deserializer)? {
        Some(dialect) => Ok(Some(dialect)),
        None => Err(D::Error::custom(
            "`dialect` is present but null; omit the key for the Lashlang default",
        )),
    }
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

#[cfg(test)]
mod dialect_serde_tests {
    use super::{RlmCreateExtras, RlmDialect};

    /// Absence is the pre-layer compatibility answer and must stay Lashlang.
    #[test]
    fn an_absent_dialect_is_the_lashlang_default() {
        let extras: RlmCreateExtras = serde_json::from_str("{}").expect("pre-layer state decodes");
        assert_eq!(extras.dialect, None);
    }

    /// An explicit `null` is not the same statement as saying nothing.
    ///
    /// Every other tampered value fails closed: an unknown id, a case-drifted
    /// id, a junk extra key. `null` was the one shape that silently downgraded
    /// a recorded TypeScript session to Lashlang, because serde cannot tell it
    /// from an absent key by default. The field is never written as `null` —
    /// it is skipped when absent — so a `null` in durable state is a store that
    /// has been edited, and failing closed is the same answer the other tamper
    /// shapes already get.
    #[test]
    fn an_explicit_null_dialect_fails_closed() {
        let error = serde_json::from_str::<RlmCreateExtras>(r#"{"dialect":null}"#)
            .expect_err("an explicit null must be refused");
        assert!(
            error.to_string().contains("dialect"),
            "the refusal names the field: {error}"
        );
    }

    #[test]
    fn a_named_dialect_decodes() {
        let extras: RlmCreateExtras =
            serde_json::from_str(r#"{"dialect":"typescript"}"#).expect("named dialect decodes");
        assert_eq!(extras.dialect, Some(RlmDialect::Typescript));
    }

    #[test]
    fn an_unknown_dialect_still_fails_closed() {
        serde_json::from_str::<RlmCreateExtras>(r#"{"dialect":"python"}"#)
            .expect_err("an unknown id must be refused");
    }

    /// The create path is unchanged: `None` still round-trips by being skipped,
    /// so a session that asks for no dialect writes no key and stays decodable.
    #[test]
    fn none_round_trips_as_an_absent_key() {
        let encoded = serde_json::to_string(&RlmCreateExtras::default()).expect("encode");
        assert!(!encoded.contains("dialect"), "{encoded}");
        let decoded: RlmCreateExtras = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded.dialect, None);
    }
}
