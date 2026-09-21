use crate::SessionId;
use std::sync::Arc;

use super::{EmptyToolProvider, MockSessionManager, test_code_protocol_factories};

pub enum TestSessionHostMode {
    Independent,
    Shared(Arc<MockSessionManager>),
}

pub enum TestEffectController<'run> {
    Shared(Arc<dyn crate::RuntimeEffectController>),
    Borrowed(crate::ScopedEffectController<'run>),
}

pub struct TestExecutionContextBuilder<'run> {
    session_id: SessionId,
    provider: Arc<dyn crate::ToolProvider>,
    tool_catalog: crate::ToolCatalog,
    tool_registry: Option<Arc<crate::ToolRegistry>>,
    trigger_router: Option<crate::TriggerRouter>,
    processes: Arc<dyn crate::ProcessService>,
    process_definitions: Option<Arc<dyn crate::ProcessDefinitionRegistry>>,
    process_engines: crate::ProcessEngineRegistry,
    direct_completions: Option<crate::DirectCompletionClient<'run>>,
    process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    execution_env_spec: crate::ProcessExecutionEnvSpec,
    session_host_mode: TestSessionHostMode,
    session_lifecycle: Option<Arc<dyn crate::plugin::SessionLifecycleService>>,
    effect_controller: TestEffectController<'run>,
    dispatch_parent_invocation: Option<crate::RuntimeInvocation>,
    runtime_parent_invocation: Option<crate::RuntimeInvocation>,
    /// Which cell of the turn this context executes.
    ///
    /// Production separates one cell from the next by the turn driver's
    /// protocol iteration, which is part of every cell's effect replay key
    /// (`runtime::causal::turn_effect_replay_key`). A fixture cannot invent
    /// that number without asserting a shape production cannot produce, so it
    /// states it: a fixture that runs two cells passes 0 and then 1, and a
    /// fixture that redrives one cell reuses the invocation through
    /// [`TestExecutionContextBuilder::runtime_parent_invocation`] instead.
    protocol_iteration: usize,
    attachment_store: Arc<crate::SessionAttachmentStore>,
    clock: Arc<dyn crate::Clock>,
    /// Protocol factories the session is built from. `None` takes the code
    /// protocol these contexts default to; lash-core's own `cfg(test)` binary
    /// already carries a builtin protocol factory and passes an empty vec so
    /// the two do not both claim the protocol-session capability.
    plugin_factories: Option<Vec<Arc<dyn crate::plugin::PluginFactory>>>,
}

pub struct BuiltTestExecutionContext<'run> {
    pub dispatch: Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
    pub process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    pub execution_env_spec: crate::ProcessExecutionEnvSpec,
    pub runtime_parent_invocation: Option<crate::RuntimeInvocation>,
    /// Which cell of the turn this context executes; see
    /// [`TestExecutionContextBuilder::protocol_iteration`].
    pub protocol_iteration: usize,
}

impl<'run> Default for TestExecutionContextBuilder<'run> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'run> TestExecutionContextBuilder<'run> {
    pub fn new() -> Self {
        Self {
            session_id: SessionId::from("test-session"),
            provider: Arc::new(EmptyToolProvider),
            tool_catalog: crate::ToolCatalog::from_tool_definitions(Vec::new()),
            tool_registry: None,
            trigger_router: None,
            processes: Arc::new(crate::UnavailableProcessService),
            process_definitions: None,
            process_engines: crate::ProcessEngineRegistry::default(),
            direct_completions: None,
            process_env_store: Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            ),
            session_host_mode: TestSessionHostMode::Independent,
            session_lifecycle: None,
            effect_controller: TestEffectController::Shared(Arc::new(
                crate::NativeRuntimeEffectController::default()
                    .allow_process_lifetime_completion_keys(),
            )),
            dispatch_parent_invocation: None,
            runtime_parent_invocation: None,
            protocol_iteration: 0,
            attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
            clock: Arc::new(crate::SystemClock),
            plugin_factories: None,
        }
    }

    pub fn session_id(mut self, session_id: impl Into<SessionId>) -> Self {
        self.session_id = session_id.into();
        self
    }

    pub fn provider(mut self, provider: Arc<dyn crate::ToolProvider>) -> Self {
        self.provider = provider;
        self
    }

    pub fn tool_catalog(mut self, tool_catalog: crate::ToolCatalog) -> Self {
        self.tool_catalog = tool_catalog;
        self
    }

    pub fn tool_registry(mut self, tool_registry: Arc<crate::ToolRegistry>) -> Self {
        self.tool_registry = Some(tool_registry);
        self
    }

    pub fn trigger_router(mut self, trigger_router: Option<crate::TriggerRouter>) -> Self {
        self.trigger_router = trigger_router;
        self
    }

    pub fn processes(mut self, processes: Arc<dyn crate::ProcessService>) -> Self {
        self.processes = processes;
        self
    }

    pub fn process_definitions(
        mut self,
        process_definitions: Arc<dyn crate::ProcessDefinitionRegistry>,
    ) -> Self {
        self.process_definitions = Some(process_definitions);
        self
    }

    pub fn process_engines(mut self, process_engines: crate::ProcessEngineRegistry) -> Self {
        self.process_engines = process_engines;
        self
    }

    pub fn direct_completions(
        mut self,
        direct_completions: crate::DirectCompletionClient<'run>,
    ) -> Self {
        self.direct_completions = Some(direct_completions);
        self
    }

    pub fn process_env_store(
        mut self,
        process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    ) -> Self {
        self.process_env_store = process_env_store;
        self
    }

    pub fn execution_env_spec(
        mut self,
        execution_env_spec: crate::ProcessExecutionEnvSpec,
    ) -> Self {
        self.execution_env_spec = execution_env_spec;
        self
    }

    pub fn shared_session_host(mut self, host: Arc<MockSessionManager>) -> Self {
        self.session_host_mode = TestSessionHostMode::Shared(host);
        self
    }

    pub fn session_lifecycle(
        mut self,
        session_lifecycle: Arc<dyn crate::plugin::SessionLifecycleService>,
    ) -> Self {
        self.session_lifecycle = Some(session_lifecycle);
        self
    }

    pub fn shared_effect_controller(
        mut self,
        effect_controller: Arc<dyn crate::RuntimeEffectController>,
    ) -> Self {
        self.effect_controller = TestEffectController::Shared(effect_controller);
        self
    }

    pub fn borrowed_effect_controller(
        mut self,
        effect_controller: crate::ScopedEffectController<'run>,
    ) -> Self {
        self.effect_controller = TestEffectController::Borrowed(effect_controller);
        self
    }

    pub fn dispatch_parent_invocation(
        mut self,
        parent_invocation: crate::RuntimeInvocation,
    ) -> Self {
        self.dispatch_parent_invocation = Some(parent_invocation);
        self
    }

    pub fn runtime_parent_invocation(
        mut self,
        parent_invocation: crate::RuntimeInvocation,
    ) -> Self {
        self.runtime_parent_invocation = Some(parent_invocation);
        self
    }

    /// Which cell of the turn this context executes, counting from 0.
    ///
    /// Only a fixture that executes more than one cell needs to say.
    pub fn protocol_iteration(mut self, protocol_iteration: usize) -> Self {
        self.protocol_iteration = protocol_iteration;
        self
    }

    /// Overrides the factories the context's plugin session is built from.
    ///
    /// A caller outside lash-core's own `cfg(test)` binary — conformance laws,
    /// say — must pass `test_code_protocol_factories()` plus its additions so
    /// the session still carries a protocol. Inside that binary the builtin
    /// factory list already carries one, so passing another would double-claim
    /// the protocol-session capability.
    #[cfg(any(test, feature = "testing"))]
    pub fn plugin_factories(
        mut self,
        factories: Vec<Arc<dyn crate::plugin::PluginFactory>>,
    ) -> Self {
        self.plugin_factories = Some(factories);
        self
    }

    pub fn clock(mut self, clock: Arc<dyn crate::Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn build(self) -> BuiltTestExecutionContext<'run> {
        let plugins = crate::plugin::PluginHost::new(
            self.plugin_factories
                .unwrap_or_else(test_code_protocol_factories),
        )
        .build_session(&self.session_id)
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
                // The admitted pair must match the scope the installed parent
                // invocation claims: the fixture models the turn driver, which
                // scopes its controller to the same scope the code-execution
                // effect runs under, and `HostBridge` refuses a claim/opener
                // disagreement rather than re-pairing the two halves itself.
                // A fixture that installs an invocation naming a scope the
                // unpinned constructors cannot admit (a process scope needs
                // its incarnation) must hand the builder an already-scoped
                // `Borrowed` controller instead.
                let admitted = self
                    .runtime_parent_invocation
                    .as_ref()
                    .and_then(crate::RuntimeInvocation::effect_address)
                    .and_then(|address| {
                        crate::AdmittedScope::unpinned(address.execution_scope.clone()).ok()
                    })
                    .unwrap_or_else(|| {
                        crate::AdmittedScope::turn(
                            self.session_id.clone(),
                            crate::TurnId::from("test-turn"),
                        )
                    });
                crate::runtime::RuntimeEffectControllerHandle::Shared {
                    controller: effect_controller,
                    admitted,
                }
            }
            TestEffectController::Borrowed(effect_controller) => {
                crate::runtime::RuntimeEffectControllerHandle::borrowed(effect_controller)
            }
        };
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(1);
        let dispatch = Arc::new(crate::tool_dispatch::ToolDispatchContext {
            probe_private_field: (),
            process_definitions: self.process_definitions,
            process_engines: self.process_engines,
            plugins,
            tools: self.provider,
            tool_registry: self.tool_registry,
            tool_catalog: Arc::new(self.tool_catalog),
            sessions,
            session_lifecycle,
            session_graph,
            processes: self.processes,
            trigger_router: self.trigger_router,
            effect_controller,
            direct_completions: self.direct_completions.unwrap_or_else(|| {
                crate::DirectCompletionClient::unavailable(
                    "direct completions are unavailable in this test context",
                )
            }),
            parent_invocation: self.dispatch_parent_invocation,
            execution_env_spec: self.execution_env_spec.clone(),
            session_id: self.session_id,
            agent_frame_id: crate::FrameNodeId::new("test-frame")
                .expect("test frame identity is non-empty"),
            event_tx,
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
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
            protocol_iteration: self.protocol_iteration,
        }
    }
}

impl<'run> BuiltTestExecutionContext<'run> {
    pub fn into_runtime(self) -> crate::RuntimeExecutionContext<'run> {
        let attachment_store = Arc::clone(&self.dispatch.attachment_store);
        let session_id = self.dispatch.session_id.clone();
        let mut context = crate::RuntimeExecutionContext::new(
            session_id.clone(),
            self.dispatch,
            self.process_env_store,
            attachment_store,
            Arc::new(crate::ChronologicalProjection::default()),
            None,
            crate::TurnContext::default(),
        )
        .with_execution_env_spec(self.execution_env_spec);
        let parent_invocation = self
            .runtime_parent_invocation
            .unwrap_or_else(|| code_execution_invocation(&session_id, self.protocol_iteration));
        context = context.with_parent_invocation(parent_invocation);
        context
    }
}

/// The code-execution effect the turn driver would have installed for this
/// cell.
///
/// Production builds every cell's invocation through this same function
/// (`turn_driver/lease.rs::turn_effect_invocation` →
/// `runtime::causal::turn_effect_invocation`), so a fixture gets its cell key
/// the way production mints one rather than by spelling a string. The protocol
/// iteration is the only part a fixture supplies, because it is the only part
/// that says *which* cell this is.
fn code_execution_invocation(
    session_id: &SessionId,
    protocol_iteration: usize,
) -> crate::RuntimeInvocation {
    let turn_id = crate::TurnId::from("test-turn");
    crate::runtime::causal::turn_effect_invocation(
        &crate::ExecutionScope::turn(session_id.clone(), turn_id.clone()),
        session_id,
        &turn_id,
        0,
        protocol_iteration,
        crate::sansio::EffectId(protocol_iteration as u64),
        crate::RuntimeEffectKind::ExecCode,
    )
    .into_runtime_invocation()
}
