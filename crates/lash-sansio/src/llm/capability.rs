//! Host-supplied model capability metadata.
//!
//! Capability is data the host attaches to a model spec and threads onto every
//! [`LlmRequest`](crate::llm::types::LlmRequest). Lash validates a requested
//! effort against it and normalizes (alias-clamps) the value before a provider
//! sees it. Providers consume capability; they never produce it.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Capability metadata for a single model on a route, supplied by the host.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelCapability {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningCapability>,
    /// Cache-control wire dialect accepted by this model on its selected route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControlDialect>,
    /// How a streaming provider proves that this model's response completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_termination: Option<StreamTermination>,
    /// Whether this model lets a caller set the sampling temperature.
    #[serde(default, skip_serializing_if = "SamplingCapability::is_default")]
    pub sampling: SamplingCapability,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StreamTermination {
    /// EOF is successful only after the dialect's semantic terminal event.
    RequireTerminalEvidence,
    /// Clean EOF is a valid completion boundary for this route.
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
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReasoningCapability {
    #[serde(default)]
    pub efforts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    /// requested-effort -> canonical-effort clamp map (e.g. "xhigh" -> "max", "minimal" -> "low")
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub encoding: ReasoningEncoding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable: Option<ReasoningDisableEncoding>,
    #[serde(default)]
    pub mandatory: bool,
}

/// The host-resolved reasoning choice carried from model selection to providers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSelection {
    /// Leave reasoning configuration entirely to the provider.
    #[default]
    ProviderDefault,
    /// Explicitly disable reasoning using the model capability's disable encoding.
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

/// How an explicit [`ReasoningSelection::Disabled`] is encoded on the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningDisableEncoding {
    Native,
    Omit,
    Effort(String),
    Budget(u32),
    ToggleFalse,
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

/// Deterministic taxonomy of effort-validation failures. The serde snake_case
/// codes are a stable contract: downstream consumers match on them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelEffortValidationCategory {
    UnsupportedEffort,
    EffortNotConfigurable,
    EffortRequired,
    MalformedCapability,
}

impl ModelEffortValidationCategory {
    /// Stable snake_case code, matching the serde representation. Turn-driver
    /// validation surfaces this as the turn-issue code.
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedEffort => "unsupported_effort",
            Self::EffortNotConfigurable => "effort_not_configurable",
            Self::EffortRequired => "effort_required",
            Self::MalformedCapability => "malformed_capability",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEffortValidationError {
    pub category: ModelEffortValidationCategory,
    pub message: String,
}

impl std::fmt::Display for ModelEffortValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ModelEffortValidationError {}

impl ModelCapability {
    pub fn is_empty(&self) -> bool {
        self.reasoning.is_none()
            && self.cache_control.is_none()
            && self.stream_termination.is_none()
            && self.sampling.is_default()
    }

    /// Whether an adapter may put a caller-requested temperature on the wire
    /// for this model.
    pub fn allows_caller_temperature(&self) -> bool {
        self.sampling == SamplingCapability::Configurable
    }

    /// Resolve a requested effort to its canonical form: alias-map first
    /// (input lowercased/trimmed), then direct membership in `efforts`.
    /// Returns `None` when the request maps to nothing this model exposes.
    pub fn resolve_effort(&self, requested: &str) -> Option<String> {
        let reasoning = self.reasoning.as_ref()?;
        let key = requested.trim().to_lowercase();
        if let Some(canonical) = reasoning.aliases.get(&key) {
            return Some(canonical.clone());
        }
        if reasoning.efforts.iter().any(|effort| effort == &key) {
            return Some(key);
        }
        None
    }

    /// Validate a requested effort against this capability and return the
    /// canonical (alias-normalized) effort to send on the wire. `Ok(None)`
    /// means no effort is configured on the request.
    pub fn validate_selection(
        &self,
        model: &str,
        provider_kind: &str,
        requested: &ReasoningSelection,
    ) -> Result<ReasoningSelection, ModelEffortValidationError> {
        if let Some(ReasoningCapability {
            efforts,
            encoding: ReasoningEncoding::Budget(budgets),
            ..
        }) = self.reasoning.as_ref()
            && let Some(missing) = efforts.iter().find(|effort| !budgets.contains_key(*effort))
        {
            return Err(ModelEffortValidationError {
                category: ModelEffortValidationCategory::MalformedCapability,
                message: format!(
                    "Malformed capability for model `{model}` on {provider_kind}: budget encoding is missing advertised effort `{missing}`."
                ),
            });
        }

        match (self.reasoning.as_ref(), requested) {
            (None, ReasoningSelection::Effort(effort)) => Err(ModelEffortValidationError {
                category: ModelEffortValidationCategory::EffortNotConfigurable,
                message: format!(
                    "Model `{model}` on {provider_kind} does not expose configurable effort (requested `{effort}`)."
                ),
            }),
            (None, ReasoningSelection::Disabled) => Err(ModelEffortValidationError {
                category: ModelEffortValidationCategory::EffortNotConfigurable,
                message: format!(
                    "Model `{model}` on {provider_kind} does not expose configurable effort (requested disabled)."
                ),
            }),
            (None, ReasoningSelection::ProviderDefault) => Ok(ReasoningSelection::ProviderDefault),
            (Some(reasoning), ReasoningSelection::ProviderDefault) => {
                if reasoning.mandatory {
                    Err(ModelEffortValidationError {
                        category: ModelEffortValidationCategory::EffortRequired,
                        message: format!(
                            "Model `{model}` on {provider_kind} requires an explicit effort. Available: {}",
                            reasoning.efforts.join(", ")
                        ),
                    })
                } else {
                    Ok(ReasoningSelection::ProviderDefault)
                }
            }
            (Some(reasoning), ReasoningSelection::Disabled) => {
                if reasoning.disable.is_some() {
                    Ok(ReasoningSelection::Disabled)
                } else {
                    Err(ModelEffortValidationError {
                        category: ModelEffortValidationCategory::UnsupportedEffort,
                        message: format!(
                            "Model `{model}` on {provider_kind} does not support disabling reasoning."
                        ),
                    })
                }
            }
            (Some(reasoning), ReasoningSelection::Effort(effort)) => {
                if reasoning.efforts.is_empty() {
                    return Err(ModelEffortValidationError {
                        category: ModelEffortValidationCategory::EffortNotConfigurable,
                        message: format!(
                            "Model `{model}` on {provider_kind} does not expose configurable effort (requested `{effort}`)."
                        ),
                    });
                }
                match self.resolve_effort(effort) {
                    Some(resolved) if reasoning.efforts.contains(&resolved) => {
                        Ok(ReasoningSelection::Effort(resolved))
                    }
                    _ => Err(ModelEffortValidationError {
                        category: ModelEffortValidationCategory::UnsupportedEffort,
                        message: format!(
                            "Unsupported effort `{effort}` for `{model}` on {provider_kind}. Available: {}",
                            reasoning.efforts.join(", ")
                        ),
                    }),
                }
            }
        }
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
            default_effort: Some("medium".to_string()),
            aliases: BTreeMap::new(),
            encoding: ReasoningEncoding::Effort,
            disable: Some(ReasoningDisableEncoding::Effort("none".to_string())),
            mandatory: false,
        }
    }

    fn capability(reasoning: Option<ReasoningCapability>) -> ModelCapability {
        ModelCapability {
            reasoning,
            cache_control: None,
            stream_termination: None,
            sampling: SamplingCapability::Configurable,
        }
    }

    #[test]
    fn is_empty_tracks_reasoning_presence() {
        assert!(capability(None).is_empty());
        assert!(!capability(Some(reasoning())).is_empty());
        assert!(
            !ModelCapability {
                cache_control: Some(CacheControlDialect::Anthropic),
                ..ModelCapability::default()
            }
            .is_empty()
        );
        assert!(
            !ModelCapability {
                stream_termination: Some(StreamTermination::RequireTerminalEvidence),
                ..ModelCapability::default()
            }
            .is_empty()
        );
        // A capability whose only statement is "this model pins its own
        // sampling" must still reach the wire.
        let pinned = ModelCapability {
            sampling: SamplingCapability::Pinned,
            ..ModelCapability::default()
        };
        assert!(!pinned.is_empty());
        assert!(!pinned.allows_caller_temperature());
        assert!(ModelCapability::default().allows_caller_temperature());
    }

    #[test]
    fn resolve_effort_prefers_alias_then_membership() {
        let mut r = reasoning();
        r.aliases.insert("xhigh".to_string(), "max".to_string());
        r.aliases.insert("minimal".to_string(), "low".to_string());
        let cap = capability(Some(r));

        // alias, including case/whitespace normalization
        assert_eq!(cap.resolve_effort("xhigh").as_deref(), Some("max"));
        assert_eq!(cap.resolve_effort("  XHigh ").as_deref(), Some("max"));
        assert_eq!(cap.resolve_effort("MINIMAL").as_deref(), Some("low"));
        // direct membership
        assert_eq!(cap.resolve_effort("high").as_deref(), Some("high"));
        // neither
        assert_eq!(cap.resolve_effort("turbo"), None);
    }

    #[test]
    fn resolve_effort_none_when_no_reasoning() {
        assert_eq!(capability(None).resolve_effort("low"), None);
    }

    #[test]
    fn resolved_selection_classifier_covers_ratified_table() {
        struct Case {
            name: &'static str,
            capability: ModelCapability,
            selection: ReasoningSelection,
            expected: Result<ReasoningSelection, ModelEffortValidationCategory>,
        }

        let mut aliased = reasoning();
        aliased
            .aliases
            .insert("xhigh".to_string(), "max".to_string());
        let mut cannot_disable = reasoning();
        cannot_disable.disable = None;
        let mut mandatory = reasoning();
        mandatory.mandatory = true;
        let mut malformed = reasoning();
        malformed
            .aliases
            .insert("broken".to_string(), "missing".to_string());

        let cases = [
            Case {
                name: "default",
                capability: capability(Some(reasoning())),
                selection: ReasoningSelection::ProviderDefault,
                expected: Ok(ReasoningSelection::ProviderDefault),
            },
            Case {
                name: "effort",
                capability: capability(Some(reasoning())),
                selection: ReasoningSelection::Effort("High".to_string()),
                expected: Ok(ReasoningSelection::Effort("high".to_string())),
            },
            Case {
                name: "alias_to_effort",
                capability: capability(Some(aliased)),
                selection: ReasoningSelection::Effort("xhigh".to_string()),
                expected: Ok(ReasoningSelection::Effort("max".to_string())),
            },
            Case {
                name: "disabled",
                capability: capability(Some(reasoning())),
                selection: ReasoningSelection::Disabled,
                expected: Ok(ReasoningSelection::Disabled),
            },
            Case {
                name: "disabled_unsupported",
                capability: capability(Some(cannot_disable)),
                selection: ReasoningSelection::Disabled,
                expected: Err(ModelEffortValidationCategory::UnsupportedEffort),
            },
            Case {
                name: "no_reasoning",
                capability: capability(None),
                selection: ReasoningSelection::Effort("low".to_string()),
                expected: Err(ModelEffortValidationCategory::EffortNotConfigurable),
            },
            Case {
                name: "mandatory_without_selection",
                capability: capability(Some(mandatory)),
                selection: ReasoningSelection::ProviderDefault,
                expected: Err(ModelEffortValidationCategory::EffortRequired),
            },
            Case {
                name: "malformed_capability",
                capability: capability(Some(malformed)),
                selection: ReasoningSelection::Effort("broken".to_string()),
                expected: Err(ModelEffortValidationCategory::UnsupportedEffort),
            },
        ];

        for case in cases {
            let actual = case
                .capability
                .validate_selection("m", "test", &case.selection)
                .map_err(|error| error.category);
            assert_eq!(actual, case.expected, "{}", case.name);
        }
    }

    #[test]
    fn category_codes_are_stable_snake_case() {
        assert_eq!(
            ModelEffortValidationCategory::UnsupportedEffort.code(),
            "unsupported_effort"
        );
        assert_eq!(
            ModelEffortValidationCategory::EffortNotConfigurable.code(),
            "effort_not_configurable"
        );
        assert_eq!(
            ModelEffortValidationCategory::EffortRequired.code(),
            "effort_required"
        );
        assert_eq!(
            ModelEffortValidationCategory::MalformedCapability.code(),
            "malformed_capability"
        );
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
                .validate_selection("m", "test", &case.selection)
                .expect_err(case.name);
            assert_eq!(
                error.category,
                ModelEffortValidationCategory::MalformedCapability,
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
            capability(Some(r)).validate_selection(
                "m",
                "test",
                &ReasoningSelection::Effort("high".to_string())
            ),
            Ok(ReasoningSelection::Effort("high".to_string()))
        );
    }

    #[test]
    fn model_capability_serde_roundtrips_and_skips_empties() {
        let cap = capability(None);
        let json = serde_json::to_value(&cap).expect("serialize");
        assert_eq!(json, serde_json::json!({}));
        let back: ModelCapability = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, cap);

        let cap = ModelCapability {
            cache_control: Some(CacheControlDialect::Gemini),
            ..ModelCapability::default()
        };
        let json = serde_json::to_value(&cap).expect("serialize");
        assert_eq!(json, serde_json::json!({ "cache_control": "gemini" }));
        let back: ModelCapability = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, cap);

        let mut r = reasoning();
        r.aliases.insert("xhigh".to_string(), "max".to_string());
        r.encoding = ReasoningEncoding::Budget(BTreeMap::from([
            ("low".to_string(), 1024u32),
            ("medium".to_string(), 4096u32),
            ("high".to_string(), 8192u32),
            ("max".to_string(), 16384u32),
        ]));
        let cap = capability(Some(r));
        let json = serde_json::to_value(&cap).expect("serialize");
        let back: ModelCapability = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, cap);
    }
}
