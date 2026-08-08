use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RlmProtocolPluginConfig {
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

impl Default for RlmProtocolPluginConfig {
    fn default() -> Self {
        Self {
            prompt_features: crate::protocol::RlmPromptFeatures::default(),
            lashlang_abilities: lashlang::LashlangAbilities::default(),
            lashlang_language_features: lashlang::LashlangLanguageFeatures::default(),
            max_output_chars: default_max_output_chars(),
            continue_as_soft_warn_tokens: default_continue_as_soft_warn_tokens(),
            redaction_roots: None,
        }
    }
}

impl RlmProtocolPluginConfig {
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

    /// Use explicit, deployment-stable roots for capture-time error redaction.
    pub fn with_redaction_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.redaction_roots = Some(roots);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rlm_config_defaults_soft_budget_threshold() {
        let config = RlmProtocolPluginConfig::default();

        assert_eq!(config.continue_as_soft_warn_tokens, Some(100_000));
    }
}
