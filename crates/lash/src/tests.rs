use crate::support::{
    Arc, EmbedError, LashCore, PluginFactory, ProcessRegistry, Result, StaticPluginFactory,
    ToolProvider, TurnActivity, TurnActivitySink, TurnInput, async_trait,
};
use lash_core::facade_support::ProviderHandle;
use lash_sansio::SessionId;
use lash_sansio::llm::types::{StreamBlockEvent, StreamBlockKind};
use lash_sansio::sync::MutexExt;
use std::sync::Mutex as StdMutex;
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
            .create(crate::SessionCreation::root(
                crate::plugins::SessionToolAccess::ambient(),
                spec,
            ))
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

/// Test convenience over [`SessionConfigAdmin`](crate::admin::SessionConfigAdmin):
/// apply a config transaction against the session's current config revision,
/// under a fresh id, and require it to apply.
pub(crate) trait ConfigureExt {
    async fn configure(&self, transaction: crate::config::ConfigTransaction) -> Result<()>;
}

impl ConfigureExt for crate::admin::SessionConfigAdmin {
    async fn configure(&self, transaction: crate::config::ConfigTransaction) -> Result<()> {
        let revision = self.revision().await?;
        let write = crate::config::ConfigWrite::new(
            format!("test-config:{}", uuid::Uuid::new_v4()),
            revision,
        );
        match self
            .apply(write, transaction)
            .await?
            .await_outcome(self)
            .await?
        {
            crate::config::ConfigTransactionOutcome::Applied { .. } => Ok(()),
            outcome => Err(EmbedError::Session(crate::support::SessionError::Protocol(
                format!("the config transaction did not apply: {outcome:?}"),
            ))),
        }
    }
}

/// A turn-activity sink that keeps everything it was handed, in order.
#[derive(Default)]
struct RecordingEvents {
    events: tokio::sync::Mutex<Vec<TurnActivity>>,
}

impl RecordingEvents {
    async fn snapshot(&self) -> Vec<TurnActivity> {
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
fn typescript_block(source: &str) -> String {
    format!("<typescript>\n{}\n</typescript>", source.trim())
}

/// An RLM core builder over `backend`, with the default test factory.
#[cfg(feature = "rlm")]
fn rlm_core_builder_over(backend: lash_core::Backend) -> crate::core::LashCoreBuilder {
    let factory = rlm_factory();
    LashCore::rlm_builder(backend, factory)
}

/// The session's committed head as a read view: its turns run on its
/// session actor, so a host's open holds only the snapshot it opened with.
async fn committed(session: &crate::LashSession) -> lash_core::SessionReadView {
    session
        .durable()
        .read()
        .await
        .expect("read the committed head")
        .expect("the session has a head")
}

/// `definition` bound under `tools.<name>`.
fn test_tool_definition_with_tool_binding(
    definition: lash_core::ToolDefinition,
    name: impl Into<String>,
) -> lash_core::ToolDefinition {
    lash_core::ToolDefinitionBindingExt::with_tool_binding(
        definition,
        lash_core::ToolBinding::new(["tools"], name),
    )
}

/// A provider that answers each request with the next of `texts`.
fn queued_text_provider(texts: Vec<impl Into<String>>) -> ProviderHandle {
    let responses = Arc::new(tokio::sync::Mutex::new(
        texts
            .into_iter()
            .map(|text| text_response(&text.into()))
            .collect::<std::collections::VecDeque<_>>(),
    ));
    crate::testing::TestProvider::builder()
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
async fn delete_session_and_await(core: &LashCore, session_id: &str) -> Result<()> {
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
pub(crate) fn standard_core_over(backend: lash_core::Backend) -> LashCore {
    standard_core_builder_over(backend)
        .build(crate::testing::runtime_lease_owner())
        .expect("standard core")
}

/// The builder of a standard core over `backend`, for a law that names the
/// core's owner or pacing itself.
pub(crate) fn standard_core_builder_over(backend: lash_core::Backend) -> crate::LashCoreBuilder {
    explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
}

/// A catalog serving every model in `models` by its wire model, on `provider`.
pub(crate) fn test_catalog(
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
fn text_provider(kind: &'static str, text: impl Into<String>) -> ProviderHandle {
    let text: Arc<str> = text.into().into();
    crate::testing::TestProvider::builder()
        .kind(kind)
        .complete(move |_request| {
            let text = Arc::clone(&text);
            async move { Ok(text_response(&text)) }
        })
        .build()
        .into_handle()
}

/// The request's instructions, the system prompt the protocol rendered.
fn system_text(request: &LlmRequest) -> String {
    request
        .instructions
        .as_deref()
        .unwrap_or_default()
        .to_owned()
}

/// Every text block of the request's messages, joined.
fn request_text(request: &LlmRequest) -> String {
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
pub(crate) async fn durable_history(
    durable: &crate::DurableSession,
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
pub(crate) fn history_frames(
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
struct AppTools;

#[async_trait]
impl ToolProvider for AppTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![app_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(app_tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true })).into()
    }
}

fn app_tool_definition() -> lash_core::ToolDefinition {
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
fn rlm_factory() -> lash_protocol_rlm::RlmProtocolPluginFactory {
    lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
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
fn untimed_fixture_workers() -> crate::vm::WorkerService {
    /// Longer than any run the instruction budget admits.
    const OFF_THE_CLOCK: std::time::Duration = std::time::Duration::from_secs(365 * 24 * 60 * 60);
    let mut config = lash_vm_client::service::Service::default().config().clone();
    config.deadlines.compute = OFF_THE_CLOCK;
    config.deadlines.serialization = OFF_THE_CLOCK;
    config.deadlines.cumulative_cpu = OFF_THE_CLOCK;
    crate::vm::WorkerService::new(config)
}

mod absent_session_delete;
mod assistant_hook_faults;
#[cfg(feature = "rlm")]
mod cell_race_loser;
mod config_transactions;
mod core_session_builder;
mod crashed_create_drain;
mod cross_core_process_changes;
mod deleted_session_run_replay;
mod deployment_and_testing_facade;
mod direct_completion;
mod direct_completion_lanes;
#[cfg(feature = "rlm")]
mod discovery_execution;
mod durable_session;
mod facade_construction;
mod facade_turn;
mod fixtures;
mod generation_policy;
use fixtures::{LongTextTools, SurfacePluginFactory};
pub(crate) mod harness;
mod node_drain;
mod output_retention;
mod panic_containment;
mod projection;
#[cfg(feature = "rlm")]
mod recorded_carriers;
mod telemetry_content;
#[cfg(feature = "otel-trace")]
mod trace_emission;
mod tracing;
pub(crate) use harness::{
    DecoratedBackend, explicit_ephemeral_facets, explicit_ephemeral_facets_with_budget,
    llm_profile_spec, mock_llm_profile_spec, mock_session_spec, postgres_store_parts,
    postgres_store_set, recorded_llm_profile, session_spec_for, sqlite_memory_store_backend,
    sqlite_memory_store_set, store_backend_with_clock,
};
#[cfg(feature = "rlm")]
mod admin_reads_without_runtime;
#[cfg(feature = "rlm")]
mod adr_claims;
#[cfg(feature = "rlm")]
mod agent_scenarios;
#[cfg(feature = "rlm")]
mod aggregate_await_comprehension;
mod plugin_build_refusal;
mod plugin_lifecycle;
mod plugin_operations;
mod plugin_reopen;
mod plugin_stack;
mod presentation_fallback;
mod process_start_input;
mod protocol_effects;
mod provider_attempts;
mod recorded_execution_controls;
mod recorded_protocol_prompt;
mod recorded_request_defaults;
mod replay_origin;
mod response_phase_replay;
mod round_cancel_state;
mod run_effects;
mod send_handle;
mod session_control;
mod session_create;
mod session_turn_process;
mod standard_compaction_persistence;
mod standard_protocol_turns;
mod store_faults;
mod stream_evidence;
mod tool_check_control;
mod tool_intent_ingress;
mod tool_restore_report;
mod turn_cancel_modes;
#[cfg(feature = "rlm")]
mod turn_cancel_waits;
mod turn_checkpoints;
mod turn_streaming;
mod workflow_reads;
mod writer_fence;
