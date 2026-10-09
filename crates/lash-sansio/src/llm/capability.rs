//! Host-supplied model capability metadata.
//!
//! Capability is data the host attaches to a model spec and threads onto every
//! [`LlmRequest`](crate::llm::types::LlmRequest). Lash validates a requested
//! effort against it and resolves it into one [`ReasoningIntent`] that each
//! provider maps onto its wire. Providers consume capability; they never
//! produce it.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Capability metadata for a single model on a route, supplied by the host.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LlmProfileCapability {
    /// Native instruction role on Responses, Codex, and Chat Completions.
    #[serde(default, skip_serializing_if = "InstructionRole::is_system")]
    pub instruction_role: InstructionRole,
    /// Anthropic native runtime feedback at legal conversation positions.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub native_mid_conversation_system: bool,
    /// Google wire dialect selected by the host for this route.
    #[serde(default, skip_serializing_if = "GoogleDialect::is_legacy")]
    pub google_dialect: GoogleDialect,
    /// Cache-control wire dialect accepted by this model on its selected route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControlDialect>,
    /// How a streaming provider proves that this model's response completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_termination: Option<StreamTermination>,
    /// Whether this model lets a caller set the sampling temperature.
    #[serde(default, skip_serializing_if = "SamplingCapability::is_default")]
    pub sampling: SamplingCapability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningCapability>,
    /// Host-selected reasoning-history retention, independent of reasoning
    /// effort. Providers consume this exact capability/selection pair; they do
    /// not infer support from model identifiers or approximate other units.
    #[serde(default, skip_serializing_if = "ReasoningRetentionPolicy::is_default")]
    pub reasoning_retention: Box<ReasoningRetentionPolicy>,
}

/// One provider-native retention primitive, or the conservative fallback for
/// routes whose API has no native reasoning-history control.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningRetentionCapability {
    OpenAiContext {
        /// Exact `reasoning.context` values accepted by the selected route.
        supported: Vec<OpenAiReasoningContext>,
    },
    AnthropicClearThinking,
    ClientSideUserSegments,
}

/// OpenAI Responses reasoning-history policy. These are provider wire values,
/// not Lash approximations of token or turn budgets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiReasoningContext {
    CurrentTurn,
    AllTurns,
}

impl OpenAiReasoningContext {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CurrentTurn => "current_turn",
            Self::AllTurns => "all_turns",
        }
    }
}

/// Anthropic native thinking retention. The count is Anthropic thinking turns,
/// deliberately distinct from Lash genuine-user segments.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AnthropicThinkingRetention {
    All,
    Turns(std::num::NonZeroU32),
}

/// Exact host choice for reasoning-history retention.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningRetentionSelection {
    #[default]
    ProviderDefault,
    OpenAiContext {
        context: OpenAiReasoningContext,
    },
    AnthropicClearThinking {
        keep: AnthropicThinkingRetention,
    },
    ClientSideUserSegments {
        max_segments: std::num::NonZeroUsize,
    },
}

/// Host-visible capability plus selection. Keeping both in the durable model
/// snapshot makes cold reopen and remote execution apply the same contract.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReasoningRetentionPolicy {
    pub capability: Option<ReasoningRetentionCapability>,
    pub selection: ReasoningRetentionSelection,
}

impl ReasoningRetentionPolicy {
    pub fn is_default(&self) -> bool {
        self.capability.is_none() && self.selection == ReasoningRetentionSelection::ProviderDefault
    }
}

/// The primitive implemented by an adapter. It is determined by the provider
/// protocol, never by a model-name heuristic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderReasoningRetentionSupport {
    OpenAiContext,
    AnthropicClearThinking,
    ClientSideUserSegments,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningRetentionValidationCategory {
    UnsupportedSelection,
    MalformedCapability,
    InvalidHistory,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningRetentionValidationError {
    pub category: ReasoningRetentionValidationCategory,
    pub message: String,
}

impl std::fmt::Display for ReasoningRetentionValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ReasoningRetentionValidationError {}

/// Host-supplied instruction role; model identifiers never select authority.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstructionRole {
    #[default]
    System,
    Developer,
}
impl InstructionRole {
    pub fn is_system(&self) -> bool {
        *self == Self::System
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Developer => "developer",
        }
    }
}

/// Host-supplied Google wire dialect; model identifiers never select it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GoogleDialect {
    #[default]
    Legacy,
    Gemini3,
    ClaudeOnVertex,
}
impl GoogleDialect {
    pub fn is_legacy(&self) -> bool {
        *self == Self::Legacy
    }
}

/// Whether a model accepts a caller-set sampling temperature at all.
///
/// Some models pin their own sampling and reject any temperature the caller
/// supplies — Anthropic models released after Claude Opus 4.6 answer a
/// non-default temperature with HTTP 400. That is a per-model fact the host
/// knows from its model catalogue, so it travels with the capability rather
/// than being guessed from a model name in an adapter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SamplingCapability {
    /// The model accepts a caller-set temperature.
    #[default]
    Configurable,
    /// The model pins sampling itself; adapters omit temperature entirely.
    Pinned,
}

impl SamplingCapability {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Host-supplied policy for interpreting a clean EOF on a provider stream.
/// A route or model capability that states none tolerates EOF: a stream
/// that ends without its terminal event completes as stopped, as most
/// provider SDKs complete it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StreamTermination {
    /// EOF is successful only after the dialect's semantic terminal event;
    /// a stream that ends before it fails typed, its partial output kept.
    RequireTerminalEvidence,
    /// Clean EOF is a valid completion boundary for this route.
    #[default]
    EofTolerated,
}

/// How an OpenAI-compatible Chat Completions route places `cache_control`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CacheControlDialect {
    /// Anthropic-style canonical placement, including extended TTL support.
    Anthropic,
    /// A single ephemeral breakpoint with no extended TTL.
    Gemini,
}

/// What reasoning/effort the model exposes and how effort maps onto the wire.
///
/// There is no default effort here: a host's default is the model spec's
/// `variant`. Effort names match exactly; lash neither aliases, lowercases nor
/// clamps them to a "nearest" level.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReasoningCapability {
    /// Exact effort names this route accepts, sent verbatim.
    #[serde(default)]
    pub efforts: Vec<String>,
    #[serde(default)]
    pub encoding: ReasoningEncoding,
    /// Whether this route accepts an explicit reasoning-off selection. The
    /// wire form of "off" belongs to the route's dialect, not to this data.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disable: bool,
    #[serde(default)]
    pub mandatory: bool,
}

/// The host-resolved reasoning choice carried from model selection to providers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSelection {
    /// Send no reasoning control; the endpoint's own default applies.
    #[default]
    ProviderDefault,
    /// Explicitly turn reasoning off, where the capability declares `disable`.
    Disabled,
    /// Request a named, capability-validated effort.
    Effort(String),
}

impl ReasoningSelection {
    pub fn effort(&self) -> Option<&str> {
        match self {
            Self::Effort(effort) => Some(effort),
            Self::ProviderDefault | Self::Disabled => None,
        }
    }
}

/// The one dialect-independent reasoning intent a provider maps onto its
/// wire, resolved once from `ReasoningSelection` × `ReasoningCapability`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReasoningIntent {
    /// A named effort, sent verbatim.
    Effort(String),
    /// A reasoning token budget the host mapped from the selected effort.
    Budget(u32),
    /// Reasoning explicitly turned off.
    Off,
}

/// How a resolved effort level is encoded on the wire.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEncoding {
    /// Named effort level sent as-is on the wire (Anthropic adaptive thinking, OpenAI reasoning.effort, Gemini thinkingLevel)
    #[default]
    Effort,
    /// Effort name resolves to a token budget (Anthropic budget thinking, Gemini 2.5 thinkingBudget); map is effort -> tokens
    Budget(BTreeMap<String, u32>),
}

/// Prompt-cache lifetime hint. Providers translate this into their own
/// wire dialect (Anthropic and OpenRouter Claude/Gemini `cache_control`,
/// OpenAI Responses and Codex `prompt_cache_key`, and OpenAI
/// `prompt_cache_retention`). Providers without a cache-control concept,
/// such as direct Google, read the value but emit nothing for it.
/// How long a provider keeps this model's prompt prefix cached, and so what
/// its cache writes cost and how long the provider retains the prompt.
///
/// The host states it for every model; there is no default (D-DEFAULTS2).
/// Providers price the choices differently: a longer lifetime costs more per
/// cache write and pays back only when the prefix is read again inside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CacheRetention {
    /// Off: no prompt-cache directive is sent.
    None,
    /// Cache for the provider's default lifetime: the directive is sent with
    /// no lifetime of its own (Anthropic's ephemeral window, 5 minutes).
    Short,
    /// Cache for the extended lifetime where the route supports one (a 1-hour
    /// TTL on Anthropic's dialect, `24h` on OpenAI Responses); elsewhere as
    /// [`Self::Short`].
    Long,
}

/// How a model's requests behave where a request states nothing (FIG-4374):
/// host intent recorded with the model's metadata when a registry mints its
/// binding, and carried on every request that model serves. A transport
/// keeps only live concerns: its client, credentials, endpoint and limits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LlmProfileRequestDefaults {
    /// Surface the reasoning the provider streams in responses.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub expose_thinking: bool,
    /// The prompt-cache lifetime the host states for this model; see
    /// [`CacheRetention`]. Required and always recorded: a record without it
    /// names no choice and does not decode.
    pub cache_retention: CacheRetention,
    /// Response header names (case-insensitive) captured into
    /// `LlmResponse.response_metadata` as `header:<lowercased-name>` entries.
    /// Headers not named here are never retained. Recorded with the model
    /// like the rest (FIG-4397): what a session's calls capture does not
    /// depend on the worker that serves them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub response_metadata_headers: Vec<String>,
    /// JSON pointers probed against buffered response bodies and every SSE
    /// event. Captured values use `body:<pointer>` keys; unlisted body fields
    /// are never retained.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub response_metadata_body_paths: Vec<String>,
}

impl LlmProfileRequestDefaults {
    /// Request behavior under the prompt-cache lifetime the host states:
    /// thinking is not exposed and no response metadata is captured.
    pub fn new(cache_retention: CacheRetention) -> Self {
        Self {
            expose_thinking: false,
            cache_retention,
            response_metadata_headers: Vec::new(),
            response_metadata_body_paths: Vec::new(),
        }
    }
}

/// Deterministic taxonomy of effort-validation failures. The serde snake_case
/// codes are a stable contract: downstream consumers match on them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LlmProfileEffortValidationCategory {
    UnsupportedEffort,
    EffortNotConfigurable,
    EffortRequired,
    MalformedCapability,
}

impl LlmProfileEffortValidationCategory {
    /// The typed turn-failure code the turn driver surfaces for this
    /// validation category. Its wire spelling matches the serde
    /// representation of this enum.
    pub fn failure_code(&self) -> crate::session_model::TurnFailureCode {
        use crate::session_model::TurnFailureCode as Code;
        match self {
            Self::UnsupportedEffort => Code::UnsupportedEffort,
            Self::EffortNotConfigurable => Code::EffortNotConfigurable,
            Self::EffortRequired => Code::EffortRequired,
            Self::MalformedCapability => Code::MalformedCapability,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmProfileEffortValidationError {
    pub category: LlmProfileEffortValidationCategory,
    pub message: String,
}

impl std::fmt::Display for LlmProfileEffortValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LlmProfileEffortValidationError {}

impl LlmProfileCapability {
    pub fn is_empty(&self) -> bool {
        self.instruction_role.is_system()
            && !self.native_mid_conversation_system
            && self.google_dialect.is_legacy()
            && self.reasoning.is_none()
            && self.cache_control.is_none()
            && self.stream_termination.is_none()
            && self.sampling.is_default()
            && self.reasoning_retention.is_default()
    }

    /// No cross-unit approximation is allowed.
    pub fn validate_reasoning_retention(
        &self,
        model: &str,
        provider_kind: &str,
        adapter_support: ProviderReasoningRetentionSupport,
    ) -> Result<(), ReasoningRetentionValidationError> {
        use ReasoningRetentionCapability as Capability;
        use ReasoningRetentionSelection as Selection;

        let policy = &self.reasoning_retention;
        if policy.selection == Selection::ProviderDefault {
            return Ok(());
        }
        let Some(capability) = policy.capability.as_ref() else {
            return Err(ReasoningRetentionValidationError {
                category: ReasoningRetentionValidationCategory::MalformedCapability,
                message: format!(
                    "Model `{model}` on {provider_kind} selects reasoning retention without a host-supplied retention capability."
                ),
            });
        };

        let supported = matches!(
            (capability, adapter_support),
            (
                Capability::OpenAiContext { .. },
                ProviderReasoningRetentionSupport::OpenAiContext
            ) | (
                Capability::AnthropicClearThinking,
                ProviderReasoningRetentionSupport::AnthropicClearThinking
            ) | (
                Capability::ClientSideUserSegments,
                ProviderReasoningRetentionSupport::ClientSideUserSegments
            )
        );
        if !supported {
            return Err(ReasoningRetentionValidationError {
                category: ReasoningRetentionValidationCategory::UnsupportedSelection,
                message: format!(
                    "Model `{model}` on {provider_kind} cannot apply the selected reasoning-retention primitive."
                ),
            });
        }

        match (&policy.selection, capability) {
            (Selection::OpenAiContext { context }, Capability::OpenAiContext { supported })
                if supported.contains(context) =>
            {
                Ok(())
            }
            (Selection::AnthropicClearThinking { .. }, Capability::AnthropicClearThinking)
            | (Selection::ClientSideUserSegments { .. }, Capability::ClientSideUserSegments) => {
                Ok(())
            }
            (Selection::OpenAiContext { context }, Capability::OpenAiContext { .. }) => {
                Err(ReasoningRetentionValidationError {
                    category: ReasoningRetentionValidationCategory::UnsupportedSelection,
                    message: format!(
                        "Model `{model}` on {provider_kind} does not support OpenAI reasoning.context=`{}`.",
                        context.as_str()
                    ),
                })
            }
            _ => Err(ReasoningRetentionValidationError {
                category: ReasoningRetentionValidationCategory::UnsupportedSelection,
                message: format!(
                    "Model `{model}` on {provider_kind} has a retention selection that does not match its host-supplied capability."
                ),
            }),
        }
    }

    /// Whether an adapter may put a caller-requested temperature on the wire
    /// for this model.
    pub fn allows_caller_temperature(&self) -> bool {
        self.sampling == SamplingCapability::Configurable
    }

    /// Resolve the requested selection against this capability into the
    /// one reasoning intent providers map onto their wires. `Ok(None)` means
    /// the selection is `ProviderDefault`: nothing is sent.
    ///
    /// A budget encoding missing an advertised effort is malformed data and an
    /// error for every selection, never an omission.
    pub fn reasoning_intent(
        &self,
        model: &str,
        provider_kind: &str,
        requested: &ReasoningSelection,
    ) -> Result<Option<ReasoningIntent>, LlmProfileEffortValidationError> {
        if let Some(ReasoningCapability {
            efforts,
            encoding: ReasoningEncoding::Budget(budgets),
            ..
        }) = self.reasoning.as_ref()
            && let Some(missing) = efforts.iter().find(|effort| !budgets.contains_key(*effort))
        {
            return Err(LlmProfileEffortValidationError {
                category: LlmProfileEffortValidationCategory::MalformedCapability,
                message: format!(
                    "Malformed capability for model `{model}` on {provider_kind}: budget encoding is missing advertised effort `{missing}`."
                ),
            });
        }

        match (self.reasoning.as_ref(), requested) {
            (None, ReasoningSelection::Effort(effort)) => Err(LlmProfileEffortValidationError {
                category: LlmProfileEffortValidationCategory::EffortNotConfigurable,
                message: format!(
                    "Model `{model}` on {provider_kind} does not expose configurable effort (requested `{effort}`)."
                ),
            }),
            (None, ReasoningSelection::Disabled) => Err(LlmProfileEffortValidationError {
                category: LlmProfileEffortValidationCategory::EffortNotConfigurable,
                message: format!(
                    "Model `{model}` on {provider_kind} does not expose configurable effort (requested disabled)."
                ),
            }),
            (None, ReasoningSelection::ProviderDefault) => Ok(None),
            (Some(reasoning), ReasoningSelection::ProviderDefault) => {
                if reasoning.mandatory {
                    Err(LlmProfileEffortValidationError {
                        category: LlmProfileEffortValidationCategory::EffortRequired,
                        message: format!(
                            "Model `{model}` on {provider_kind} requires an explicit effort. Available: {}",
                            reasoning.efforts.join(", ")
                        ),
                    })
                } else {
                    Ok(None)
                }
            }
            (Some(reasoning), ReasoningSelection::Disabled) => {
                if reasoning.disable {
                    Ok(Some(ReasoningIntent::Off))
                } else {
                    Err(LlmProfileEffortValidationError {
                        category: LlmProfileEffortValidationCategory::UnsupportedEffort,
                        message: format!(
                            "Model `{model}` on {provider_kind} does not support disabling reasoning."
                        ),
                    })
                }
            }
            (Some(reasoning), ReasoningSelection::Effort(effort)) => {
                if reasoning.efforts.is_empty() {
                    return Err(LlmProfileEffortValidationError {
                        category: LlmProfileEffortValidationCategory::EffortNotConfigurable,
                        message: format!(
                            "Model `{model}` on {provider_kind} does not expose configurable effort (requested `{effort}`)."
                        ),
                    });
                }
                if !reasoning.efforts.contains(effort) {
                    return Err(LlmProfileEffortValidationError {
                        category: LlmProfileEffortValidationCategory::UnsupportedEffort,
                        message: format!(
                            "Unsupported effort `{effort}` for `{model}` on {provider_kind}. Available: {}",
                            reasoning.efforts.join(", ")
                        ),
                    });
                }
                match &reasoning.encoding {
                    ReasoningEncoding::Effort => Ok(Some(ReasoningIntent::Effort(effort.clone()))),
                    // Completeness was checked above, so every advertised
                    // effort has a budget.
                    ReasoningEncoding::Budget(budgets) => budgets
                        .get(effort)
                        .map(|budget| Some(ReasoningIntent::Budget(*budget)))
                        .ok_or_else(|| LlmProfileEffortValidationError {
                            category: LlmProfileEffortValidationCategory::MalformedCapability,
                            message: format!(
                                "Malformed capability for model `{model}` on {provider_kind}: budget encoding is missing advertised effort `{effort}`."
                            ),
                        }),
                }
            }
        }
    }

    /// Validate a requested selection against this capability at a runtime
    /// seam. The selection travels unchanged: validation never rewrites it.
    pub fn validate_selection(
        &self,
        model: &str,
        provider_kind: &str,
        requested: &ReasoningSelection,
    ) -> Result<(), LlmProfileEffortValidationError> {
        self.reasoning_intent(model, provider_kind, requested)
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn efforts() -> Vec<String> {
        ["low", "medium", "high", "max"]
            .into_iter()
            .map(String::from)
            .collect()
    }

    fn reasoning() -> ReasoningCapability {
        ReasoningCapability {
            efforts: efforts(),
            encoding: ReasoningEncoding::Effort,
            disable: true,
            mandatory: false,
        }
    }

    fn capability(reasoning: Option<ReasoningCapability>) -> LlmProfileCapability {
        LlmProfileCapability {
            instruction_role: Default::default(),
            native_mid_conversation_system: false,
            google_dialect: Default::default(),
            reasoning,
            cache_control: None,
            stream_termination: None,
            sampling: SamplingCapability::Configurable,
            reasoning_retention: Box::default(),
        }
    }

    #[test]
    fn effort_names_match_exactly_without_aliases_case_folding_or_clamping() {
        let cap = capability(Some(reasoning()));
        for near_miss in ["High", " high"] {
            let error = cap
                .reasoning_intent(
                    "m",
                    "test",
                    &ReasoningSelection::Effort(near_miss.to_string()),
                )
                .expect_err(near_miss);
            assert_eq!(
                error.category,
                LlmProfileEffortValidationCategory::UnsupportedEffort,
                "{near_miss}"
            );
        }
    }

    #[test]
    fn reasoning_intent_classifier_covers_ratified_table() {
        struct Case {
            name: &'static str,
            capability: LlmProfileCapability,
            selection: ReasoningSelection,
            expected: Result<Option<ReasoningIntent>, LlmProfileEffortValidationCategory>,
        }

        let mut cannot_disable = reasoning();
        cannot_disable.disable = false;
        let mut mandatory = reasoning();
        mandatory.mandatory = true;
        let mut budget = reasoning();
        budget.encoding = ReasoningEncoding::Budget(BTreeMap::from([
            ("low".to_string(), 1024),
            ("medium".to_string(), 4096),
            ("high".to_string(), 8192),
            ("max".to_string(), 16384),
        ]));
        let mut no_efforts = reasoning();
        no_efforts.efforts.clear();

        let cases = [
            Case {
                name: "default",
                capability: capability(Some(reasoning())),
                selection: ReasoningSelection::ProviderDefault,
                expected: Ok(None),
            },
            Case {
                name: "effort",
                capability: capability(Some(reasoning())),
                selection: ReasoningSelection::Effort("high".to_string()),
                expected: Ok(Some(ReasoningIntent::Effort("high".to_string()))),
            },
            Case {
                name: "budget",
                capability: capability(Some(budget)),
                selection: ReasoningSelection::Effort("medium".to_string()),
                expected: Ok(Some(ReasoningIntent::Budget(4096))),
            },
            Case {
                name: "disabled",
                capability: capability(Some(reasoning())),
                selection: ReasoningSelection::Disabled,
                expected: Ok(Some(ReasoningIntent::Off)),
            },
            Case {
                name: "disabled_unsupported",
                capability: capability(Some(cannot_disable)),
                selection: ReasoningSelection::Disabled,
                expected: Err(LlmProfileEffortValidationCategory::UnsupportedEffort),
            },
            Case {
                name: "no_reasoning",
                capability: capability(None),
                selection: ReasoningSelection::Effort("low".to_string()),
                expected: Err(LlmProfileEffortValidationCategory::EffortNotConfigurable),
            },
            Case {
                name: "no_reasoning_default",
                capability: capability(None),
                selection: ReasoningSelection::ProviderDefault,
                expected: Ok(None),
            },
            Case {
                name: "no_efforts",
                capability: capability(Some(no_efforts)),
                selection: ReasoningSelection::Effort("low".to_string()),
                expected: Err(LlmProfileEffortValidationCategory::EffortNotConfigurable),
            },
            Case {
                name: "mandatory_without_selection",
                capability: capability(Some(mandatory)),
                selection: ReasoningSelection::ProviderDefault,
                expected: Err(LlmProfileEffortValidationCategory::EffortRequired),
            },
        ];

        for case in cases {
            let actual = case
                .capability
                .reasoning_intent("m", "test", &case.selection)
                .map_err(|error| error.category);
            assert_eq!(actual, case.expected, "{}", case.name);
            assert_eq!(
                case.capability
                    .validate_selection("m", "test", &case.selection)
                    .map_err(|error| error.category),
                case.expected.map(|_| ()),
                "{} validate_selection agrees with reasoning_intent",
                case.name
            );
        }
    }

    #[test]
    fn budget_encoding_completeness_is_validated_for_every_selection() {
        struct Case {
            name: &'static str,
            selection: ReasoningSelection,
        }

        let cases = [
            Case {
                name: "provider_default",
                selection: ReasoningSelection::ProviderDefault,
            },
            Case {
                name: "disabled",
                selection: ReasoningSelection::Disabled,
            },
            Case {
                name: "effort",
                selection: ReasoningSelection::Effort("low".to_string()),
            },
        ];

        for case in cases {
            let mut r = reasoning();
            r.encoding = ReasoningEncoding::Budget(BTreeMap::from([
                ("low".to_string(), 1024),
                ("high".to_string(), 8192),
            ]));
            let error = capability(Some(r))
                .reasoning_intent("m", "test", &case.selection)
                .expect_err(case.name);
            assert_eq!(
                error.category,
                LlmProfileEffortValidationCategory::MalformedCapability,
                "{}",
                case.name
            );
            assert_eq!(
                error.message,
                "Malformed capability for model `m` on test: budget encoding is missing advertised effort `medium`.",
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn complete_budget_encoding_passes_integrity_validation() {
        let mut r = reasoning();
        r.encoding = ReasoningEncoding::Budget(BTreeMap::from([
            ("low".to_string(), 1024),
            ("medium".to_string(), 4096),
            ("high".to_string(), 8192),
            ("max".to_string(), 16384),
        ]));

        assert_eq!(
            capability(Some(r)).reasoning_intent(
                "m",
                "test",
                &ReasoningSelection::Effort("high".to_string())
            ),
            Ok(Some(ReasoningIntent::Budget(8192)))
        );
    }

    #[test]
    fn recorded_reasoning_capabilities_of_the_removed_shape_are_refused() {
        for removed in [
            serde_json::json!({ "efforts": ["low"], "retired_default": "low" }),
            serde_json::json!({ "efforts": ["low"], "aliases": { "minimal": "low" } }),
            serde_json::json!({ "efforts": ["low"], "disable": "native" }),
            serde_json::json!({ "efforts": ["low"], "disable": { "effort": "none" } }),
        ] {
            assert!(
                serde_json::from_value::<ReasoningCapability>(removed.clone()).is_err(),
                "{removed}"
            );
        }
        let current: ReasoningCapability =
            serde_json::from_value(serde_json::json!({ "efforts": ["low"], "disable": true }))
                .expect("current shape");
        assert!(current.disable);
    }
}

/// An immutable host catalogue revision retained in a session's model policy.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachmentCapabilitySnapshot {
    pub revision: String,
    pub acceptors: Vec<AttachmentAcceptor>,
}
impl AttachmentCapabilitySnapshot {
    pub fn is_empty(&self) -> bool {
        self.revision.is_empty() && self.acceptors.is_empty()
    }
    pub fn is_empty_arc(snapshot: &std::sync::Arc<Self>) -> bool {
        snapshot.is_empty()
    }
    pub fn forms(
        &self,
        provider: &str,
        media_type: &crate::MediaType,
        position: super::attachment_delivery::AttachmentPosition,
    ) -> super::attachment_delivery::DeliveryForms {
        let mut forms = super::attachment_delivery::DeliveryForms::default();
        for rule in self
            .acceptors
            .iter()
            .filter(|a| a.provider == provider)
            .flat_map(|a| &a.rules)
            .filter(|r| r.accepts(media_type, position))
        {
            forms.bytes |= rule.forms.bytes;
            forms.url |= rule.forms.url;
            forms.provider_file |= rule.forms.provider_file;
        }
        forms
    }
    pub fn acceptors(
        &self,
        media_type: &crate::MediaType,
        position: super::attachment_delivery::AttachmentPosition,
    ) -> Vec<&str> {
        self.acceptors
            .iter()
            .filter(|a| {
                a.rules.iter().any(|r| {
                    r.accepts(media_type, position)
                        && (r.forms.bytes || r.forms.url || r.forms.provider_file)
                })
            })
            .map(|a| a.provider.as_str())
            .collect()
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachmentAcceptor {
    pub provider: String,
    pub rules: Vec<AttachmentAcceptanceRule>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachmentAcceptanceRule {
    pub positions: Vec<super::attachment_delivery::AttachmentPosition>,
    pub media_types: Vec<String>,
    pub media_families: Vec<String>,
    pub forms: super::attachment_delivery::DeliveryForms,
}
impl AttachmentAcceptanceRule {
    fn accepts(
        &self,
        mime: &crate::MediaType,
        position: super::attachment_delivery::AttachmentPosition,
    ) -> bool {
        self.positions.contains(&position)
            && (self.media_types.iter().any(|m| m == mime.as_str())
                || self.media_families.iter().any(|f| f == mime.family()))
    }
}
