//! Shared host fixtures for the facade's domain laws.
//!
//! This test-only package depends on the production facade. Fixtures keep
//! their original providers, recorded model config and SQLite store layers.
#![allow(
    clippy::expect_used,
    reason = "test fixtures fail immediately when their setup is invalid"
)]
#![cfg_attr(
    test,
    allow(
        dead_code,
        unused_imports,
        reason = "external domain laws exercise the other fixtures of this shared source"
    )
)]

#[cfg(test)]
use crate as lash;

use lash::{EmbedError, LashCore, Result, async_trait};
use lash_core::facade_support::ProviderHandle;
use lash_core::facade_support::{PluginFactory, TurnActivitySink};
use lash_core::{ToolProvider, TurnActivity};
use lash_protocol_types::SessionId;
use lash_protocol_types::sync::MutexExt;
use lash_protocol_types::{llm::types::StreamBlockEvent, llm::types::StreamBlockKind};
use std::sync::Arc;

use lash_core::llm::types::{LlmContentBlock, LlmRequest, LlmResponse, LlmRole, LlmStreamEvent};
use lash_core::{LlmOutputPart, StoreError};

pub fn mock_provider() -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("embed-test")
        .requires_streaming(true)
        .complete(|request| async move {
            let user_text = last_user_text(&request);
            let reply = format!("echo: {user_text}");
            if let Some(events) = request.stream_events.as_ref() {
                events.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                    kind: StreamBlockKind::AssistantText,
                    block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                    text: reply.clone(),
                }));
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
                    unknown_calls: 0,
                },
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle()
}

pub fn text_response(text: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

pub fn last_user_text(request: &LlmRequest) -> String {
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

/// Test convenience over [`SessionConfigAdmin`](lash::config::SessionConfigAdmin):
/// apply a config transaction against the session's current config revision,
/// under a fresh id, and require it to apply.
pub trait ConfigureExt {
    fn configure(
        &self,
        transaction: lash::config::ConfigTransaction,
    ) -> impl std::future::Future<Output = Result<()>> + Send;
}

impl ConfigureExt for lash::config::SessionConfigAdmin {
    async fn configure(&self, transaction: lash::config::ConfigTransaction) -> Result<()> {
        let revision = self.revision().await?;
        let write = lash::config::ConfigWrite::new(
            format!("test-config:{}", uuid::Uuid::new_v4()),
            revision,
        );
        match self
            .apply(write, transaction)
            .await?
            .await_outcome(self)
            .await?
        {
            lash::config::ConfigTransactionOutcome::Applied { .. } => Ok(()),
            outcome => Err(EmbedError::Session(lash_core::SessionError::Protocol(
                format!("the config transaction did not apply: {outcome:?}"),
            ))),
        }
    }
}

/// A turn-activity sink that keeps everything it was handed, in order.
#[derive(Default)]
pub struct RecordingEvents {
    events: tokio::sync::Mutex<Vec<TurnActivity>>,
}

impl RecordingEvents {
    pub async fn snapshot(&self) -> Vec<TurnActivity> {
        self.events.lock().await.clone()
    }
}

#[async_trait]
impl TurnActivitySink for RecordingEvents {
    async fn emit(&self, activity: TurnActivity) {
        self.events.lock().await.push(activity);
    }
}

/// `source` as one TypeScript cell of an RLM answer.
#[cfg(feature = "rlm")]
pub fn typescript_block(source: &str) -> String {
    format!("<typescript>\n{}\n</typescript>", source.trim())
}

/// An RLM core builder over `backend`, with the default test factory.
#[cfg(feature = "rlm")]
pub fn rlm_core_builder_over(backend: lash_core::Backend) -> lash::LashCoreBuilder {
    let factory = rlm_factory();
    LashCore::rlm_builder(backend, factory)
}

/// The session's committed head as a read view: its turns run on its
/// session actor, so a host's open holds only the snapshot it opened with.
pub async fn committed(session: &lash::LashSession) -> lash_core::SessionReadView {
    session
        .durable()
        .read()
        .await
        .expect("read the committed head")
        .expect("the session has a head")
}

/// `definition` bound under `tools.<name>`.
pub fn test_tool_definition_with_tool_binding(
    definition: lash_core::ToolDefinition,
    name: impl Into<String>,
) -> lash_core::ToolDefinition {
    lash_core::ToolDefinitionBindingExt::with_tool_binding(
        definition,
        lash_core::ToolBinding::new(["tools"], name),
    )
}

/// A provider that answers each request with the next of `texts`.
pub fn queued_text_provider(texts: Vec<impl Into<String>>) -> ProviderHandle {
    let responses = Arc::new(tokio::sync::Mutex::new(
        texts
            .into_iter()
            .map(|text| text_response(&text.into()))
            .collect::<std::collections::VecDeque<_>>(),
    ));
    lash_core::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |_request| {
            let responses = Arc::clone(&responses);
            async move { Ok(responses.lock().await.pop_front().expect("queued response")) }
        })
        .build()
        .into_handle()
}

/// Delete `session_id` through the core's administration and await its
/// tombstone: the session actor closes itself (ADR 0132 §12).
pub async fn delete_session_and_await(core: &LashCore, session_id: &str) -> Result<()> {
    let administration = core.session_administration().await;
    LashCore::delete_session(administration.delete_context(session_id)?).await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        core.await_session_deletion(&SessionId::fixture(session_id.to_owned())),
    )
    .await
    .expect("the session's deletion completes")?;
    Ok(())
}

/// A standard core over `backend`.
pub fn standard_core_over(backend: lash_core::Backend) -> LashCore {
    standard_core_builder_over(backend)
        .build(runtime_lease_owner())
        .expect("standard core")
}

/// The builder of a standard core over `backend`, for a law that names the
/// core's owner or pacing itself.
pub fn standard_core_builder_over(backend: lash_core::Backend) -> lash::LashCoreBuilder {
    explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
}

/// A catalog serving every model in `models` by its wire model, on `provider`.
pub fn test_catalog(
    provider: ProviderHandle,
    models: impl IntoIterator<Item = lash_core::LlmProfileMetadata>,
) -> Arc<lash_core::LlmProfileRegistry> {
    let registry = models
        .into_iter()
        .try_fold(
            lash_core::LlmProfileRegistry::new(),
            |registry, metadata| {
                registry.register(
                    metadata.wire_model.clone(),
                    lash_core::RegisteredLlmProfile::new(metadata, provider.clone()),
                )
            },
        )
        .expect("a test catalog registers each wire model once");
    Arc::new(registry)
}

/// A provider that answers `text` to every request.
pub fn text_provider(kind: &'static str, text: impl Into<String>) -> ProviderHandle {
    let text: Arc<str> = text.into().into();
    lash_core::testing::TestProvider::builder()
        .kind(kind)
        .complete(move |_request| {
            let text = Arc::clone(&text);
            async move { Ok(text_response(&text)) }
        })
        .build()
        .into_handle()
}

/// The request's instructions, the system prompt the protocol rendered.
pub fn system_text(request: &LlmRequest) -> String {
    request
        .instructions
        .as_deref()
        .unwrap_or_default()
        .to_owned()
}

/// Every text block of the request's messages, joined.
pub fn request_text(request: &LlmRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every node of `durable`'s ancestry from its head, newest first, paged
/// through the history reader: prior frames stay durable but not resident.
pub async fn durable_history(
    durable: &lash::DurableSession,
) -> Result<Vec<lash_core::store::HistoryNode>> {
    let budget = lash_core::store::HistoryBudget {
        max_nodes: std::num::NonZeroU32::new(64).expect("non-zero page"),
        max_bytes: std::num::NonZeroU64::new(1 << 20).expect("non-zero page"),
    };
    let mut anchor = lash_core::store::HistoryAnchor::Head;
    let mut nodes = Vec::new();
    loop {
        let page = durable.history(anchor, budget).await?;
        nodes.extend(page.nodes);
        match page.next {
            Some(cursor) => anchor = lash_core::store::HistoryAnchor::Cursor(cursor),
            None => return Ok(nodes),
        }
    }
}

/// The agent frames `history` records, oldest first.
pub fn history_frames(
    history: Vec<lash_core::store::HistoryNode>,
    session_id: &lash_core::SessionId,
) -> Vec<lash_core::AgentFrameRecord> {
    use lash_core::facade_support::SessionGraphFacadeOps as _;
    let records = history
        .into_iter()
        .rev()
        .map(|node| node.record)
        .collect::<Vec<_>>();
    let leaf = records.last().map(|record| record.node_id.clone());
    lash_core::SessionGraph::from_nodes(records, leaf)
        .expect("a durable ancestry is a valid graph")
        .agent_frame_records(session_id)
}

/// A deferring tool source with one tool, `app_lookup`.
pub struct AppTools;

#[async_trait]
impl ToolProvider for AppTools {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash::plugins::PluginError> {
        Ok(vec![app_tool_definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(app_tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true })).into()
    }
}

pub fn app_tool_definition() -> lash_core::ToolDefinition {
    use lash_core::ToolDefinitionBindingExt as _;
    lash_core::ToolDefinition::raw(
        "tool:app_lookup",
        "app_lookup",
        "Look up app state.",
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], "app_lookup"))
    .with_declaration(
        lash_core::ToolDeclaration::deferring(),
        Some(lash_core::ParkBound::Within(
            std::time::Duration::from_secs(120),
        )),
    )
    .expect("a deferring tool declares its park bound")
}

/// Default RLM protocol factory for tests.
#[cfg(feature = "rlm")]
pub fn rlm_factory() -> lash_protocol_rlm::RlmProtocolPluginFactory {
    lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::standard())
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        lash_protocol_rlm::CellDialect::typescript(),
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
pub fn untimed_fixture_workers() -> lash::vm::WorkerService {
    /// Longer than any run the instruction budget admits.
    const OFF_THE_CLOCK: std::time::Duration = std::time::Duration::from_secs(365 * 24 * 60 * 60);
    let mut config = lash_vm_client::service::Service::default().config().clone();
    config.deadlines.compute = OFF_THE_CLOCK;
    config.deadlines.serialization = OFF_THE_CLOCK;
    config.deadlines.cumulative_cpu = OFF_THE_CLOCK;
    lash::vm::WorkerService::new(config)
}

pub mod fixtures;
mod harness;
pub use fixtures::{LongTextTools, SurfacePluginFactory};
pub use harness::{
    DecoratedBackend, explicit_ephemeral_facets, explicit_ephemeral_facets_with_budget,
    llm_profile_spec, mock_llm_profile_spec, mock_session_spec, postgres_store_parts,
    postgres_store_set, recorded_llm_profile, session_spec_for, sqlite_memory_store_backend,
    sqlite_memory_store_set, store_backend_with_clock,
};

/// The test host's fixed worker incarnation.
pub fn runtime_lease_owner() -> lash_core::LeaseOwnerIdentity {
    lash_core::LeaseOwnerIdentity::opaque(
        lash_core::LeaseOwnerId::new("lash-runtime-test-worker"),
        lash_core::LeaseIncarnationId::new("lash-runtime-test-boot"),
    )
}

/// Create with the host's default config, then return the public open builder.
pub async fn created_session(core: &LashCore, id: lash::SessionId) -> lash::SessionBuilder {
    created_session_with(core, id, mock_session_spec()).await
}

/// Match the original fixture's create-or-open behavior through the public API.
pub async fn created_session_with(
    core: &LashCore,
    id: lash::SessionId,
    spec: lash::SessionSpec,
) -> lash::SessionBuilder {
    match core
        .session(id.clone())
        .create(lash::SessionCreation::root(spec))
        .await
    {
        Ok(_)
        | Err(EmbedError::SessionAlreadyExists { .. })
        | Err(EmbedError::Store(StoreError::SessionDeleted { .. })) => core.session(id),
        Err(error) => panic!("create session `{id}`: {error:?}"),
    }
}

#[cfg(feature = "rlm")]
pub fn control_cell_response(
    dialect: &lash::rlm::CellDialect,
    channel: lash::rlm::RlmChannel,
    index: usize,
    code: String,
) -> LlmResponse {
    if channel == lash::rlm::RlmChannel::Cell {
        let tags = dialect.prompts().prompt_vocabulary().cell_tags;
        text_response(&format!("{}\n{code}\n{}", tags.open, tags.close))
    } else {
        LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: format!("cell-{index}"),
                tool_name: "execute_code".into(),
                input_json: serde_json::json!({"code": code}).to_string(),
                replay: None,
            }],
            ..Default::default()
        }
    }
}
pub mod ingress;

pub mod send;

pub mod streaming;

pub mod turn;

pub mod durable;

pub mod observations;

pub mod session_process;

pub mod history;

pub fn diagnostic_core(backend: lash_core::Backend) -> LashCore {
    explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_sessions(false)
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(runtime_lease_owner())
        .expect("diagnostic core")
}
pub mod controls;
