pub const RLM_PROTOCOL_PLUGIN_ID: &str = "rlm_protocol";

pub(crate) mod budget_warning;
mod config;
mod config_types;
mod factory;
mod prose_projector;
mod protocol_driver;
pub(crate) mod protocol_session;
mod registration;
pub(crate) mod runtime_state;
pub(crate) mod tool_args;

pub use config::{
    RlmProtocolPluginConfig, RlmProtocolPluginConfigBuilder, UnsetBound, UnsetChannel,
};
pub use config_types::{
    ExecutionBounds, InstructionBound, MemoryBound, RlmAbilities, RlmLanguageFeatures,
    WallClockBound,
};
pub use factory::{
    LashlangCompileSurface, LashlangCompileSurfaceRequest, LashlangModuleCompileError,
    LashlangModuleCompileRequest, ModuleCompileOutput, RlmProtocolPluginFactory,
    rlm_lashlang_surface, rlm_protocol_config,
};
pub use protocol_session::{
    RlmSessionConfigDecodeError, apply_rlm_session_config_if_unset, rlm_session_config,
    rlm_session_config_options,
};

mod channel;
pub use channel::RlmChannel;
