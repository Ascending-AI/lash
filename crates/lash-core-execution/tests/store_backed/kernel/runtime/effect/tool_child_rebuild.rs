//! A group tool child whose opener is not live where it runs builds its
//! context from the deployment's source (FIG-3712), driven end to end through
//! a group on the Restate server double.

mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use crate::runtime::effect::*;
    use crate::runtime::{ToolChildAdmission, ToolChildCompletionRouting, ToolChildScope};
    use crate::tool_dispatch::{ToolAttemptEffectIdentity, ToolDispatchContext};
    use crate::{
        EffectHost, ExecutionScope, FrameNodeId, PreparedToolCall, ProcessExecutionEnvSpec,
        RuntimeEffectCommand, SessionId, StoreSet, ToolId, ToolManifest, ToolRetryPolicy,
    };

    const SESSION: &str = "child-session";
    const TURN: &str = "turn";
    const TOOL: &str = "search";
    const SEED: u64 = 0xcc_b1d;

    fn scope() -> ExecutionScope {
        ExecutionScope::turn(SESSION, TURN)
    }

    fn manifest(retry_policy: ToolRetryPolicy) -> ToolManifest {
        let mut manifest = crate::ToolDefinition::raw(
            TOOL,
            TOOL,
            "a rebuilt-context fixture tool",
            crate::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "object", "additionalProperties": true }),
        )
        .manifest;
        manifest.retry_policy = retry_policy;
        manifest
    }

    /// What a context runs the child's tool with: it counts executions, and
    /// reads the session when told to.
    #[derive(Clone, Copy)]
    enum Behavior {
        Answer,
        ReadSession,
    }

    struct CountingTools {
        executions: Arc<AtomicUsize>,
        behavior: Behavior,
    }

    #[async_trait::async_trait]
    impl crate::ToolProvider for CountingTools {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            vec![manifest(ToolRetryPolicy::Never)]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            (name == TOOL).then(|| Arc::new(crate::ToolContract::default()))
        }

        async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
            self.executions.fetch_add(1, Ordering::SeqCst);
            match self.behavior {
                Behavior::Answer => crate::ToolOutcome::ok(serde_json::json!("answered")).into(),
                Behavior::ReadSession => match call.context.sessions().snapshot_current().await {
                    Ok(_) => crate::ToolOutcome::ok(serde_json::json!("read")).into(),
                    Err(error) => crate::ToolOutcome::err_fmt(error).into(),
                },
            }
        }
    }

    /// A context for the child: the opener's when lent, the deployment's when
    /// a source builds it. Only its tools matter here.
    fn context(tools: Arc<CountingTools>) -> ToolDispatchContext<'static> {
        ToolDispatchContext {
            plugins: crate::support::plugin_host(Vec::new())
                .build_session(SESSION)
                .expect("plugin session"),
            tools,
            tool_registry: None,
            tool_catalog: Arc::new(crate::ToolCatalog::default()),
            sessions: Arc::new(crate::testing::MockSessionManager::default()),
            session_lifecycle: Arc::new(crate::testing::MockSessionManager::default()),
            session_graph: Arc::new(crate::testing::MockSessionManager::default()),
            processes: Arc::new(crate::UnavailableProcessService),
            trigger_router: None,
            process_definitions: None,
            process_engines: crate::ProcessEngineRegistry::default(),
            effect_controller: crate::runtime::RuntimeEffectControllerHandle::shared(Arc::new(
                crate::testing::UnavailableEffectController,
            )),
            direct_completions: crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
            parent_invocation: None,
            observation_call_key: None,
            execution_env_spec: spec(),
            session_id: SessionId::from(SESSION),
            agent_frame_id: FrameNodeId::new("frame").expect("a valid frame id"),
            observer: crate::engine::NullObservationSink::arc(),
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::new(crate::SessionAttachmentStore::unavailable()),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: Arc::new(crate::SystemClock),
            process_lineage: None,
            process_originator: None,
        }
    }

    fn spec() -> ProcessExecutionEnvSpec {
        ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::bounded(3)),
        )
    }

    fn tools(behavior: Behavior) -> Arc<CountingTools> {
        Arc::new(CountingTools {
            executions: Arc::new(AtomicUsize::new(0)),
            behavior,
        })
    }

    /// A deployment source that builds the child's context around `tools`
    /// and counts its builds.
    struct FixedSource {
        tools: Arc<CountingTools>,
        builds: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ToolChildContextSource for FixedSource {
        async fn tool_child_context(
            &self,
            _request: &ToolChildRequest,
            _execution_env: &ProcessExecutionEnvSpec,
            lent_controller: crate::ScopedEffectController<'static>,
        ) -> Result<DeploymentToolChildContext, crate::PluginError> {
            self.builds.fetch_add(1, Ordering::SeqCst);
            let mut dispatch = context(Arc::clone(&self.tools));
            dispatch.effect_controller =
                crate::runtime::RuntimeEffectControllerHandle::borrowed(lent_controller);
            Ok(DeploymentToolChildContext::new(dispatch, Arc::new(())))
        }
    }

    fn source(tools: &Arc<CountingTools>) -> (Arc<FixedSource>, Arc<dyn ToolChildContextSource>) {
        let fixed = Arc::new(FixedSource {
            tools: Arc::clone(tools),
            builds: AtomicUsize::new(0),
        });
        let source: Arc<dyn ToolChildContextSource> = fixed.clone();
        (fixed, source)
    }

    /// One worker's view of the backend: its effect host and its tool-child
    /// host, which routes the children it can run.
    struct Worker {
        host: Arc<dyn EffectHost>,
        tool_children: Arc<ToolChildHost>,
    }

    struct Backend {
        double: lash_restate_test::RestateTestBackend,
        env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
        host: Arc<dyn EffectHost>,
    }

    impl Backend {
        async fn new() -> Self {
            let double =
                crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default())
                    .await;
            let env_store: Arc<dyn crate::ProcessExecutionEnvStore> =
                double.lash_backend().process_env_store();
            let host: Arc<dyn EffectHost> = double.lash_backend().effect_host();
            Self {
                double,
                env_store,
                host,
            }
        }

        /// A worker on this backend with a tool-child host of its own.
        fn worker(&self) -> Worker {
            Worker {
                host: Arc::clone(&self.host),
                tool_children: ToolChildHost::new(
                    &self.host,
                    Arc::clone(&self.env_store),
                    self.double.stores().clock(),
                ),
            }
        }

        /// A request admitted durably under this backend's host: the
        /// cancellation binding its host derives for the child's scope, and a
        /// published environment.
        async fn request(&self, retry_policy: ToolRetryPolicy) -> ToolChildRequest {
            let derived = crate::runtime::effect::executor::turn_control_binding_id_for_scope(
                &self.host.turn_control_binding_id(),
                &scope(),
            )
            .expect("a scope-derived binding id");
            let mut request = ToolChildRequest::new(
                PreparedToolCall::from_parts(
                    "call-1",
                    ToolId::from(TOOL),
                    TOOL,
                    serde_json::json!({}),
                    None,
                    serde_json::Value::Null,
                ),
                ToolChildAdmission::Catalog {
                    manifest: Box::new(manifest(retry_policy)),
                },
                ToolAttemptEffectIdentity::Scalar { parent: None },
                ToolChildScope {
                    opener: crate::EffectOpener::turn(SESSION, TURN),
                    admitted_scope: crate::AdmittedScope::turn(SESSION, TURN),
                    session_id: SessionId::from(SESSION),
                    agent_frame_id: FrameNodeId::new("frame").expect("a valid frame id"),
                },
                crate::TurnControlBindingId::new(derived).expect("a valid binding id"),
                crate::ProcessExecutionEnvRef::new("env-ref"),
                ToolChildCompletionRouting::Inline,
                ToolChildSessionFacts {
                    tool_surface: vec![crate::ToolDefinition {
                        manifest: manifest(retry_policy),
                        contract: crate::ToolContract::default(),
                    }],
                    ..Default::default()
                },
            );
            request.execution_env = crate::publish_process_execution_env(
                self.env_store.as_ref(),
                &crate::ReferrerClaim::unguarded(crate::ArtifactReferrer::HostPin(
                    crate::HostArtifactPin::mint(),
                ))
                .expect("host pin claim"),
                &spec(),
            )
            .await
            .expect("the recorded environment publishes");
            request
        }
    }

    fn envelope(request: ToolChildRequest, child: &str) -> crate::RuntimeEffectEnvelope {
        crate::RuntimeEffectEnvelope::new(
            crate::RuntimeEffectInvocation::new(
                crate::EffectAddress::new(scope(), child).expect("a valid effect address"),
                crate::RuntimeAttribution::for_session(SESSION),
                child,
            ),
            RuntimeEffectCommand::ToolInvocation {
                request: Box::new(request),
            },
        )
        .in_effect_group(
            format!("group-{child}"),
            0,
            crate::GroupWakePolicy::All,
            crate::LoserPolicy::Cancel,
        )
    }

    /// Opens `envelope`'s one-child group on the child's own controller, with
    /// `worker`'s tool-child host installed the way production installs it —
    /// `install_tool_child_host`, the one resolver a deployment has — so the
    /// dispatch's own resolution inside the child's invocation answers what
    /// this worker answers.
    async fn open_group(
        backend: &Backend,
        worker: &Worker,
        envelope: crate::RuntimeEffectEnvelope,
    ) -> (
        Arc<dyn crate::RuntimeEffectController>,
        crate::EffectGroupHandle,
    ) {
        crate::GroupExecutors::executor_for(worker.tool_children.as_ref(), &envelope)
            .expect("the worker routes the child");
        backend
            .host
            .install_tool_child_host(Arc::clone(&worker.tool_children))
            .expect("the backend host takes its first tool-child host");
        let controller = backend
            .host
            .scoped_static(crate::AdmittedScope::turn(SESSION, TURN))
            .expect("the backend host admits the scope")
            .expect("the backend host lends a static controller")
            .owned_controller()
            .expect("a static controller is shared");
        let group_key = envelope
            .group
            .as_ref()
            .expect("the child is grouped")
            .group_key
            .clone();
        let handle = crate::RuntimeEffectController::open_effect_group(
            controller.as_ref(),
            crate::RuntimeEffectGroup::try_new(
                crate::RuntimeEffectInvocation::new(
                    crate::EffectAddress::new(scope(), format!("group:{group_key}"))
                        .expect("a valid group address"),
                    crate::RuntimeAttribution::none(),
                    group_key.clone(),
                ),
                group_key,
                vec![envelope],
                crate::GroupWakePolicy::All,
                crate::LoserPolicy::Cancel,
            )
            .expect("the one-child group assembles"),
        )
        .await
        .expect("the group opens and dispatches the resolved runner");
        (controller, handle)
    }

    /// Runs `envelope`'s child in a one-child group on the child's own
    /// controller and returns its settlement outcome.
    async fn run_child(
        backend: &Backend,
        worker: &Worker,
        envelope: crate::RuntimeEffectEnvelope,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let (controller, mut handle) = open_group(backend, worker, envelope).await;
        let settlement = tokio::time::timeout(
            Duration::from_secs(10),
            crate::RuntimeEffectController::await_next_settlement(
                controller.as_ref(),
                &mut handle,
                crate::runtime::TurnCancelWait::unobserved(
                    tokio_util::sync::CancellationToken::new(),
                ),
            ),
        )
        .await
        .expect("the child settles in time")
        .expect("the group settles its one child");
        settlement.outcome
    }

    /// Waits until the child's dispatch invocation reports `needle` as the
    /// live fault it keeps retrying, and returns that failure. A refused
    /// child settles nothing on the double — "the engine keeps it for its
    /// opener" is the invocation retrying its drive — so the typed refusal
    /// is observed in the invocation's `last_failure` rather than an outcome.
    async fn await_child_retry(backend: &Backend, group_key: &str, needle: &str) -> (u32, String) {
        // The dispatch runs on the lane the opener's build recorded: stable
        // `EffectGroupDispatch` or a generation lane `EffectGroupDispatch_g<G>`
        // (FIG-3795), so the observation matches the lane prefix rather than
        // one service name.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            for view in backend.double.server().invocations() {
                let mut segments = view.target.split('/');
                let is_child = segments.next().is_some_and(|service| {
                    service
                        .strip_prefix("EffectGroupDispatch")
                        .is_some_and(|lane| lane.is_empty() || lane.starts_with("_g"))
                }) && segments.next() == Some(group_key)
                    && segments.next() == Some("child");
                if is_child
                    && let Some(failure) = &view.last_failure
                    && failure.1.contains(needle)
                {
                    return failure.clone();
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the child invocation never retried the typed refusal `{needle}`: {:?}",
                backend.double.server().invocations()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Registers `tools` as the opener's live context on `worker`.
    fn go_live(
        worker: &Worker,
        tools: &Arc<CountingTools>,
    ) -> (LiveOpenerGuard, tokio_util::sync::CancellationToken) {
        let lent = context(Arc::clone(tools));
        let controller = worker
            .host
            .scoped_static(crate::AdmittedScope::turn(SESSION, TURN))
            .expect("the backend host admits the scope")
            .expect("the backend host lends a static controller");
        let live = LiveOpenerContext::capture(
            &lent,
            controller,
            tokio_util::sync::CancellationToken::new(),
        );
        worker
            .tool_children
            .openers()
            .register(crate::EffectOpener::turn(SESSION, TURN), live)
    }

    /// With no source installed, a child with no live opener here is not
    /// routed, exactly as before FIG-3712. Installing one routes it; the
    /// source going away stops the routing again rather than leaving a
    /// dangling builder.
    #[tokio::test]
    async fn only_a_live_source_routes_a_child_with_no_live_opener() {
        let backend = Backend::new().await;
        let worker = backend.worker();
        let envelope = envelope(backend.request(ToolRetryPolicy::Never).await, "child");
        assert!(
            crate::GroupExecutors::executor_for(worker.tool_children.as_ref(), &envelope).is_none(),
            "no source: the child is not routed here"
        );
        let built = tools(Behavior::Answer);
        let (_fixed, source) = source(&built);
        assert_eq!(
            worker.tool_children.install_context_source(&source),
            ContextSourceInstall::Sole
        );
        assert!(
            crate::GroupExecutors::executor_for(worker.tool_children.as_ref(), &envelope).is_some(),
            "a live source routes the child"
        );
        drop(source);
        drop(_fixed);
        assert!(
            crate::GroupExecutors::executor_for(worker.tool_children.as_ref(), &envelope).is_none(),
            "a dropped source routes nothing"
        );
    }

    /// Two distinct live sources leave the host ambiguous: a child with no
    /// live opener here is refused, typed and retryable, and neither source
    /// builds anything, so no child runs under whichever deployment was built
    /// last. Installing the same source again is not a second one, and once
    /// the other is dropped the kept child's next retry builds the sole
    /// remaining source and settles.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_live_sources_leave_the_host_ambiguous() {
        let backend = Backend::new().await;
        let worker = backend.worker();
        let first_tools = tools(Behavior::Answer);
        let (first_fixed, first) = source(&first_tools);
        let second_tools = tools(Behavior::Answer);
        let (second_fixed, second) = source(&second_tools);
        assert_eq!(
            worker.tool_children.install_context_source(&first),
            ContextSourceInstall::Sole
        );
        assert_eq!(
            worker.tool_children.install_context_source(&first),
            ContextSourceInstall::Sole,
            "installing the same source again is not a second one"
        );
        assert_eq!(
            worker.tool_children.install_context_source(&second),
            ContextSourceInstall::Ambiguous { live: 2 }
        );

        let child_envelope = envelope(backend.request(ToolRetryPolicy::Never).await, "ambiguous");
        let group_key = child_envelope
            .group
            .as_ref()
            .expect("the child is grouped")
            .group_key
            .clone();
        let (controller, mut handle) = open_group(&backend, &worker, child_envelope).await;
        let (_, failure) = await_child_retry(
            &backend,
            &group_key,
            &ToolChildRebuildRefusal::AmbiguousDeployment.to_string(),
        )
        .await;
        assert!(
            failure.contains("waits for it"),
            "the refusal says why the child waits: {failure}"
        );
        assert_eq!(first_fixed.builds.load(Ordering::SeqCst), 0);
        assert_eq!(second_fixed.builds.load(Ordering::SeqCst), 0);
        assert_eq!(first_tools.executions.load(Ordering::SeqCst), 0);
        assert_eq!(second_tools.executions.load(Ordering::SeqCst), 0);

        // The ambiguity died with the second source: the kept child's next
        // retry sees `first` as the sole live source, builds its context and
        // runs the child the group was holding.
        drop(second);
        drop(second_fixed);
        let settlement = tokio::time::timeout(
            Duration::from_secs(10),
            crate::RuntimeEffectController::await_next_settlement(
                controller.as_ref(),
                &mut handle,
                crate::runtime::TurnCancelWait::unobserved(
                    tokio_util::sync::CancellationToken::new(),
                ),
            ),
        )
        .await
        .expect("the kept child's retry settles once a sole source remains")
        .expect("the group settles its one child");
        settlement
            .outcome
            .expect("the sole source builds the child's context");
        assert_eq!(first_fixed.builds.load(Ordering::SeqCst), 1);
        assert_eq!(first_tools.executions.load(Ordering::SeqCst), 1);
    }

    /// The #2106 cross-worker path: the child lands on a worker where its
    /// opener is not live. Before FIG-3712 that worker answered "not mine";
    /// with a source it builds the context and runs the child there, never
    /// touching the opener's context on the other worker.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_child_on_a_worker_without_its_opener_runs_on_that_workers_built_context() {
        let backend = Backend::new().await;
        let opener_worker = backend.worker();
        let other_worker = backend.worker();
        let opener_tools = tools(Behavior::Answer);
        let _live = go_live(&opener_worker, &opener_tools);
        let built = tools(Behavior::Answer);
        let (fixed, source) = source(&built);
        assert_eq!(
            other_worker.tool_children.install_context_source(&source),
            ContextSourceInstall::Sole
        );
        let envelope = envelope(backend.request(ToolRetryPolicy::Never).await, "child");
        assert!(
            crate::GroupExecutors::routes(opener_worker.tool_children.as_ref(), &envelope)
                && crate::GroupExecutors::routes(other_worker.tool_children.as_ref(), &envelope),
            "every worker routes a tool child"
        );

        let outcome = run_child(&backend, &other_worker, envelope)
            .await
            .expect("the other worker runs the child on its built context");

        assert!(matches!(
            outcome,
            crate::RuntimeEffectOutcome::ToolInvocation { .. }
        ));
        assert_eq!(fixed.builds.load(Ordering::SeqCst), 1);
        assert_eq!(built.executions.load(Ordering::SeqCst), 1);
        assert_eq!(
            opener_tools.executions.load(Ordering::SeqCst),
            0,
            "the opener's context on the other worker is never used"
        );
    }

    /// A child whose opener is a process runs on a built context too: the
    /// source answers for the child's recorded session whatever opened it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_process_openers_child_runs_on_a_built_context() {
        let backend = Backend::new().await;
        let worker = backend.worker();
        let built = tools(Behavior::Answer);
        let (fixed, source) = source(&built);
        assert_eq!(
            worker.tool_children.install_context_source(&source),
            ContextSourceInstall::Sole
        );
        let mut request = backend.request(ToolRetryPolicy::Never).await;
        let opener = crate::ProcessId::fixture("worker");
        request.scope.opener = crate::EffectOpener::process(opener.clone());
        request.enclosing_process = Some(opener);
        request
            .validate()
            .expect("a process opener enclosing itself is a legal request");

        let outcome = run_child(&backend, &worker, envelope(request, "child"))
            .await
            .expect("the child runs on the built context");

        assert!(matches!(
            outcome,
            crate::RuntimeEffectOutcome::ToolInvocation { .. }
        ));
        assert_eq!(fixed.builds.load(Ordering::SeqCst), 1);
        assert_eq!(built.executions.load(Ordering::SeqCst), 1);
    }

    /// A child whose opener's session had a source no deployment can rebuild
    /// is refused on the built path before anything is built or run, with a
    /// typed reason, and the engine keeps it for its opener. The same child
    /// runs where its opener is live.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_child_with_an_unrecorded_session_source_waits_for_its_opener() {
        let sources = [
            (
                UnrecordedSessionSources {
                    context_overlay_tools: true,
                    ..Default::default()
                },
                ToolChildRebuildRefusal::ContextOverlayTools,
            ),
            (
                UnrecordedSessionSources {
                    open_plugins: true,
                    ..Default::default()
                },
                ToolChildRebuildRefusal::OpenPlugins,
            ),
            (
                UnrecordedSessionSources {
                    fork_plugins: true,
                    ..Default::default()
                },
                ToolChildRebuildRefusal::ForkPlugins,
            ),
            (
                UnrecordedSessionSources {
                    open_provider: true,
                    ..Default::default()
                },
                ToolChildRebuildRefusal::OpenProvider,
            ),
            (
                UnrecordedSessionSources {
                    open_tool_policy: true,
                    ..Default::default()
                },
                ToolChildRebuildRefusal::OpenToolPolicy,
            ),
            (
                UnrecordedSessionSources {
                    plugin_state: true,
                    ..Default::default()
                },
                ToolChildRebuildRefusal::PluginState,
            ),
        ];
        for (index, (unrecorded, refusal)) in sources.into_iter().enumerate() {
            let backend = Backend::new().await;
            let worker = backend.worker();
            let built = tools(Behavior::Answer);
            let (fixed, source) = source(&built);
            assert_eq!(
                worker.tool_children.install_context_source(&source),
                ContextSourceInstall::Sole
            );
            let mut request = backend.request(ToolRetryPolicy::Never).await;
            request.session.unrecorded = unrecorded;

            let child = format!("refused-{index}");
            let child_envelope = envelope(request, &child);
            let group_key = child_envelope
                .group
                .as_ref()
                .expect("the child is grouped")
                .group_key
                .clone();
            open_group(&backend, &worker, child_envelope).await;
            let (_, failure) = await_child_retry(&backend, &group_key, &refusal.to_string()).await;
            assert!(
                failure.contains("waits for it"),
                "the refusal says why the child waits: {failure}"
            );
            assert_eq!(fixed.builds.load(Ordering::SeqCst), 0, "nothing is built");
            assert_eq!(built.executions.load(Ordering::SeqCst), 0, "nothing runs");

            // A backend's host answers group routing once, so the live run
            // is on a backend of its own: the same child, its opener live.
            let backend = Backend::new().await;
            let worker = backend.worker();
            let opener_tools = tools(Behavior::Answer);
            let _live = go_live(&worker, &opener_tools);
            let mut request = backend.request(ToolRetryPolicy::Never).await;
            request.session.unrecorded = unrecorded;
            let outcome = run_child(
                &backend,
                &worker,
                envelope(request, &format!("live-{index}")),
            )
            .await
            .expect("the opener's live context runs the same child");
            assert!(matches!(
                outcome,
                crate::RuntimeEffectOutcome::ToolInvocation { .. }
            ));
            assert_eq!(opener_tools.executions.load(Ordering::SeqCst), 1);
        }
    }

    /// A child that reads its session on a built context is abandoned where
    /// it stands and refused with a typed reason: the read never answers, so
    /// the tool never turns a refusal into a result a live opener would not
    /// have produced.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_session_read_on_a_built_context_abandons_the_child() {
        let backend = Backend::new().await;
        let worker = backend.worker();
        let built = tools(Behavior::ReadSession);
        let (fixed, source) = source(&built);
        assert_eq!(
            worker.tool_children.install_context_source(&source),
            ContextSourceInstall::Sole
        );
        let request = backend.request(ToolRetryPolicy::Never).await;

        let child_envelope = envelope(request, "child");
        let group_key = child_envelope
            .group
            .as_ref()
            .expect("the child is grouped")
            .group_key
            .clone();
        open_group(&backend, &worker, child_envelope).await;
        await_child_retry(
            &backend,
            &group_key,
            &ToolChildRebuildRefusal::SessionServices.to_string(),
        )
        .await;
        // The child retries its drive, so the counts are floors, not exact:
        // each retry rebuilds the context and re-runs the journaled tool
        // attempt that the refused read abandoned.
        assert!(fixed.builds.load(Ordering::SeqCst) >= 1);
        assert!(built.executions.load(Ordering::SeqCst) >= 1);
    }
}
