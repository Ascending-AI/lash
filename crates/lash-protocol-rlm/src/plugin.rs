pub const RLM_PROTOCOL_PLUGIN_ID: &str = "rlm_protocol";

mod budget_warning;
mod config;
mod config_types;
mod factory;
mod prose_projector;
mod protocol_driver;
mod protocol_session;
mod registration;
mod runtime_state;
mod tool_args;

pub use config::RlmProtocolPluginConfig;
pub use config_types::{ExecutionBound, ExecutionBounds, RlmAbilities, RlmLanguageFeatures};
pub use factory::{
    LashlangCompileSurface, LashlangCompileSurfaceRequest, LashlangModuleCompileError,
    LashlangModuleCompileRequest, ModuleCompileOutput, RlmProtocolPluginFactory,
    rlm_lashlang_surface, rlm_protocol_config,
};
pub use protocol_session::{
    RlmSessionConfigDecodeError, apply_rlm_session_config_if_unset,
    apply_rlm_session_config_post_open, rlm_session_config, rlm_session_config_options,
};
