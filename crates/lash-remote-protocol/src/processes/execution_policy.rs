use super::*;

/// Required wire mirror of the session's turn budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteTurnBudget {
    Bounded(std::num::NonZeroUsize),
    Unbounded,
}

/// Required wire mirror of the session's consecutive unproductive-attempt budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteNoProgressBudget {
    Bounded(std::num::NonZeroUsize),
    Unbounded,
}

/// Required wire mirror of the session's duplicate-billing appetite.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum RemoteChargeSafetyPolicy {
    #[default]
    RequireGuarantee,
    AcceptDuplicateBilling {
        max_unsafe_retries: u8,
        max_duplicate_cost_tokens: Option<u64>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessExecutionPolicy {
    /// The recorded model selection; absent for a policy that selects none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<RemoteModelConfig>,
    /// The session's recorded attachment-acceptance rules.
    #[serde(
        default,
        skip_serializing_if = "crate::llm::RemoteAttachmentCapabilitySnapshot::is_empty"
    )]
    pub attachment_acceptance: crate::llm::RemoteAttachmentCapabilitySnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default)]
    pub autonomous: bool,
    pub turn_budget: RemoteTurnBudget,
    pub no_progress_budget: RemoteNoProgressBudget,
    pub charge_safety: RemoteChargeSafetyPolicy,
    /// Session-wide generation intent, mirroring `SessionPolicy.generation`.
    /// A remote peer that persists an execution policy without it would
    /// resume the session with uncontrolled sampling.
    #[serde(
        default,
        skip_serializing_if = "crate::llm::RemoteGenerationOptions::is_empty"
    )]
    pub generation: crate::llm::RemoteGenerationOptions,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteRecordedRender {
    pub renderer_id: String,
    pub params: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessExecutionEnvSpec {
    pub plugin_config: RemoteProcessPluginConfig,
    pub policy: RemoteProcessExecutionPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<RemoteRecordedRender>,
}

impl RemoteProcessExecutionPolicy {
    pub fn new(turn_budget: RemoteTurnBudget) -> Self {
        Self {
            model: None,
            attachment_acceptance: Default::default(),
            session_id: None,
            autonomous: false,
            turn_budget,
            no_progress_budget: match lash_sansio::NoProgressBudget::default() {
                lash_sansio::NoProgressBudget::Bounded(limit) => {
                    RemoteNoProgressBudget::Bounded(limit)
                }
                lash_sansio::NoProgressBudget::Unbounded => RemoteNoProgressBudget::Unbounded,
            },
            charge_safety: RemoteChargeSafetyPolicy::default(),
            generation: crate::llm::RemoteGenerationOptions::default(),
        }
    }
}

impl RemoteProcessExecutionEnvSpec {
    pub fn new(turn_budget: RemoteTurnBudget) -> Self {
        Self {
            plugin_config: RemoteProcessPluginConfig::default(),
            policy: RemoteProcessExecutionPolicy::new(turn_budget),
            render: None,
        }
    }

    pub fn validate(&self, type_name: &'static str) -> Result<(), RemoteProtocolError> {
        if let Some(model) = &self.policy.model {
            require_non_empty(type_name, "env_spec.policy.model.key", &model.key)?;
            let limits = &model.metadata.limits;
            if limits.context_window_tokens == 0 {
                return Err(RemoteProtocolError::InvalidEnvelope {
                    type_name,
                    message: "env_spec.policy.model.metadata.limits.context_window_tokens must be greater than zero"
                        .to_string(),
                });
            }
            if limits.output_token_capacity.is_some_and(|value| value == 0) {
                return Err(RemoteProtocolError::InvalidEnvelope {
                    type_name,
                    message: "env_spec.policy.model.metadata.limits.output_token_capacity must be greater than zero"
                        .to_string(),
                });
            }
        }
        self.policy.generation.validate(type_name)?;
        Ok(())
    }
}
