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

impl RlmHistoryItem {
    /// Build the model-visible step from its persisted protocol entry.
    ///
    /// The image representations intentionally remain distinct: persisted
    /// entries retain attachment references, while history items expose the
    /// compact image metadata used by the model.
    pub fn from_trajectory_entry(entry: &RlmTrajectoryEntry, images: Vec<RlmImageRef>) -> Self {
        Self::LashlangStep {
            id: entry.id.clone(),
            protocol_iteration: entry.protocol_iteration,
            code: entry.code.clone(),
            output: entry.output.clone(),
            images,
            calls: entry.calls.clone(),
            calls_omitted: entry.calls_omitted,
            error: entry.error.clone(),
            final_output: entry.final_output.clone(),
        }
    }
}

#[cfg(test)]
mod rlm_step_serde_tests {
    use std::fmt;

    use serde::de::{IgnoredAny, MapAccess, Visitor};
    use serde::{Deserializer as _, Serialize};

    use super::{RlmHistoryItem, RlmImageRef, RlmTrajectoryEntry};

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
                type_metadata: None,
                label: Some("plot".to_string()),
            }],
            calls: vec![lash_sansio::ExecutedCallRecord {
                operation: "math.add".to_string(),
                outcome: lash_sansio::ExecutedCallOutcome::Ok,
            }],
            calls_omitted: 2,
            error: Some("not really an error".to_string()),
            final_output: Some(serde_json::json!({"answer": 42})),
        }
    }

    fn populated_images() -> Vec<RlmImageRef> {
        vec![RlmImageRef {
            id: "image-1".to_string(),
            media_type: "image/png".parse().expect("valid media type"),
            width: Some(640),
            height: Some(480),
            bytes: 42,
            label: Some("plot".to_string()),
        }]
    }

    #[test]
    fn trajectory_and_history_step_serde_shapes_stay_in_parity() {
        let entry = populated_entry();
        let history = RlmHistoryItem::from_trajectory_entry(&entry, populated_images());

        let expected_shared_fields = [
            "id",
            "protocol_iteration",
            "code",
            "output",
            "calls",
            "calls_omitted",
            "error",
            "final_output",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        assert_eq!(shared_field_order(&entry), expected_shared_fields);
        assert_eq!(shared_field_order(&history), expected_shared_fields);

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
            error: None,
            final_output: None,
        };
        let sparse_history = RlmHistoryItem::from_trajectory_entry(&sparse_entry, Vec::new());
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
    fn legacy_observations_decode_with_matching_defaults() {
        let legacy_entry: RlmTrajectoryEntry = serde_json::from_value(serde_json::json!({
            "id": "legacy-step",
            "protocol_iteration": 4,
            "code": "print('legacy')",
            "observations": ["legacy output"],
        }))
        .expect("legacy trajectory entry decodes");
        assert_eq!(legacy_entry.output, vec!["legacy output"]);
        assert!(legacy_entry.images.is_empty());
        assert!(legacy_entry.calls.is_empty());
        assert_eq!(legacy_entry.calls_omitted, 0);
        assert!(legacy_entry.error.is_none());
        assert!(legacy_entry.final_output.is_none());

        let legacy_history: RlmHistoryItem = serde_json::from_value(serde_json::json!({
            "kind": "lashlang_step",
            "id": "legacy-step",
            "protocol_iteration": 4,
            "code": "print('legacy')",
            "observations": ["legacy output"],
        }))
        .expect("legacy history item decodes");
        let RlmHistoryItem::LashlangStep {
            id,
            protocol_iteration,
            code,
            output,
            images,
            calls,
            calls_omitted,
            error,
            final_output,
        } = legacy_history
        else {
            panic!("legacy payload decoded to the wrong history item");
        };
        assert_eq!(id, legacy_entry.id);
        assert_eq!(protocol_iteration, legacy_entry.protocol_iteration);
        assert_eq!(code, legacy_entry.code);
        assert_eq!(output, legacy_entry.output);
        assert!(images.is_empty());
        assert!(calls.is_empty());
        assert_eq!(calls_omitted, 0);
        assert!(error.is_none());
        assert!(final_output.is_none());
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

    /// Resolve the ambient dialect statement from `LASH_RUNBOOK_DIALECT`.
    ///
    /// Every shipped host reads the ambient dialect from the same variable, so
    /// the parse and its refusal wording live here once: a host that spelled
    /// its own copy would drift on the day a dialect is added, and the drift
    /// shows up as one host accepting an id another refuses.
    ///
    /// `Ok(None)` is "the operator stated nothing". What that means is host
    /// policy, spelled in one visible line at the call site, not a second copy
    /// of this parse. An unregistered id is an `Err` carrying the operator-
    /// facing refusal, never a silent substitution: the dialect is pinned for
    /// the session's whole durable lifetime, so a typo that quietly selected a
    /// dialect would pin the wrong one.
    pub fn from_env() -> Result<Option<Self>, String> {
        parse_dialect_env(std::env::var(DIALECT_ENV).ok().as_deref())
    }
}

/// The environment variable every shipped host reads its ambient dialect from.
const DIALECT_ENV: &str = "LASH_RUNBOOK_DIALECT";

/// The parse behind [`RlmDialect::from_env`], split from the environment read
/// so the table of answers is testable without mutating process state.
fn parse_dialect_env(stated: Option<&str>) -> Result<Option<RlmDialect>, String> {
    let Some(stated) = stated else {
        return Ok(None);
    };
    RlmDialect::from_language_id(stated)
        .map(Some)
        .ok_or_else(|| {
            format!(
                "{DIALECT_ENV} must be a registered RLM language id ({}), got `{stated}`",
                RlmDialect::registered_language_ids()
            )
        })
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
    /// set-if-unset guard (ADR 0066).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination: Option<RlmTermination>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_answer_format: Option<RlmFinalAnswerFormat>,
}

/// The RLM options a *single turn* may restate (FIG-1979).
///
/// The dialect is deliberately absent. It is resolved once, at session
/// materialization, from the session's own recorded config (ADR 0066), and the
/// executor never consults a per-turn value. While the turn bag carried a
/// `dialect` field, a turn could name one dialect while its cells ran another
/// and a host reading the turn bag would print the wrong language's vocabulary;
/// with the field gone that disagreement is unrepresentable rather than
/// merely unused. Hosts that need the running language read it from the
/// session config instead — `lash_protocol_rlm::rlm_session_dialect`.
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
        self.dialect.is_none() && self.final_answer_format.is_none() && self.termination.is_none()
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
mod dialect_env_tests {
    use super::{DIALECT_ENV, RlmDialect, parse_dialect_env};

    /// The whole answer table for the ambient variable, in one place.
    ///
    /// Absence is `None` — a statement of nothing that each host maps to its
    /// own policy — every registered id resolves, and anything else refuses
    /// with a message naming the variable and the ids that exist.
    #[test]
    fn the_ambient_variable_answers_one_table() {
        assert_eq!(parse_dialect_env(None), Ok(None));
        assert_eq!(
            parse_dialect_env(Some("lashlang")),
            Ok(Some(RlmDialect::Lashlang))
        );
        assert_eq!(
            parse_dialect_env(Some("typescript")),
            Ok(Some(RlmDialect::Typescript))
        );
        let refusal = parse_dialect_env(Some("lashscript")).expect_err("an unknown id refuses");
        assert_eq!(
            refusal,
            "LASH_RUNBOOK_DIALECT must be a registered RLM language id \
             (`lashlang`, `typescript`), got `lashscript`"
        );
    }

    /// Every registered dialect is selectable by its own registered id: the
    /// menu a host offers and the ids this parse accepts are one list.
    #[test]
    fn every_registered_dialect_resolves_from_its_language_id() {
        for dialect in RlmDialect::ALL {
            assert_eq!(
                parse_dialect_env(Some(dialect.language_id())),
                Ok(Some(dialect))
            );
        }
    }

    /// An empty variable is a value the operator set, not an absence, so it
    /// refuses rather than resolving to the default.
    #[test]
    fn an_empty_ambient_value_refuses() {
        let refusal = parse_dialect_env(Some("")).expect_err("an empty value refuses");
        assert!(refusal.contains(DIALECT_ENV), "{refusal}");
    }
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

#[cfg(test)]
mod turn_options_tests {
    use super::{RlmFinalAnswerFormat, RlmTermination, RlmTurnOptions};

    /// FIG-1979: a turn cannot name a dialect. The type has no field for it, so
    /// the only way a `dialect` key reaches this bag is the session bag it is
    /// merged over — and that key is read from the session config, never from
    /// here.
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

    /// The bag decodes off the *merged* session⊕turn payload, so a session's
    /// recorded dialect key must be carried past rather than refused.
    #[test]
    fn a_merged_payload_with_a_session_dialect_still_decodes() {
        let options: RlmTurnOptions = serde_json::from_str(
            r#"{"dialect":"typescript","termination":{"kind":"natural"},"final_answer_format":{"kind":"markdown"}}"#,
        )
        .expect("merged payload decodes");
        assert_eq!(options.effective_termination(), RlmTermination::Natural);
        assert_eq!(
            options.final_answer_format,
            Some(RlmFinalAnswerFormat::Markdown)
        );
    }

    #[test]
    fn an_unstated_termination_is_the_natural_default() {
        assert_eq!(
            RlmTurnOptions::default().effective_termination(),
            RlmTermination::Natural
        );
    }
}
