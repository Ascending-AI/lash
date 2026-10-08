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
    RlmPresentationConfig, RlmProtocolPluginConfig, RlmProtocolPluginConfigBuilder,
    RlmRecordedBehaviour, UnsetBound, UnsetChannel,
};
pub use config_owner::{
    RlmConfigOwner, RlmConfigRefusal, RlmCreateConfig, RlmRecordedConfig, RlmRenderRefusal,
    RlmRunOptions, SetRlmRender,
};
pub use config_types::{ExecutionBounds, InstructionBound, MemoryBound, RlmLanguageFeatures};
pub use factory::{
    LashlangCompileSurface, LashlangCompileSurfaceRequest, LashlangModuleCompileError,
    LashlangModuleCompileRequest, ModuleCompileOutput, RlmProtocolPluginFactory,
    rlm_lashlang_surface,
};
pub use protocol_session::{RlmSessionConfigDecodeError, rlm_session_config};

mod channel;
pub use channel::RlmChannel;
