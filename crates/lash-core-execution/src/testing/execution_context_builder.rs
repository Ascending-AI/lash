use crate::SessionId;
use std::sync::Arc;

use super::{EmptyToolProvider, MockSessionManager};

pub enum TestSessionHostMode {
    Independent,
    Shared(Arc<MockSessionManager>),
}

pub enum TestEffectController<'run> {
    Shared(Arc<dyn crate::RuntimeEffectController>),
    Borrowed(crate::ScopedEffectController<'run>),
}

impl From<Arc<dyn crate::RuntimeEffectController>> for TestEffectController<'_> {
    fn from(controller: Arc<dyn crate::RuntimeEffectController>) -> Self {
        Self::Shared(controller)
    }
}

impl<'run> From<crate::ScopedEffectController<'run>> for TestEffectController<'run> {
    fn from(controller: crate::ScopedEffectController<'run>) -> Self {
        Self::Borrowed(controller)
    }
}

/// The ports an execution context runs over: the effect host that journals
/// its effects, the store its process executions publish environments to, the
/// attachment backend its tool bodies write through, and the clock it stamps
/// from.
///
/// There is no in-memory default (ADR 0102). A fixture that has a backend
/// takes [`TestExecutionPorts::of`]; a conformance tier that proves a host
/// without one names each port it runs the law over.
#[derive(Clone)]
pub struct TestExecutionPorts {
    pub effect_host: Arc<dyn crate::EffectHost>,
    pub process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    pub attachment_store: Arc<dyn crate::AttachmentStore>,
    pub clock: Arc<dyn crate::Clock>,
}

impl TestExecutionPorts {
    /// Every port from one backend, on its clock.
    pub fn of(backend: &dyn crate::Backend) -> Self {
        Self::from(backend)
    }

    /// Ports over a tier's bare host, for a conformance law that proves a
    /// host rather than a backend: the process-exec-env store the tier
    /// supplies beside it, no attachment port (puts are refused), and the
    /// system clock.
    pub fn over_host(
        effect_host: Arc<dyn crate::EffectHost>,
        process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    ) -> Self {
        Self {
            effect_host,
            process_env_store,
            attachment_store: Arc::new(crate::attachments::UnavailableAttachmentStore),
            clock: Arc::new(crate::SystemClock),
        }
    }
}

impl<D: crate::Backend + ?Sized> From<&D> for TestExecutionPorts {
    fn from(backend: &D) -> Self {
        Self {
            effect_host: backend.effect_host(),
            process_env_store: backend.process_env_store(),
            attachment_store: backend.attachment_store(),
            clock: backend.clock(),
        }
    }
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
    turn_context: crate::TurnContext,
    session_host_mode: TestSessionHostMode,
    session_lifecycle: Option<Arc<dyn crate::plugin::SessionLifecycleService>>,
    /// The host the context's effects run on. Unless `effect_controller`
    /// overrides it, its controller, scoped to the context's admitted scope,
    /// serves them and its tool-child host routes the context's group
    /// children (ADR 0099 §2/§3). `None` for a context built
    /// [`over_controller`](TestExecutionContextBuilder::over_controller).
    effect_host: Option<Arc<dyn crate::EffectHost>>,
    /// A controller that serves the context's effects in place of the host's
    /// own: a fixture that already holds a scoped controller over the host's
    /// journal, or a foreign one it is proving.
    effect_controller: Option<TestEffectController<'run>>,
    /// Whether the host routes the context's group children. Always when the
    /// host's own controller serves the context; opt-in beside an override
    /// controller, through [`TestExecutionContextBuilder::route_tool_children`].
    route_tool_children: bool,
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
    pub turn_context: crate::TurnContext,
    pub runtime_parent_invocation: Option<crate::RuntimeInvocation>,
    /// Which cell of the turn this context executes; see
    /// [`TestExecutionContextBuilder::protocol_iteration`].
    pub protocol_iteration: usize,
    /// The live-opener registration this context's tool children route
    /// through; kept here so [`into_runtime`](Self::into_runtime) can pin it
    /// to the context's lifetime.
    tool_child_guard: Option<crate::runtime::effect::LiveOpenerGuard>,
    /// The issuer identity a group child records on a `ProcessLifetime`
    /// completion route (ADR 0099 §14): the host's binding id, when the
    /// context was built against a host.
    tool_child_completion_issuer: Option<crate::TurnControlBindingId>,
    /// The host the tool-child resolver holds only weakly; pinned to the
    /// context's lifetime by [`into_runtime`](Self::into_runtime).
    tool_child_host: Option<Arc<dyn crate::EffectHost>>,
}

impl<'run> TestExecutionContextBuilder<'run> {
    /// A builder over `ports`: the host's own controller serves the context's
    /// effects and its tool-child host routes the context's group children.
    pub fn new(ports: TestExecutionPorts) -> Self {
        let TestExecutionPorts {
            effect_host,
            process_env_store,
            attachment_store,
            clock,
        } = ports;
        Self::assemble(
            Some(effect_host),
            None,
            process_env_store,
            attachment_store,
            clock,
        )
    }

    fn assemble(
        effect_host: Option<Arc<dyn crate::EffectHost>>,
        effect_controller: Option<TestEffectController<'run>>,
        process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
        attachment_store: Arc<dyn crate::AttachmentStore>,
        clock: Arc<dyn crate::Clock>,
    ) -> Self {
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
            process_env_store,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            ),
            turn_context: crate::TurnContext::default(),
            session_host_mode: TestSessionHostMode::Independent,
            session_lifecycle: None,
            route_tool_children: effect_host.is_some(),
            effect_host,
            effect_controller,
            dispatch_parent_invocation: None,
            runtime_parent_invocation: None,
            protocol_iteration: 0,
            attachment_store: Arc::new(crate::SessionAttachmentStore::ephemeral(attachment_store)),
            clock,
            plugin_factories: None,
        }
    }

    /// A builder over every port of `backend`.
    pub fn for_backend(backend: &dyn crate::Backend) -> Self {
        Self::new(TestExecutionPorts::of(backend))
    }

    /// A builder with no host: `effect_controller` serves the context's
    /// effects, no group children are routed, and the context has no
    /// process-exec-env store and no attachment port (both refuse). For a
    /// test of the context's own logic over a fake or recording controller;
    /// a test that journals, publishes environments or stores attachments
    /// builds its context over a backend.
    pub fn over_controller(effect_controller: impl Into<TestEffectController<'run>>) -> Self {
        Self::assemble(
            None,
            Some(effect_controller.into()),
            Arc::new(super::UnavailableProcessExecutionEnvStore),
            Arc::new(crate::attachments::UnavailableAttachmentStore),
            Arc::new(crate::SystemClock),
        )
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

    pub fn execution_env_spec(
        mut self,
        execution_env_spec: crate::ProcessExecutionEnvSpec,
    ) -> Self {
        self.execution_env_spec = execution_env_spec;
        self
    }

    pub fn turn_context(mut self, turn_context: crate::TurnContext) -> Self {
        self.turn_context = turn_context;
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

    /// Serves the context's effects through `effect_controller`, admitted
    /// under the context's scope, instead of the host's own controller. The
    /// host then routes no group children unless
    /// [`route_tool_children`](Self::route_tool_children) says so.
    pub fn shared_effect_controller(
        mut self,
        effect_controller: Arc<dyn crate::RuntimeEffectController>,
    ) -> Self {
        self.effect_controller = Some(TestEffectController::Shared(effect_controller));
        self.route_tool_children = false;
        self
    }

    /// Serves the context's effects through an already scoped controller
    /// instead of the host's own. The host then routes no group children
    /// unless [`route_tool_children`](Self::route_tool_children) says so.
    pub fn borrowed_effect_controller(
        mut self,
        effect_controller: crate::ScopedEffectController<'run>,
    ) -> Self {
        self.effect_controller = Some(TestEffectController::Borrowed(effect_controller));
        self.route_tool_children = false;
        self
    }

    /// Routes the context's group children through the ports' host beside an
    /// override controller (ADR 0099 §2/§3): the tool-child host is installed
    /// on it, and the context's opener is registered in its live-opener
    /// registry for the context's lifetime. A conformance or differential
    /// fixture running `call_tool_batch` against a tier's host calls this
    /// after handing in the controller it scoped from that host.
    pub fn route_tool_children(mut self) -> Self {
        self.route_tool_children = true;
        self
    }

    pub fn dispatch_parent_invocation(
        mut self,
        parent_invocation: crate::RuntimeInvocation,
    ) -> Self {
        self.dispatch_parent_invocation = Some(parent_invocation);
        self
    }

    /// Installs the parent invocation. The context attributes its work to
    /// the session that invocation's scope names, so the scope it admits and
    /// the session it attributes to are one fact (a group child's retained
    /// request refuses the two disagreeing).
    pub fn runtime_parent_invocation(
        mut self,
        parent_invocation: crate::RuntimeInvocation,
    ) -> Self {
        if let Some(session_id) = parent_invocation
            .effect_address()
            .and_then(|address| address.execution_scope.session_id())
        {
            self.session_id = session_id.clone();
        }
        self.runtime_parent_invocation = Some(parent_invocation);
        self
    }

    /// Overrides the session attachment facade the context binds.
    ///
    /// The default is an ephemeral facade over the builder's attachment port;
    /// a law that must observe or share the facade a presentation step retains
    /// artifacts through (FIG-3420) hands in its own.
    pub fn attachment_store(
        mut self,
        attachment_store: Arc<crate::SessionAttachmentStore>,
    ) -> Self {
        self.attachment_store = attachment_store;
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
                .unwrap_or_else(default_plugin_factories),
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
        // The scope the context's controller admits: the scope the installed
        // parent invocation claims, else the default test turn. A fixture that
        // installs an invocation naming a scope the unpinned constructors
        // cannot admit (a process scope needs its incarnation) must hand the
        // builder an already-scoped `Borrowed` controller instead.
        let default_admitted = || {
            self.runtime_parent_invocation
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
                })
        };
        let effect_host = self.effect_host;
        // The admitted pair must match the scope the installed parent
        // invocation claims: the fixture models the turn driver, which scopes
        // its controller to the same scope the code-execution effect runs
        // under, and `HostBridge` refuses a claim/opener disagreement rather
        // than re-pairing the two halves itself.
        let effect_controller = match self.effect_controller {
            None => crate::runtime::RuntimeEffectControllerHandle::borrowed(
                effect_host
                    .as_ref()
                    .expect("a builder with no host is built over a controller")
                    .scoped_static(default_admitted())
                    .expect("the supplied host binds the fixture's admitted scope")
                    .expect("the supplied host lends a static controller"),
            ),
            Some(TestEffectController::Shared(effect_controller)) => {
                crate::runtime::RuntimeEffectControllerHandle::Shared {
                    controller: effect_controller,
                    admitted: default_admitted(),
                }
            }
            Some(TestEffectController::Borrowed(effect_controller)) => {
                crate::runtime::RuntimeEffectControllerHandle::borrowed(effect_controller)
            }
        };
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(1);
        let dispatch = Arc::new(crate::tool_dispatch::ToolDispatchContext {
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
            turn_activity_tx: None,
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::clone(&self.attachment_store),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: self.turn_context.clone(),
            clock: self.clock,
        });

        let tool_child_host = if self.route_tool_children {
            Some(effect_host.expect("tool children are routed only through the builder's host"))
        } else {
            None
        };
        let (tool_child_guard, tool_child_completion_issuer) = tool_child_host
            .as_ref()
            .and_then(|host| wire_test_tool_children(&dispatch, &self.process_env_store, host))
            .map(|(guard, issuer)| (Some(guard), issuer))
            .unwrap_or((None, None));

        BuiltTestExecutionContext {
            dispatch,
            process_env_store: self.process_env_store,
            execution_env_spec: self.execution_env_spec,
            turn_context: self.turn_context,
            runtime_parent_invocation: self.runtime_parent_invocation,
            protocol_iteration: self.protocol_iteration,
            tool_child_guard,
            tool_child_completion_issuer,
            tool_child_host,
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
            self.turn_context,
        )
        .with_execution_env_spec(self.execution_env_spec);
        let parent_invocation = self
            .runtime_parent_invocation
            .unwrap_or_else(|| code_execution_invocation(&session_id, self.protocol_iteration));
        context = context.with_parent_invocation(parent_invocation);
        if let Some(issuer) = self.tool_child_completion_issuer {
            context = context.with_tool_child_completion_issuer(issuer);
        }
        if let Some(guard) = self.tool_child_guard {
            context = context.with_live_opener_guard(Arc::new(guard));
        }
        if let Some(host) = self.tool_child_host {
            context = context.with_tool_child_host(host);
        }
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

/// Installs tool-child routing for a context whose `ToolDispatchContext` was
/// assembled outside `TestExecutionContextBuilder` (ADR 0099 §2/§3): a
/// `ToolChildHost` is installed on `host` and the context's opener is
/// registered in the host's live-opener registry so `context_for` answers for
/// its children.
///
/// Returns the live-opener guard (pin it to the context with
/// `RuntimeExecutionContext::with_live_opener_guard`) and the host's
/// turn-control binding id for `with_tool_child_completion_issuer`.
///
/// The wiring is deliberately best-effort, mirroring production's
/// registration sites: a controller that accepts no group-executor resolver,
/// a scope that names no opener, or a host that lends no `'static` controller
/// each mean this context routes no tool children — and a group opened anyway
/// fails closed at `open_effect_group`.
pub fn wire_test_tool_children(
    dispatch: &Arc<crate::tool_dispatch::ToolDispatchContext<'_>>,
    process_env_store: &Arc<dyn crate::ProcessExecutionEnvStore>,
    host: &Arc<dyn crate::EffectHost>,
) -> Option<(
    crate::runtime::effect::LiveOpenerGuard,
    Option<crate::TurnControlBindingId>,
)> {
    let tool_children = host.install_tool_child_host(
        crate::runtime::effect::ToolChildHost::new(host, Arc::clone(process_env_store))
            .with_clock(Arc::clone(&dispatch.clock)),
    )?;
    let admitted = dispatch.effect_controller.scoped().admitted_scope().clone();
    let opener = crate::runtime::effect::opener_for_execution_scope(&admitted)?;
    let lent = host.scoped_static(admitted).ok()??;
    let (guard, _ended) = tool_children.openers().register(
        opener,
        crate::runtime::effect::LiveOpenerContext::capture(
            dispatch.as_ref(),
            lent,
            tokio_util::sync::CancellationToken::new(),
        ),
    );
    let issuer = crate::TurnControlBindingId::new(host.turn_control_binding_id()).ok();
    Some((guard, issuer))
}

/// The protocol factories a built context needs: under `cfg(test)` the
/// builtin factory list already carries `test_protocol`, so adding the
/// code-protocol fake would double-claim the protocol-session capability;
/// under `feature = "testing"` (an external crate) nothing builtin provides
/// one, so the code-protocol fake is required.
fn default_plugin_factories() -> Vec<Arc<dyn crate::plugin::PluginFactory>> {
    #[cfg(test)]
    {
        Vec::new()
    }
    #[cfg(not(test))]
    {
        super::test_code_protocol_factories()
    }
}
