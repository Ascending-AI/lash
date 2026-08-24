use lash_sansio::sync::MutexExt;
use sha2::Digest as _;
use std::sync::{Arc, OnceLock};

use crate::PluginMessage;
use crate::tool_dispatch::ToolDispatchContext;
use crate::{PromptContribution, RuntimeServices, SessionStreamEvent, ToolProvider};

mod execution_context;
#[cfg(test)]
mod fig790_tests;
mod process_handles;
#[cfg(test)]
mod settlement_latency_tests;
mod tool_execution;

pub use execution_context::RuntimeExecutionContext;
pub(crate) use execution_context::{RuntimeExecutionProcessEventContext, RuntimeExecutionTracing};
pub use tool_execution::{ToolInvocation, ToolInvocationReply};

#[derive(Clone, Debug, PartialEq, Eq)]
struct ToolCatalogCacheKey {
    include_base_tools: bool,
    context_overlay_revision: u64,
    tool_generation: u64,
    plugin_revision: u64,
    authority_fingerprint: [u8; 32],
}

#[derive(Debug, Default)]
struct ToolCatalogDerived {
    catalog: OnceLock<Arc<Vec<serde_json::Value>>>,
}

struct ToolCatalogArtifact {
    tool_catalog: Arc<crate::ToolCatalog>,
    preamble: Arc<crate::TurnDriverPreamble>,
    derived: ToolCatalogDerived,
}

#[derive(Clone)]
pub(crate) struct ToolCatalogHandle(Arc<ToolCatalogArtifact>);

type ToolContractFingerprints = Arc<Vec<[u8; 32]>>;
type CompositionToolFingerprintCache = Arc<
    std::sync::Mutex<
        Vec<(
            Arc<Vec<crate::llm::types::LlmToolSpec>>,
            ToolContractFingerprints,
        )>,
    >,
>;

impl ToolCatalogHandle {
    fn tool_catalog(&self) -> Arc<crate::ToolCatalog> {
        Arc::clone(&self.0.tool_catalog)
    }

    fn preamble(&self) -> Arc<crate::TurnDriverPreamble> {
        Arc::clone(&self.0.preamble)
    }

    fn catalog(&self) -> Arc<Vec<serde_json::Value>> {
        Arc::clone(self.0.derived.catalog.get_or_init(|| {
            Arc::new(crate::tool_registry::project_tool_catalog(
                self.0.tool_catalog.tools.iter().cloned(),
            ))
        }))
    }
}

#[derive(Clone, Debug)]
pub struct InjectedTurnInput {
    pub id: Option<String>,
    pub message: PluginMessage,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("code execution is not available in this session")]
    CodeExecutionUnavailable,
    #[error("code execution runtime exited unexpectedly")]
    CodeExecutionRuntimeStopped,
    #[error(
        "provider mismatch for session `{session_id}`: persisted provider `{expected}` does not match live provider `{actual}`"
    )]
    ProviderMismatch {
        expected: String,
        actual: String,
        session_id: String,
    },
    #[error("provider is not configured for session `{session_id}`")]
    ProviderUnconfigured { session_id: String },
    #[error("provider `{provider_id}` is not registered for session `{session_id}`")]
    ProviderUnavailable {
        provider_id: String,
        session_id: String,
    },
    #[error("{context}: {source}")]
    Store {
        context: String,
        #[source]
        source: crate::StoreError,
    },
    #[error("session config command has not settled yet: {0}")]
    SessionCommandPending(crate::SessionCommandReceipt),
    #[error("session config command was cancelled before settlement: {0}")]
    SessionCommandCancelled(crate::SessionCommandReceipt),
    #[error("protocol error: {0}")]
    Protocol(String),
}

#[derive(Clone, Debug)]
pub struct ExecRequest {
    pub language: String,
    pub code: String,
    pub accept_finish: bool,
}

pub struct Session {
    session_id: String,
    services: RuntimeServices,
    include_base_tools: bool,
    context_overlay_revision: u64,
    context_tools: Vec<Arc<dyn ToolProvider>>,
    tool_registry: Arc<crate::ToolRegistry>,
    context_prompt_contributions: Vec<PromptContribution>,
    tool_catalog_cache: Arc<std::sync::Mutex<Option<(ToolCatalogCacheKey, ToolCatalogHandle)>>>,
    composition_tool_fingerprint_cache: CompositionToolFingerprintCache,
    /// Memoizes the rendered system prompt across turns. Most consecutive
    /// turns reuse the same template + context overlay, so the cache hits
    /// and we skip the section/Vec-join work in
    /// `lash_sansio::PromptTemplate::render`.
    prompt_cache: Arc<lash_sansio::PromptCache>,
    /// Fingerprint of the last model-facing composition emitted to the trace
    /// sink for this resident session. Effect-driver clones share the slot so
    /// a mid-turn execution-environment refresh cannot double-emit it.
    composition_trace_fingerprint: Arc<std::sync::Mutex<Option<[u8; 32]>>>,
}

impl Session {
    pub async fn new(services: RuntimeServices, session_id: &str) -> Result<Self, SessionError> {
        let tool_registry = services.plugins.tool_registry();
        let mut session = Self {
            session_id: session_id.to_string(),
            services,
            include_base_tools: true,
            context_overlay_revision: 0,
            context_tools: Vec::new(),
            tool_registry,
            context_prompt_contributions: Vec::new(),
            tool_catalog_cache: Arc::new(std::sync::Mutex::new(None)),
            composition_tool_fingerprint_cache: Arc::new(std::sync::Mutex::new(Vec::new())),
            prompt_cache: Arc::new(lash_sansio::PromptCache::new()),
            composition_trace_fingerprint: Arc::new(std::sync::Mutex::new(None)),
        };

        let protocol_session = Arc::clone(session.plugins().protocol_session());
        protocol_session
            .initialize_session(crate::plugin::ProtocolSessionContext::new(
                &mut session,
                session_id,
            ))
            .await?;

        Ok(session)
    }

    pub(crate) fn clone_for_effect(&self) -> Self {
        Self {
            session_id: self.session_id.clone(),
            services: self.services.clone(),
            include_base_tools: self.include_base_tools,
            context_overlay_revision: self.context_overlay_revision,
            context_tools: self.context_tools.clone(),
            tool_registry: Arc::clone(&self.tool_registry),
            context_prompt_contributions: self.context_prompt_contributions.clone(),
            tool_catalog_cache: Arc::clone(&self.tool_catalog_cache),
            composition_tool_fingerprint_cache: Arc::clone(
                &self.composition_tool_fingerprint_cache,
            ),
            prompt_cache: Arc::clone(&self.prompt_cache),
            composition_trace_fingerprint: Arc::clone(&self.composition_trace_fingerprint),
        }
    }

    pub(crate) fn record_composition_trace_fingerprint(&self, fingerprint: [u8; 32]) -> bool {
        let mut last = self.composition_trace_fingerprint.lock_recover();
        if last.as_ref() == Some(&fingerprint) {
            return false;
        }
        *last = Some(fingerprint);
        true
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn protocol_extra_prompt_contributions(&self) -> Vec<PromptContribution> {
        // Protocol-specific prompt contributions are owned by the protocol
        // plugins via their
        // `reg.prompt().contribute(...)` hooks. Nothing to add here.
        Vec::new()
    }

    pub fn tools(&self) -> Arc<dyn ToolProvider> {
        Arc::clone(&self.tool_registry) as Arc<dyn ToolProvider>
    }

    pub fn plugins(&self) -> &Arc<crate::PluginSession> {
        &self.services.plugins
    }

    pub fn set_context_overlay(
        &mut self,
        tool_providers: Vec<Arc<dyn ToolProvider>>,
        prompt_contributions: Vec<PromptContribution>,
        include_base_tools: bool,
    ) -> Result<(), crate::PluginError> {
        let tool_providers_unchanged = self.context_tools.len() == tool_providers.len()
            && self
                .context_tools
                .iter()
                .zip(&tool_providers)
                .all(|(current, next)| Arc::ptr_eq(current, next));
        let overlay_unchanged = self.include_base_tools == include_base_tools
            && self.context_prompt_contributions == prompt_contributions
            && tool_providers_unchanged;
        let registry = self
            .services
            .plugins
            .tool_registry()
            .compose_session_catalog(include_base_tools, tool_providers.clone())
            .map(Arc::new)
            .map_err(|err| {
                crate::PluginError::Session(format!("failed to build session tool registry: {err}"))
            })?;
        self.include_base_tools = include_base_tools;
        if !overlay_unchanged {
            self.context_overlay_revision = self.context_overlay_revision.wrapping_add(1);
        }
        self.context_tools = tool_providers;
        self.tool_registry = registry;
        self.context_prompt_contributions = prompt_contributions;
        *self.tool_catalog_cache.lock_recover() = None;
        Ok(())
    }

    pub fn prompt_cache(&self) -> Arc<lash_sansio::PromptCache> {
        Arc::clone(&self.prompt_cache)
    }

    pub fn context_prompt_contributions(&self) -> &[PromptContribution] {
        &self.context_prompt_contributions
    }

    pub fn history_store(&self) -> Option<Arc<dyn crate::store::RuntimePersistence>> {
        self.services.store.clone()
    }

    fn tool_catalog_cache_key(&self) -> ToolCatalogCacheKey {
        ToolCatalogCacheKey {
            include_base_tools: self.include_base_tools,
            context_overlay_revision: self.context_overlay_revision,
            tool_generation: self.tool_registry.generation(),
            plugin_revision: self.plugins().snapshot_revision_fingerprint(),
            authority_fingerprint: tool_catalog_authority_fingerprint(self.plugins().tool_access()),
        }
    }

    fn build_tool_catalog_entry(
        &self,
        session_id: &str,
    ) -> Result<ToolCatalogHandle, crate::PluginError> {
        let provider = self.tools();
        let tools = provider.tool_manifests();
        let contract_provider = Arc::clone(&provider);
        let resolve_contract: lash_sansio::ToolContractResolver =
            Arc::new(move |name: &str| contract_provider.resolve_contract(name));
        let tool_catalog = Arc::new(self.plugins().resolve_tool_catalog(
            crate::plugin::ToolCatalogContext {
                session_id: session_id.to_string(),
                tools,
                resolve_contract: Some(Arc::clone(&resolve_contract)),
                tool_access: self.plugins().tool_access().clone(),
                subagent: self.plugins().subagent_context().cloned(),
                extensions: self.plugins().extensions().clone(),
            },
        )?);
        let input = crate::ProtocolBuildInput {
            tool_catalog: Arc::clone(&tool_catalog),
            plugin_extensions: self.plugins().extensions().clone(),
            trigger_events: self.plugins().triggers().clone(),
            extra_prompt_contributions: self.protocol_extra_prompt_contributions(),
        };
        let driver = self.plugins().protocol_driver();
        let preamble = driver.build_preamble(input);
        Ok(ToolCatalogHandle(Arc::new(ToolCatalogArtifact {
            tool_catalog,
            preamble: Arc::new(preamble),
            derived: ToolCatalogDerived::default(),
        })))
    }

    fn tool_catalog_cache_entry(
        &self,
        session_id: &str,
    ) -> Result<ToolCatalogHandle, crate::PluginError> {
        let key = self.tool_catalog_cache_key();
        let mut cache = self.tool_catalog_cache.lock_recover();
        if let Some((entry_key, entry)) = cache.as_ref()
            && *entry_key == key
        {
            return Ok(entry.clone());
        }
        let entry = self.build_tool_catalog_entry(session_id)?;
        *cache = Some((key, entry.clone()));
        Ok(entry)
    }

    pub fn resolved_tool_catalog(
        &self,
        session_id: &str,
    ) -> Result<Arc<crate::ToolCatalog>, crate::PluginError> {
        Ok(self.tool_catalog_cache_entry(session_id)?.tool_catalog())
    }

    pub(crate) fn turn_driver_preamble(
        &self,
        session_id: &str,
    ) -> Result<Arc<crate::TurnDriverPreamble>, crate::PluginError> {
        Ok(self.tool_catalog_cache_entry(session_id)?.preamble())
    }

    pub(crate) fn composition_tool_fingerprints(
        &self,
        tools: &Arc<Vec<crate::llm::types::LlmToolSpec>>,
    ) -> ToolContractFingerprints {
        let mut cache = self.composition_tool_fingerprint_cache.lock_recover();
        if let Some((_, fingerprints)) = cache.iter().find(|(cached_tools, _)| {
            Arc::ptr_eq(cached_tools, tools) || cached_tools.as_slice() == tools.as_slice()
        }) {
            return Arc::clone(fingerprints);
        }
        let fingerprints = Arc::new(
            tools
                .iter()
                .map(crate::trace::composition_tool_fingerprint)
                .collect(),
        );
        const MAX_COMPOSITION_FINGERPRINT_GENERATIONS: usize = 8;
        if cache.len() == MAX_COMPOSITION_FINGERPRINT_GENERATIONS {
            cache.remove(0);
        }
        cache.push((Arc::clone(tools), Arc::clone(&fingerprints)));
        fingerprints
    }

    pub(crate) fn shared_tool_catalog(
        &self,
        session_id: &str,
    ) -> Result<Arc<Vec<serde_json::Value>>, crate::PluginError> {
        Ok(self.tool_catalog_cache_entry(session_id)?.catalog())
    }

    pub fn tool_catalog(
        &self,
        session_id: &str,
    ) -> Result<Vec<serde_json::Value>, crate::PluginError> {
        Ok(self.shared_tool_catalog(session_id)?.as_ref().clone())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "code execution bridge carries explicit per-turn runtime dependencies"
    )]
    pub(crate) fn code_execution_context<'run>(
        &self,
        session_id: &str,
        agent_frame_id: &str,
        sessions: Arc<dyn crate::plugin::SessionStateService>,
        session_lifecycle: Arc<dyn crate::plugin::SessionLifecycleService>,
        session_graph: Arc<dyn crate::plugin::SessionGraphService>,
        processes: Arc<dyn crate::ProcessService>,
        effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
        direct_completions: crate::DirectCompletionClient<'run>,
        trigger_router: Option<crate::TriggerRouter>,
        event_tx: tokio::sync::mpsc::Sender<SessionStreamEvent>,
        chronological_projection: Arc<crate::ChronologicalProjection>,
        protocol_extension: Option<crate::ProtocolTurnExtensionHandle>,
        turn_context: crate::TurnContext,
        execution_env_spec: crate::ProcessExecutionEnvSpec,
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer,
        recorded_intent_outcomes: crate::tool_dispatch::RecordedToolIntentOutcomeBuffer,
        attachment_source_policy: Arc<dyn crate::AttachmentSourcePolicy>,
    ) -> Result<RuntimeExecutionContext<'run>, crate::PluginError> {
        let dispatch = Arc::new(ToolDispatchContext {
            plugins: Arc::clone(self.plugins()),
            tools: self.tools(),
            tool_registry: Some(Arc::clone(&self.tool_registry)),
            tool_catalog: self.resolved_tool_catalog(session_id)?,
            sessions,
            session_lifecycle,
            session_graph,
            processes,
            trigger_router,
            effect_controller,
            direct_completions: direct_completions.clone(),
            parent_invocation: None,
            execution_env_spec: execution_env_spec.clone(),
            session_id: session_id.to_string(),
            agent_frame_id: agent_frame_id.to_string(),
            event_tx,
            checkpoint_messages,
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            recorded_intent_outcomes,
            attachment_store: Arc::clone(&self.services.attachment_store),
            attachment_source_policy,
            turn_context: turn_context.clone(),
            clock: Arc::clone(&self.services.clock),
        });
        Ok(RuntimeExecutionContext::new(
            session_id.to_string(),
            dispatch,
            Arc::clone(&self.services.process_env_store),
            Arc::clone(&self.services.attachment_store),
            chronological_projection,
            protocol_extension,
            turn_context,
        ))
        .map(|context| context.with_execution_env_spec(execution_env_spec))
    }

    pub fn invalidate_runtime_caches(&self) {
        *self.tool_catalog_cache.lock_recover() = None;
        self.prompt_cache.clear();
    }

    pub async fn refresh_tool_catalog(&mut self) -> Result<(), SessionError> {
        self.tool_registry = self
            .services
            .plugins
            .tool_registry()
            .compose_session_catalog(self.include_base_tools, self.context_tools.clone())
            .map(Arc::new)
            .map_err(|err| SessionError::Protocol(format!("tool reconfigure failed: {err}")))?;
        *self.tool_catalog_cache.lock_recover() = None;
        Ok(())
    }
}

fn tool_catalog_authority_fingerprint(tool_access: &crate::SessionToolAccess) -> [u8; 32] {
    let encoded = serde_json::to_vec(tool_access)
        .expect("SessionToolAccess is composed entirely of serializable authority values");
    sha2::Sha256::digest(encoded).into()
}

#[cfg(test)]
mod tool_catalog_cache_tests {
    use super::*;

    #[test]
    fn authority_fingerprint_covers_hidden_tools_and_explicit_definitions() {
        let base = crate::SessionToolAccess::default();
        let mut hidden = base.clone();
        hidden.hidden_tools.insert("hidden".to_string());
        let explicit = crate::SessionToolAccess {
            tools: vec![crate::ToolDefinition::raw(
                "tool:explicit",
                "explicit",
                "authority-defined tool",
                crate::ToolDefinition::default_input_schema(),
                serde_json::json!({ "type": "string" }),
            )],
            ..crate::SessionToolAccess::default()
        };

        assert_ne!(
            tool_catalog_authority_fingerprint(&base),
            tool_catalog_authority_fingerprint(&hidden)
        );
        assert_ne!(
            tool_catalog_authority_fingerprint(&base),
            tool_catalog_authority_fingerprint(&explicit)
        );
    }
}
