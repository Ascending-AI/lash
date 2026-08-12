use std::path::PathBuf;

use super::{ExecutionBound, ExecutionBounds, RlmAbilities, RlmLanguageFeatures};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RlmProtocolPluginConfig {
    pub instruction_budget: ExecutionBound<std::num::NonZeroU64>,
    pub deadline: ExecutionBound<std::time::Duration>,
    #[serde(default = "default_memory_limit")]
    pub memory_limit: ExecutionBound<std::num::NonZeroU64>,
    #[serde(default)]
    pub prompt_features: crate::protocol::RlmPromptFeatures,
    #[serde(default)]
    pub lashlang_abilities: RlmAbilities,
    #[serde(default)]
    pub lashlang_language_features: RlmLanguageFeatures,
    #[serde(default = "default_max_output_chars")]
    pub max_output_chars: usize,
    #[serde(default = "default_continue_as_soft_warn_tokens")]
    pub continue_as_soft_warn_tokens: Option<usize>,
    /// Host-local roots removed from model-visible execution errors at capture
    /// time. `None` captures the factory process's cwd and HOME once;
    /// `Some([])` explicitly disables path redaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redaction_roots: Option<Vec<PathBuf>>,
}

fn default_max_output_chars() -> usize {
    10_000
}

fn default_continue_as_soft_warn_tokens() -> Option<usize> {
    Some(100_000)
}

impl RlmProtocolPluginConfig {
    pub fn new(
        instruction_budget: impl Into<ExecutionBound<std::num::NonZeroU64>>,
        deadline: impl Into<ExecutionBound<std::time::Duration>>,
    ) -> Self {
        Self {
            instruction_budget: instruction_budget.into(),
            deadline: deadline.into(),
            memory_limit: default_memory_limit(),
            prompt_features: crate::protocol::RlmPromptFeatures::default(),
            lashlang_abilities: RlmAbilities::default(),
            lashlang_language_features: RlmLanguageFeatures::default(),
            max_output_chars: default_max_output_chars(),
            continue_as_soft_warn_tokens: default_continue_as_soft_warn_tokens(),
            redaction_roots: None,
        }
    }

    pub(crate) fn execution_bounds(&self) -> ExecutionBounds {
        ExecutionBounds::new(self.instruction_budget, self.deadline)
            .with_memory_limit(self.memory_limit)
    }

    pub fn with_lashlang_abilities(mut self, abilities: impl Into<RlmAbilities>) -> Self {
        self.lashlang_abilities = abilities.into();
        self
    }

    pub fn with_lashlang_language_features(
        mut self,
        language_features: impl Into<RlmLanguageFeatures>,
    ) -> Self {
        self.lashlang_language_features = language_features.into();
        self
    }

    /// Use explicit, deployment-stable roots for capture-time error redaction.
    pub fn with_redaction_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.redaction_roots = Some(roots);
        self
    }
}

fn default_memory_limit() -> ExecutionBound<std::num::NonZeroU64> {
    ExecutionBound::instructions(lashlang::DEFAULT_HEAP_LOGICAL_BYTE_LIMIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rlm_config_defaults_soft_budget_threshold_after_explicit_bounds() {
        let config =
            RlmProtocolPluginConfig::new(ExecutionBound::Unbounded, ExecutionBound::Unbounded);

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
            ExecutionBound::instructions(1_000_000),
            ExecutionBound::millis(30_000),
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
