//! H2's reusable real tool bodies; the example compiles the same fixture
//! through the public facade, without a dependency on this harness.
#[path = "../../../../examples/shared/h2_tool_bodies.rs"]
mod bodies;
pub use bodies::*;
#[path = "../../../../examples/shared/h2_provider.rs"]
mod provider;
pub use provider::{FixtureProtocol, scripted_provider};
#[path = "../../../../examples/shared/h2_receiver.rs"]
mod receiver;
pub use receiver::{
    ReceiverEnginePlugin, ReceiverEvents, receiver_events, receiver_input, register_receiver,
};

/// Fleet adapters decorate the StoreSet before constructing their engine,
/// then pass that engine's Backend here. The fixture does not open a store.
pub fn builder(
    backend: lash::Backend,
    channel: &crate::e2e::case::Channel,
    profiles: std::sync::Arc<dyn lash::LlmProfiles>,
    tools: std::sync::Arc<dyn lash::tools::ToolProvider>,
) -> lash::LashCoreBuilder {
    let builder = match channel {
        crate::e2e::case::Channel::Standard => lash::LashCore::standard_builder(backend),
        crate::e2e::case::Channel::Rlm => {
            let protocol = lash::rlm::RlmProtocolPluginFactory::new(
                lash::rlm::RlmProtocolPluginConfig::builder()
                    .channel(lash::rlm::RlmChannel::Cell)
                    .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                    .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                    .build(),
                std::sync::Arc::new(lash::rlm::TypescriptDialect),
                &backend,
            );
            lash::LashCore::rlm_builder(backend, protocol)
        }
    };
    builder
        .llm_profiles(profiles)
        .tools(tools)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
}
