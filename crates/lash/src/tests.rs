use crate::support::{
    Arc, EmbedError, LashCore, PluginFactory, ProcessRegistry, Result, StaticPluginFactory,
    StdMutex, ToolProvider, async_trait,
};
use lash_core::facade_support::ProviderHandle;
use lash_sansio::sync::MutexExt;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::llm::types::{LlmContentBlock, LlmRequest, LlmResponse, LlmRole, LlmStreamEvent};
use lash_core::{LlmOutputPart, StoreError};

pub(crate) trait CreatedSession: Sized {
    /// Created from the test host's default spec ([`mock_session_spec`]).
    async fn created(self) -> Self;
    /// Created from `spec`.
    async fn created_with(self, spec: crate::SessionSpec) -> Self;
}

impl CreatedSession for crate::SessionBuilder {
    async fn created(self) -> Self {
        self.created_with(mock_session_spec()).await
    }

    async fn created_with(self, spec: crate::SessionSpec) -> Self {
        match self
            .core
            .session(self.session_id.clone())
            .create(crate::SessionCreation::root(spec))
            .await
        {
            Ok(_)
            | Err(EmbedError::SessionAlreadyExists { .. })
            | Err(EmbedError::Store(StoreError::SessionDeleted { .. })) => self,
            Err(error) => panic!("create session `{}`: {error:?}", self.session_id),
        }
    }
}

fn mock_provider() -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .requires_streaming(true)
        .complete(|request| async move {
            let user_text = last_user_text(&request);
            let reply = format!("echo: {user_text}");
            if let Some(events) = request.stream_events.as_ref() {
                events.send(LlmStreamEvent::Delta {
                    block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                    text: reply.clone(),
                });
            }
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: reply,
                    response_meta: None,
                }],
                usage: lash_core::llm::types::LlmUsage {
                    input_tokens: user_text.split_whitespace().count() as i64,
                    output_tokens: 2,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_output_tokens: 0,
                },
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle()
}

fn text_response(text: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

fn last_user_text(request: &LlmRequest) -> String {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == LlmRole::User)
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// A standard core over `backend`.
pub(crate) fn standard_core_over(backend: lash_core::Backend) -> LashCore {
    explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())
        .expect("standard core")
}

/// Default RLM protocol factory for tests, over `backend`, the substrate its
/// Lashlang artifacts live in.
#[cfg(feature = "rlm")]
fn rlm_factory(backend: &lash_core::Backend) -> lash_protocol_rlm::RlmProtocolPluginFactory {
    lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        std::sync::Arc::new(lash_protocol_rlm::TypescriptDialect),
        backend,
    )
    .with_worker_service(untimed_fixture_workers())
}

/// The dialect's worker service with its run deadlines off the clock. A
/// fixture's guest is bounded by the instruction and memory budgets its
/// factory sets, which count the same work the same way under any load;
/// the standard compute and cumulative-CPU deadlines measure the host
/// instead, and a loaded full suite spent them on a law about retention
/// (FIG-4751). Laws about worker deadlines set their own.
#[cfg(feature = "rlm")]
fn untimed_fixture_workers() -> crate::rlm::WorkerService {
    /// Longer than any run the instruction budget admits.
    const OFF_THE_CLOCK: std::time::Duration = std::time::Duration::from_secs(365 * 24 * 60 * 60);
    let mut config =
        lash_protocol_rlm::Dialect::worker_service(&lash_protocol_rlm::TypescriptDialect)
            .config()
            .clone();
    config.deadlines.compute = OFF_THE_CLOCK;
    config.deadlines.serialization = OFF_THE_CLOCK;
    config.deadlines.cumulative_cpu = OFF_THE_CLOCK;
    crate::rlm::WorkerService::new(config)
}

mod core_session_builder;
mod deployment_and_testing_facade;
mod facade_construction;
mod harness;
pub(crate) use harness::{
    DecoratedBackend, explicit_ephemeral_facets, mock_llm_profile_spec, mock_session_spec,
    recorded_llm_profile, sqlite_memory_store_backend, sqlite_memory_store_set,
    store_backend_with_clock,
};
#[cfg(feature = "rlm")]
mod aggregate_oracle;
mod plugin_reopen;
mod response_phase_replay;
mod tool_intent_ingress;
mod turn_streaming;
