use std::sync::Arc;

use super::{EmptyToolProvider, MockSessionManager, test_code_protocol_factories};

pub(crate) enum TestSessionHostMode {
    Independent,
    Shared(Arc<MockSessionManager>),
}

pub(crate) enum TestEffectController<'run> {
    Shared(Arc<dyn crate::RuntimeEffectController>),
    Borrowed(crate::ScopedEffectController<'run>),
}

pub(crate) struct TestExecutionContextBuilder<'run> {
    session_id: String,
    provider: Arc<dyn crate::ToolProvider>,
    tool_catalog: crate::ToolCatalog,
    trigger_router: Option<crate::TriggerRouter>,
    processes: Arc<dyn crate::ProcessService>,
    process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    execution_env_spec: crate::ProcessExecutionEnvSpec,
    session_host_mode: TestSessionHostMode,
    session_lifecycle: Option<Arc<dyn crate::plugin::SessionLifecycleService>>,
    effect_controller: TestEffectController<'run>,
    dispatch_parent_invocation: Option<crate::RuntimeInvocation>,
    runtime_parent_invocation: Option<crate::RuntimeInvocation>,
    attachment_store: Arc<crate::SessionAttachmentStore>,
    clock: Arc<dyn crate::Clock>,
}

pub(crate) struct BuiltTestExecutionContext<'run> {
    pub(crate) dispatch: Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
    pub(crate) process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    pub(crate) execution_env_spec: crate::ProcessExecutionEnvSpec,
    pub(crate) runtime_parent_invocation: Option<crate::RuntimeInvocation>,
}

impl<'run> TestExecutionContextBuilder<'run> {
    pub(crate) fn new() -> Self {
        Self {
            session_id: "test-session".to_string(),
            provider: Arc::new(EmptyToolProvider),
            tool_catalog: crate::ToolCatalog::from_tool_definitions(Vec::new()),
            trigger_router: None,
            processes: Arc::new(crate::UnavailableProcessService),
            process_env_store: Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            ),
            session_host_mode: TestSessionHostMode::Independent,
            session_lifecycle: None,
            effect_controller: TestEffectController::Shared(Arc::new(
                crate::InlineRuntimeEffectController::default()
                    .allow_process_lifetime_completion_keys(),
            )),
            dispatch_parent_invocation: None,
            runtime_parent_invocation: None,
            attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
            clock: Arc::new(crate::SystemClock),
        }
    }

    pub(crate) fn session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = session_id.into();
        self
    }

    pub(crate) fn provider(mut self, provider: Arc<dyn crate::ToolProvider>) -> Self {
        self.provider = provider;
        self
    }

    pub(crate) fn tool_catalog(mut self, tool_catalog: crate::ToolCatalog) -> Self {
        self.tool_catalog = tool_catalog;
        self
    }

    pub(crate) fn trigger_router(mut self, trigger_router: Option<crate::TriggerRouter>) -> Self {
        self.trigger_router = trigger_router;
        self
    }

    pub(crate) fn processes(mut self, processes: Arc<dyn crate::ProcessService>) -> Self {
        self.processes = processes;
        self
    }

    pub(crate) fn process_env_store(
        mut self,
        process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    ) -> Self {
        self.process_env_store = process_env_store;
        self
    }

    pub(crate) fn execution_env_spec(
        mut self,
        execution_env_spec: crate::ProcessExecutionEnvSpec,
    ) -> Self {
        self.execution_env_spec = execution_env_spec;
        self
    }

    pub(crate) fn shared_session_host(mut self, host: Arc<MockSessionManager>) -> Self {
        self.session_host_mode = TestSessionHostMode::Shared(host);
        self
    }

    pub(crate) fn session_lifecycle(
        mut self,
        session_lifecycle: Arc<dyn crate::plugin::SessionLifecycleService>,
    ) -> Self {
        self.session_lifecycle = Some(session_lifecycle);
        self
    }

    pub(crate) fn shared_effect_controller(
        mut self,
        effect_controller: Arc<dyn crate::RuntimeEffectController>,
    ) -> Self {
        self.effect_controller = TestEffectController::Shared(effect_controller);
        self
    }

    pub(crate) fn borrowed_effect_controller(
        mut self,
        effect_controller: crate::ScopedEffectController<'run>,
    ) -> Self {
        self.effect_controller = TestEffectController::Borrowed(effect_controller);
        self
    }

    pub(crate) fn dispatch_parent_invocation(
        mut self,
        parent_invocation: crate::RuntimeInvocation,
    ) -> Self {
        self.dispatch_parent_invocation = Some(parent_invocation);
        self
    }

    pub(crate) fn runtime_parent_invocation(
        mut self,
        parent_invocation: crate::RuntimeInvocation,
    ) -> Self {
        self.runtime_parent_invocation = Some(parent_invocation);
        self
    }

    pub(crate) fn clock(mut self, clock: Arc<dyn crate::Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub(crate) fn build(self) -> BuiltTestExecutionContext<'run> {
        let plugins = crate::plugin::PluginHost::new(test_code_protocol_factories())
            .build_session(&self.session_id, None)
            .expect("test plugin session");
        let (sessions, session_lifecycle, session_graph): (
            Arc<dyn crate::plugin::SessionStateService>,
            Arc<dyn crate::plugin::SessionLifecycleService>,
            Arc<dyn crate::plugin::SessionGraphService>,
        ) = match self.session_host_mode {
            TestSessionHostMode::Independent => {
                let sessions: Arc<dyn crate::plugin::SessionStateService> =
                    Arc::new(MockSessionManager::default());
                let session_lifecycle = self.session_lifecycle.unwrap_or_else(|| {
                    let lifecycle: Arc<dyn crate::plugin::SessionLifecycleService> =
                        Arc::new(MockSessionManager::default());
                    lifecycle
                });
                let session_graph: Arc<dyn crate::plugin::SessionGraphService> =
                    Arc::new(MockSessionManager::default());
                (sessions, session_lifecycle, session_graph)
            }
            TestSessionHostMode::Shared(host) => {
                let sessions: Arc<dyn crate::plugin::SessionStateService> = host.clone();
                let session_lifecycle = self.session_lifecycle.unwrap_or_else(|| {
                    let lifecycle: Arc<dyn crate::plugin::SessionLifecycleService> = host.clone();
                    lifecycle
                });
                let session_graph: Arc<dyn crate::plugin::SessionGraphService> = host;
                (sessions, session_lifecycle, session_graph)
            }
        };
        let effect_controller = match self.effect_controller {
            TestEffectController::Shared(effect_controller) => {
                crate::runtime::RuntimeEffectControllerHandle::shared(effect_controller)
            }
            TestEffectController::Borrowed(effect_controller) => {
                crate::runtime::RuntimeEffectControllerHandle::borrowed(effect_controller)
            }
        };
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(1);
        let dispatch = Arc::new(crate::tool_dispatch::ToolDispatchContext {
            plugins,
            tools: self.provider,
            tool_registry: None,
            tool_catalog: Arc::new(self.tool_catalog),
            sessions,
            session_lifecycle,
            session_graph,
            processes: self.processes,
            trigger_router: self.trigger_router,
            effect_controller,
            direct_completions: crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
            parent_invocation: self.dispatch_parent_invocation,
            execution_env_spec: self.execution_env_spec.clone(),
            session_id: self.session_id,
            agent_frame_id: String::new(),
            event_tx,
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            recorded_intent_outcomes:
                crate::tool_dispatch::RecordedToolIntentOutcomeBuffer::default(),
            attachment_store: Arc::clone(&self.attachment_store),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: self.clock,
        });
        BuiltTestExecutionContext {
            dispatch,
            process_env_store: self.process_env_store,
            execution_env_spec: self.execution_env_spec,
            runtime_parent_invocation: self.runtime_parent_invocation,
        }
    }
}

impl<'run> BuiltTestExecutionContext<'run> {
    pub(crate) fn into_runtime(self) -> crate::RuntimeExecutionContext<'run> {
        let attachment_store = Arc::clone(&self.dispatch.attachment_store);
        let session_id = self.dispatch.session_id.clone();
        let mut context = crate::RuntimeExecutionContext::new(
            session_id,
            self.dispatch,
            self.process_env_store,
            attachment_store,
            Arc::new(crate::ChronologicalProjection::default()),
            None,
            crate::TurnContext::default(),
        )
        .with_execution_env_spec(self.execution_env_spec);
        if let Some(parent_invocation) = self.runtime_parent_invocation {
            context = context.with_parent_invocation(parent_invocation);
        }
        context
    }
}
