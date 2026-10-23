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

/// The key of the protocol's own argument transform, which projects a
/// call's arguments as its tool's manifest asks: a cell's read applies the
/// same projection itself (ADR 0144).
pub use config::{
    RlmDiscovery, RlmPresentationConfig, RlmProtocolPluginConfig, RlmProtocolPluginConfigBuilder,
    RlmRecordedBehaviour, UnsetBound, UnsetChannel,
};
pub use config_owner::{
    RLM_RUN_OPTIONS, RlmConfigOwner, RlmConfigRefusal, RlmCreateConfig, RlmRecordedConfig,
    RlmRenderRefusal, RlmRunOptions, SetRlmRender,
};
pub use config_types::{
    Charge, ChargeLimiter, ExecutionBounds, InstructionBound, LimitDecision, MemoryBound,
    MemoryLimiter, MemoryMeter,
};
pub use factory::RlmProtocolPluginFactory;
pub use protocol_session::{RlmSessionConfigDecodeError, rlm_session_config};

mod channel;
pub use channel::RlmChannel;

pub(crate) use factory::RlmProtocolPlugin;
