use super::{
    ExecutionBounds, InstructionBound, MemoryBound, RlmAbilities, RlmLanguageFeatures,
    WallClockBound,
};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RlmProtocolPluginConfig {
    pub instruction_limit: InstructionBound,
    pub wall_clock: WallClockBound,
    pub memory_limit: MemoryBound,
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
}

fn default_max_output_chars() -> usize {
    10_000
}

fn default_continue_as_soft_warn_tokens() -> Option<usize> {
    Some(100_000)
}

/// A builder slot that has not been filled in yet. [`RlmProtocolPluginConfigBuilder::build`]
/// exists only once every execution bound has replaced its `UnsetBound` slot, so a
/// config that forgot a bound is a compile error rather than a silent default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UnsetBound;

/// Builder for [`RlmProtocolPluginConfig`]. Each execution bound is set by
/// name and carries its own type, so the instruction, wall-clock, and memory
/// limits cannot be transposed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RlmProtocolPluginConfigBuilder<I = UnsetBound, W = UnsetBound, M = UnsetBound> {
    instruction_limit: I,
    wall_clock: W,
    memory_limit: M,
}

impl<I, W, M> RlmProtocolPluginConfigBuilder<I, W, M> {
    /// Set how many VM instructions an execution may run for.
    pub fn instruction_limit(
        self,
        instruction_limit: InstructionBound,
    ) -> RlmProtocolPluginConfigBuilder<InstructionBound, W, M> {
        RlmProtocolPluginConfigBuilder {
            instruction_limit,
            wall_clock: self.wall_clock,
            memory_limit: self.memory_limit,
        }
    }

    /// Set how much active execution time an execution may consume.
    pub fn wall_clock(
        self,
        wall_clock: WallClockBound,
    ) -> RlmProtocolPluginConfigBuilder<I, WallClockBound, M> {
        RlmProtocolPluginConfigBuilder {
            instruction_limit: self.instruction_limit,
            wall_clock,
            memory_limit: self.memory_limit,
        }
    }

    /// Set how many live logical heap bytes an execution may hold.
    pub fn memory_limit(
        self,
        memory_limit: MemoryBound,
    ) -> RlmProtocolPluginConfigBuilder<I, W, MemoryBound> {
        RlmProtocolPluginConfigBuilder {
            instruction_limit: self.instruction_limit,
            wall_clock: self.wall_clock,
            memory_limit,
        }
    }
}

impl RlmProtocolPluginConfigBuilder<InstructionBound, WallClockBound, MemoryBound> {
    /// Finish the config. Available only once all three bounds are chosen.
    pub fn build(self) -> RlmProtocolPluginConfig {
        RlmProtocolPluginConfig {
            instruction_limit: self.instruction_limit,
            wall_clock: self.wall_clock,
            memory_limit: self.memory_limit,
            prompt_features: crate::protocol::RlmPromptFeatures::default(),
            lashlang_abilities: RlmAbilities::default(),
            lashlang_language_features: RlmLanguageFeatures::default(),
            max_output_chars: default_max_output_chars(),
            continue_as_soft_warn_tokens: default_continue_as_soft_warn_tokens(),
        }
    }
}

impl RlmProtocolPluginConfig {
    /// Start configuring an RLM protocol plugin. Every execution bound is
    /// named and separately typed; there is no positional constructor to get
    /// them in the wrong order.
    pub fn builder() -> RlmProtocolPluginConfigBuilder {
        RlmProtocolPluginConfigBuilder {
            instruction_limit: UnsetBound,
            wall_clock: UnsetBound,
            memory_limit: UnsetBound,
        }
    }

    pub(crate) fn execution_bounds(&self) -> ExecutionBounds {
        ExecutionBounds::new(self.instruction_limit, self.wall_clock, self.memory_limit)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rlm_config_defaults_soft_budget_threshold_after_explicit_bounds() {
        let config = RlmProtocolPluginConfig::builder()
            .instruction_limit(InstructionBound::unbounded())
            .wall_clock(WallClockBound::unbounded())
            .memory_limit(MemoryBound::unbounded())
            .build();

        assert_eq!(config.continue_as_soft_warn_tokens, Some(100_000));
    }

    #[test]
    fn builder_sets_each_bound_on_its_own_axis_in_any_order() {
        let config = RlmProtocolPluginConfig::builder()
            .memory_limit(MemoryBound::mebibytes(64))
            .instruction_limit(InstructionBound::instructions(1_000_000))
            .wall_clock(WallClockBound::secs(30))
            .build();

        assert_eq!(
            config.instruction_limit,
            InstructionBound::instructions(1_000_000)
        );
        assert_eq!(config.wall_clock, WallClockBound::secs(30));
        assert_eq!(config.memory_limit, MemoryBound::bytes(64 * 1024 * 1024));
    }

    #[test]
    fn serialized_config_requires_all_execution_bounds() {
        let missing_instruction = serde_json::json!({
            "wall_clock": "unbounded"
        });
        let error = serde_json::from_value::<RlmProtocolPluginConfig>(missing_instruction)
            .expect_err("instruction limit must be explicit");
        assert!(error.to_string().contains("instruction_limit"));

        let missing = serde_json::json!({
            "instruction_limit": "unbounded"
        });
        let error = serde_json::from_value::<RlmProtocolPluginConfig>(missing)
            .expect_err("wall clock must be explicit");
        assert!(error.to_string().contains("wall_clock"));

        let missing = serde_json::json!({
            "instruction_limit": "unbounded",
            "wall_clock": "unbounded"
        });
        let error = serde_json::from_value::<RlmProtocolPluginConfig>(missing)
            .expect_err("memory limit must be explicit");
        assert!(error.to_string().contains("memory_limit"));
    }

    #[test]
    fn execution_bounds_use_host_friendly_json_shapes() {
        let config = RlmProtocolPluginConfig::builder()
            .instruction_limit(InstructionBound::instructions(1_000_000))
            .wall_clock(WallClockBound::millis(30_000))
            .memory_limit(MemoryBound::mebibytes(64))
            .build();
        let encoded = serde_json::to_value(&config).expect("serialize config");
        assert_eq!(
            encoded["instruction_limit"],
            serde_json::json!({ "bounded": 1_000_000 })
        );
        assert_eq!(
            encoded["wall_clock"],
            serde_json::json!({ "bounded": 30_000 })
        );
        assert_eq!(
            encoded["memory_limit"],
            serde_json::json!({ "bounded": 64 * 1024 * 1024 })
        );

        let decoded: RlmProtocolPluginConfig = serde_json::from_value(encoded).expect("decode");
        assert_eq!(decoded, config);
    }
}
