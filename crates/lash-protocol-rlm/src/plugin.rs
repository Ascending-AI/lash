pub const RLM_PROTOCOL_PLUGIN_ID: &str = "rlm_protocol";

pub(crate) mod budget_warning;
mod config;
mod config_owner;
mod config_types;
mod factory;
mod prose_projector;
mod protocol_driver;
pub(crate) mod protocol_session;
mod registration;
pub(crate) mod runtime_state;
pub(crate) mod tool_args;

pub use config::{
    RlmProtocolPluginConfig, RlmProtocolPluginConfigBuilder, RlmRecordedBehaviour, UnsetBound,
    UnsetChannel,
};
pub use config_owner::{
    RLM_CONFIG_IMPLEMENTATION, RlmConfigOwner, RlmConfigRefusal, RlmCreateConfig,
    RlmRecordedConfig, SetRlmRender,
};
pub use config_types::{
    ExecutionBounds, InstructionBound, MemoryBound, RlmAbilities, RlmLanguageFeatures,
};
pub use factory::{
    LashlangCompileSurface, LashlangCompileSurfaceRequest, LashlangModuleCompileError,
    LashlangModuleCompileRequest, ModuleCompileOutput, RlmProtocolPluginFactory,
    rlm_lashlang_surface, rlm_protocol_config,
};
pub use protocol_session::{RlmSessionConfigDecodeError, rlm_session_config};

mod channel;
pub use channel::RlmChannel;
#[cfg(test)]
mod recorded_behaviour_tests;
#[cfg(test)]
mod recorded_inheritance_tests;
