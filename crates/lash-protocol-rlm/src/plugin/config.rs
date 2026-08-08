#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RlmProtocolPluginConfig {
    pub instruction_budget: lashlang::ExecutionBound<std::num::NonZeroU64>,
    pub deadline: lashlang::ExecutionBound<std::time::Duration>,
    #[serde(default)]
    pub prompt_features: crate::protocol::RlmPromptFeatures,
    #[serde(default)]
    pub lashlang_abilities: lashlang::LashlangAbilities,
    #[serde(default)]
    pub lashlang_language_features: lashlang::LashlangLanguageFeatures,
    #[serde(default = "default_max_output_chars")]
    pub max_output_chars: usize,
    #[serde(default = "default_continue_as_soft_warn_tokens")]
    pub continue_as_soft_warn_tokens: Option<usize>,
}

fn default_max_output_chars() -> usize {
    10_000
}

fn default_continue_as_soft_warn_tokens() -> Option<usize> {
    Some(100_000)
}

impl RlmProtocolPluginConfig {
    pub fn new(
        instruction_budget: lashlang::ExecutionBound<std::num::NonZeroU64>,
        deadline: lashlang::ExecutionBound<std::time::Duration>,
    ) -> Self {
        Self {
            instruction_budget,
            deadline,
            prompt_features: crate::protocol::RlmPromptFeatures::default(),
            lashlang_abilities: lashlang::LashlangAbilities::default(),
            lashlang_language_features: lashlang::LashlangLanguageFeatures::default(),
            max_output_chars: default_max_output_chars(),
            continue_as_soft_warn_tokens: default_continue_as_soft_warn_tokens(),
        }
    }

    pub(crate) fn execution_bounds(&self) -> lashlang::ExecutionBounds {
        lashlang::ExecutionBounds::new(self.instruction_budget, self.deadline)
    }

    pub fn with_lashlang_abilities(mut self, abilities: lashlang::LashlangAbilities) -> Self {
        self.lashlang_abilities = abilities;
        self
    }

    pub fn with_lashlang_language_features(
        mut self,
        language_features: lashlang::LashlangLanguageFeatures,
    ) -> Self {
        self.lashlang_language_features = language_features;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rlm_config_defaults_soft_budget_threshold_after_explicit_bounds() {
        let config = RlmProtocolPluginConfig::new(
            lashlang::ExecutionBound::Unbounded,
            lashlang::ExecutionBound::Unbounded,
        );

        assert_eq!(config.continue_as_soft_warn_tokens, Some(100_000));
    }

    #[test]
    fn serialized_config_requires_both_execution_bounds() {
        let missing_instruction = serde_json::json!({
            "deadline": "unbounded"
        });
        let error = serde_json::from_value::<RlmProtocolPluginConfig>(missing_instruction)
            .expect_err("instruction budget must be explicit");
        assert!(error.to_string().contains("instruction_budget"));

        let missing = serde_json::json!({
            "instruction_budget": "unbounded"
        });
        let error = serde_json::from_value::<RlmProtocolPluginConfig>(missing)
            .expect_err("deadline must be explicit");
        assert!(error.to_string().contains("deadline"));
    }

    #[test]
    fn execution_bounds_use_host_friendly_json_shapes() {
        let config = RlmProtocolPluginConfig::new(
            lashlang::ExecutionBound::instructions(1_000_000),
            lashlang::ExecutionBound::millis(30_000),
        );
        let encoded = serde_json::to_value(&config).expect("serialize config");
        assert_eq!(
            encoded["instruction_budget"],
            serde_json::json!({ "bounded": 1_000_000 })
        );
        assert_eq!(
            encoded["deadline"],
            serde_json::json!({ "bounded": 30_000 })
        );

        let decoded: RlmProtocolPluginConfig = serde_json::from_value(encoded).expect("decode");
        assert_eq!(decoded, config);
    }
}
