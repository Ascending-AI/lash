mod tests {
    use std::sync::Arc;

    use crate::runtime::effect::*;
    use crate::runtime::{ToolChildAdmission, ToolChildCompletionRouting, ToolChildScope};
    use crate::tool_dispatch::{ToolAttemptEffectIdentity, ToolDispatchContext};
    use crate::{
        EffectHost, ExecutionScope, FrameNodeId, PreparedToolCall, ProcessExecutionEnvRef,
        ProcessExecutionEnvSpec, RuntimeEffectCommand, ScopedEffectController, SessionId, ToolId,
        ToolManifest, ToolRetryPolicy,
    };

    struct NoopTools;
    #[async_trait::async_trait]
    impl crate::ToolProvider for NoopTools {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            Vec::new()
        }

        fn resolve_contract(&self, _name: &str) -> Option<Arc<crate::ToolContract>> {
            None
        }

        async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
            crate::ToolOutcome::err_fmt("the rebind tests never run a tool").into()
        }
    }
    fn spec(turns: usize) -> ProcessExecutionEnvSpec {
        ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::bounded(turns)),
        )
    }
    fn manifest(id: &str) -> ToolManifest {
        let mut manifest = crate::ToolDefinition::raw(
            id,
            id,
            "a rebind fixture tool",
            crate::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "object", "additionalProperties": true }),
        )
        .manifest;
        manifest.retry_policy = ToolRetryPolicy::safe(4, 10, 100);
        manifest
    }
    fn invocation(effect_id: &str) -> crate::RuntimeInvocation {
        crate::RuntimeInvocation::effect(
            crate::EffectAddress::new(ExecutionScope::turn("child-session", "turn"), effect_id)
                .expect("a valid effect address"),
            crate::RuntimeAttribution::for_session("child-session"),
            effect_id,
        )
    }
    /// The request the child was admitted under. Every field here deliberately
    /// disagrees with [`lent`]'s, so a rebind that dropped a line shows up as the
    /// opener's value surviving.
    fn request() -> ToolChildRequest {
        request_with_identity(ToolAttemptEffectIdentity::Scalar {
            parent: Some(invocation("recorded-parent")),
        })
    }
    fn request_with_identity(identity: ToolAttemptEffectIdentity) -> ToolChildRequest {
        ToolChildRequest::new(
            PreparedToolCall::from_parts(
                "call-1",
                ToolId::from("search"),
                "search",
                serde_json::json!({ "q": "lash" }),
                None,
                serde_json::Value::Null,
            ),
            ToolChildAdmission::Catalog {
                manifest: Box::new(manifest("search")),
            },
            identity,
            ToolChildScope {
                opener: crate::EffectOpener::turn("child-session", "turn"),
                admitted_scope: crate::AdmittedScope::turn("child-session", "turn"),
                session_id: SessionId::from("child-session"),
                agent_frame_id: FrameNodeId::new("child-frame").expect("a valid frame id"),
            },
            ProcessExecutionEnvRef::new("env-ref"),
            ToolChildCompletionRouting::Inline,
        )
    }
    /// The opener's own context, every recorded field set to something the child
    /// must not inherit.
    fn lent() -> ToolDispatchContext<'static> {
        lent_with_direct_completions(crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ))
    }
    fn lent_with_direct_completions(
        direct_completions: crate::DirectCompletionClient<'static>,
    ) -> ToolDispatchContext<'static> {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(1);
        // Held for the test's lifetime, so the sender never reports a closed
        // channel for a reason unrelated to what is asserted.
        std::mem::forget(event_rx);
        let mut other_tool = manifest("opener-tool");
        other_tool.retry_policy = ToolRetryPolicy::Never;
        ToolDispatchContext {
            plugins: crate::support::plugin_host(Vec::new())
                .build_session("opener-session")
                .expect("plugin session"),
            tools: Arc::new(NoopTools),
            tool_registry: None,
            tool_catalog: Arc::new(crate::ToolCatalog::from_tool_definitions(vec![
                crate::ToolDefinition {
                    manifest: other_tool,
                    contract: crate::ToolContract::default(),
                },
            ])),
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
            direct_completions,
            parent_invocation: Some(invocation("opener-parent")),
            execution_env_spec: spec(9),
            session_id: SessionId::from("opener-session"),
            agent_frame_id: FrameNodeId::new("opener-frame").expect("a valid frame id"),
            event_tx,
            turn_activity_tx: None,
            checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::new(crate::SessionAttachmentStore::unavailable()),
            attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
            turn_context: crate::TurnContext::default(),
            clock: Arc::new(crate::SystemClock),
        }
    }
    /// The child's own admitted controller, bound to the child's claim scope and
    /// not the opener's.
    fn child_controller() -> ScopedEffectController<'static> {
        ScopedEffectController::shared(
            Arc::new(crate::testing::UnavailableEffectController),
            crate::AdmittedScope::turn("child-session", "turn"),
        )
        .expect("a valid child scope")
    }

    /// The backend host's own controller for the child's admitted turn scope:
    /// a durable participant whose await-event authority is the host's.
    fn durable_child_controller(host: &Arc<dyn EffectHost>) -> ScopedEffectController<'static> {
        host.scoped_static(crate::AdmittedScope::turn("child-session", "turn"))
            .expect("a valid child scope")
            .expect("the backend host lends a static controller")
    }

    /// A request whose recorded cancellation authority is exactly what `host`
    /// derives for the child's admitted scope — the fixture every durable-side
    /// check needs, because a durable participant always records `Some`.
    fn durably_admitted_request(
        host: &Arc<dyn EffectHost>,
        routing: ToolChildCompletionRouting,
    ) -> ToolChildRequest {
        let derived = crate::runtime::effect::executor::turn_control_binding_id_for_scope(
            &host.turn_control_binding_id(),
            &ExecutionScope::turn("child-session", "turn"),
        )
        .expect("a scope-derived binding id");
        let mut request = request().with_cancellation_authority(
            crate::TurnControlBindingId::new(derived).expect("a valid binding id"),
        );
        request.completion_routing = routing;
        request
    }

    /// A resolver that hands the group the one runner the test already resolved,
    /// once. The host's dispatch then claims the child's replay row and runs
    /// that runner under the claim, which is the only way production runs a
    /// tool child: a runner's nested admissions mint under the child's own row,
    /// so the row must exist before the runner starts.
    struct StagedGroupExecutor(std::sync::Mutex<Option<RuntimeEffectLocalExecutor<'static>>>);

    impl crate::GroupExecutors for StagedGroupExecutor {
        fn executor_for(
            &self,
            _envelope: &RuntimeEffectEnvelope,
        ) -> Option<RuntimeEffectLocalExecutor<'static>> {
            self.0
                .lock()
                .expect("the staged executor lock is never poisoned")
                .take()
        }
    }

    /// The claim pin is the recorded `AdmittedScope`, never `enclosing_process`.
    /// A process opener legitimately encloses its own incarnation, so the pair
    /// `opener = P#7, enclosing = P#7` validates — but when the admitted claim is a
    /// turn scope, the controller must stay that turn's controller. The retired
    /// post-admission pin block would have pinned P#7 onto it instead, making
    /// the execution context the claim pin.
    #[tokio::test]
    async fn a_process_openers_enclosing_incarnation_is_never_the_claim_pin() {
        let mut request = request();
        let opener_ref = crate::ProcessRef::new(
            "worker",
            crate::ProcessIncarnation::from_registration_sequence(7),
        );
        request.scope.opener = crate::EffectOpener::process(opener_ref.clone());
        request.enclosing_process = Some(opener_ref.clone());
        request
            .validate()
            .expect("a process opener enclosing its own incarnation is a legal request");

        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let tool_children = ToolChildHost::new(&host, backend.process_env_store());
        let binding = crate::GroupChildBinding {
            child: crate::EffectAddress::new(
                ExecutionScope::turn("child-session", "turn"),
                "child",
            )
            .expect("a valid child address"),
            membership: crate::EffectGroupMembership {
                group_key: "group".to_string(),
                position: 0,
                wake: crate::GroupWakePolicy::All,
                loser_disposition: crate::LoserPolicy::RunToCompletion,
            },
        };
        let controller =
            crate::tool_child_controller(&tool_children, &request.scope.admitted_scope, binding)
                .expect("the admitted pair constructs the child's controller");
        assert_eq!(
            controller.execution_scope(),
            &ExecutionScope::turn("child-session", "turn"),
            "the controller is the recorded claim's, a turn scope"
        );
        assert!(
            controller.admitted_process().is_none(),
            "the opener's incarnation never became the claim pin"
        );
    }
    /// §1 and the registry's key rule, at the driver's door: a child whose opener
    /// is not live in this process is **neither run nor failed**. The resolver
    /// answers absence, the group leaves the child accepted, and the process whose
    /// opener is live runs it.
    #[tokio::test]
    async fn a_child_whose_opener_is_not_registered_here_is_not_routed() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let tool_children = ToolChildHost::new(&host, backend.process_env_store());
        let envelope = crate::RuntimeEffectEnvelope::new(
            crate::RuntimeEffectInvocation::new(
                crate::EffectAddress::new(ExecutionScope::turn("child-session", "turn"), "child")
                    .expect("a valid effect address"),
                crate::RuntimeAttribution::for_session("child-session"),
                "child",
            ),
            RuntimeEffectCommand::ToolInvocation {
                request: Box::new(request()),
            },
        );

        assert!(
            crate::GroupExecutors::executor_for(tool_children.as_ref(), &envelope).is_none(),
            "an unregistered opener is a routing fact, not an executor and not a failure"
        );

        let lent_dispatch = lent();
        let lent_controller = lent_dispatch
            .effect_controller
            .scoped()
            .to_static()
            .expect("the lent dispatch's controller is 'static");
        let live = LiveOpenerContext::capture(
            &lent_dispatch,
            lent_controller,
            tokio_util::sync::CancellationToken::new(),
        );
        let guard = tool_children
            .openers()
            .register(crate::EffectOpener::turn("child-session", "turn"), live);
        assert!(
            crate::GroupExecutors::executor_for(tool_children.as_ref(), &envelope).is_some(),
            "the same child routes once its opener is live here"
        );
        drop(guard);
        assert!(
            crate::GroupExecutors::executor_for(tool_children.as_ref(), &envelope).is_none(),
            "and stops routing when the opener's registration ends"
        );
    }
    /// A command this resolver does not run is honestly not its child, and it
    /// says so rather than refusing the group on someone else's behalf. `Sleep`
    /// and `AwaitEvent` children are this resolver's too (FIG-3397), so the
    /// probe uses `SyncExecutionEnvironment` — a kind no group child can be.
    #[tokio::test]
    async fn the_resolver_answers_only_for_tool_children() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let tool_children = ToolChildHost::new(&host, backend.process_env_store());
        let envelope = crate::RuntimeEffectEnvelope::new(
            crate::RuntimeEffectInvocation::new(
                crate::EffectAddress::new(
                    ExecutionScope::turn("child-session", "turn"),
                    "env-sync",
                )
                .expect("a valid effect address"),
                crate::RuntimeAttribution::for_session("child-session"),
                "env-sync",
            ),
            RuntimeEffectCommand::SyncExecutionEnvironment {
                update_machine_config: false,
            },
        );
        assert!(crate::GroupExecutors::executor_for(tool_children.as_ref(), &envelope).is_none());
    }
    /// §3's cancellation line, wrong direction: a recorded binding this host did
    /// not mint for the admitted scope is a foreign authority — the cooperative
    /// signal it would honour is not the one this opener sends.
    #[tokio::test]
    async fn a_foreign_cancellation_binding_is_refused() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let tool_children = ToolChildHost::new(&host, backend.process_env_store());
        let request = request().with_cancellation_authority(
            crate::TurnControlBindingId::new("a-binding-this-host-did-not-mint")
                .expect("a valid binding id"),
        );
        let error =
            crate::validate_recorded_authorities(&tool_children, &child_controller(), &request)
                .await
                .expect_err("a binding this host did not mint for the scope is refused");
        assert_eq!(
            error.code,
            crate::RuntimeErrorCode::RuntimeEffectToolChildCancellationAuthority
        );
    }
    /// And the matching one is accepted: presence was never the check — the
    /// recorded id must equal what this host derives for the admitted scope, and
    /// it is only legal under the durable participation that minted it.
    #[tokio::test]
    async fn the_recorded_cancellation_binding_is_accepted() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let tool_children = ToolChildHost::new(&host, backend.process_env_store());
        let controller = durable_child_controller(&host);
        let request = durably_admitted_request(&host, ToolChildCompletionRouting::Inline);
        crate::validate_recorded_authorities(&tool_children, &controller, &request)
            .await
            .expect("the binding this host derives for the admitted scope is accepted");
    }
    /// A `None` record is legal only under local participation: reopened against
    /// a durable-journaled controller it is an inconsistency, because the
    /// cooperative authority exists and the record that omits it lies.
    #[tokio::test]
    async fn a_missing_cancellation_record_on_a_durable_participant_is_refused() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let tool_children = ToolChildHost::new(&host, backend.process_env_store());
        let controller = durable_child_controller(&host);
        let error = crate::validate_recorded_authorities(&tool_children, &controller, &request())
            .await
            .expect_err("a durable participant without a recorded binding is inconsistent");
        assert_eq!(
            error.code,
            crate::RuntimeErrorCode::RuntimeEffectToolChildCancellationAuthority
        );
    }
    /// And it is admitted where the authority exists.
    #[tokio::test]
    async fn durable_routing_with_a_durable_authority_is_accepted() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let tool_children = ToolChildHost::new(&host, backend.process_env_store());
        let controller = durable_child_controller(&host);
        let request = durably_admitted_request(&host, ToolChildCompletionRouting::Durable);
        crate::validate_recorded_authorities(&tool_children, &controller, &request)
            .await
            .expect("durable routing under a durable authority is admitted");
    }
    /// §2's lifetime line at the driver: the runner `executor_for` hands back owns
    /// the `LiveOpenerContext` it resolved against. Dropping the opener's
    /// registration guard between resolution and execution must neither stall the
    /// child on a re-registration nothing promised nor rebind it to a successor —
    /// the accepted child completes on the captured context.
    ///
    /// The resolved runner executes the way production executes it: the group's
    /// host dispatch claims the child's replay row and then runs the runner, so
    /// the child's nested admissions find the row they mint under.
    #[tokio::test]
    async fn a_resolved_child_executes_on_the_captured_opener_context() {
        let backend = crate::support::memory_backend().await;
        let controller = crate::support::scoped_controller(
            &backend,
            crate::AdmittedScope::turn("child-session", "turn"),
        );
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let env_store: Arc<dyn crate::ProcessExecutionEnvStore> = backend.process_env_store();
        let tool_children = ToolChildHost::new(&host, Arc::clone(&env_store));
        // A journaled host's child is a durable participant, so its recorded
        // request carries the cancellation binding the host derives for its scope.
        let mut request = durably_admitted_request(&host, ToolChildCompletionRouting::Inline);
        request.execution_env = crate::publish_process_execution_env(
            env_store.as_ref(),
            &crate::ArtifactOwner::host("tool-child-driver-tests"),
            &spec(3),
        )
        .await
        .expect("the recorded environment publishes");
        let opener = request.scope.opener.clone();
        let envelope = crate::RuntimeEffectEnvelope::new(
            crate::RuntimeEffectInvocation::new(
                crate::EffectAddress::new(ExecutionScope::turn("child-session", "turn"), "child")
                    .expect("a valid effect address"),
                crate::RuntimeAttribution::for_session("child-session"),
                "child",
            ),
            RuntimeEffectCommand::ToolInvocation {
                request: Box::new(request),
            },
        )
        .in_effect_group(
            "group",
            0,
            crate::GroupWakePolicy::All,
            crate::LoserPolicy::Cancel,
        );
        let lent_dispatch = lent();
        let lent_controller = lent_dispatch
            .effect_controller
            .scoped()
            .to_static()
            .expect("the lent dispatch's controller is 'static");
        let live = LiveOpenerContext::capture(
            &lent_dispatch,
            lent_controller,
            tokio_util::sync::CancellationToken::new(),
        );
        let guard = tool_children.openers().register(opener, live);
        let executor = crate::GroupExecutors::executor_for(tool_children.as_ref(), &envelope)
            .expect("the live opener routes the child");

        // The opener's registration ends between resolution and execution: the
        // runner must still complete on the context it captured rather than wait
        // for a re-registration nothing promises.
        drop(guard);
        crate::RuntimeEffectController::register_group_executors(
            controller.as_ref(),
            Arc::new(StagedGroupExecutor(std::sync::Mutex::new(Some(executor)))),
        )
        .expect("the staged resolver registers once");
        let mut handle = crate::RuntimeEffectController::open_effect_group(
            controller.as_ref(),
            crate::RuntimeEffectGroup::try_new(
                crate::RuntimeEffectInvocation::new(
                    crate::EffectAddress::new(
                        ExecutionScope::turn("child-session", "turn"),
                        "group:group",
                    )
                    .expect("a valid group address"),
                    crate::RuntimeAttribution::none(),
                    "group",
                ),
                "group",
                vec![envelope],
                crate::GroupWakePolicy::All,
                crate::LoserPolicy::Cancel,
            )
            .expect("the one-child group assembles"),
        )
        .await
        .expect("the group opens and dispatches the resolved runner");
        // The timeout is what makes a wait-for-reregistration regression a
        // failure and not a hang.
        let settlement = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            crate::RuntimeEffectController::await_next_settlement(
                controller.as_ref(),
                &mut handle,
                tokio_util::sync::CancellationToken::new(),
            ),
        )
        .await
        .expect("a resolved child never waits for the opener to re-register")
        .expect("the group settles its one child");
        let outcome = settlement
            .outcome
            .expect("the captured context executes the child");
        assert!(
            matches!(outcome, crate::RuntimeEffectOutcome::ToolInvocation { .. }),
            "the child settles its own recorded work"
        );
    }
}
