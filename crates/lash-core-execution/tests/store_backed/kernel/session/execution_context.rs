mod tests {
    use std::sync::Arc;

    use crate::{ProcessId, RuntimeExecutionContext};

    fn registration_for_starter(_process_id: &str) -> crate::ProcessRegistration {
        crate::ProcessRegistration::new(
            crate::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            crate::ProcessProvenance::host(),
            crate::Lifetime::Detached,
        )
    }

    /// A context whose controller is the backend host's own, admitted under
    /// `admitted`.
    fn scoped_context(
        backend: &crate::Backend,
        session_id: &str,
        admitted: crate::AdmittedScope,
    ) -> RuntimeExecutionContext<'static> {
        let controller = crate::EffectHost::scoped_static(backend.effect_host().as_ref(), admitted)
            .expect("the test scope validates")
            .expect("the backend host lends a static controller");
        crate::testing::TestExecutionContextBuilder::for_backend(backend)
            .session_id(session_id)
            .borrowed_effect_controller(controller)
            .build()
            .into_runtime()
    }

    fn process_event_context(
        process_id: &ProcessId,
        registry: Arc<dyn crate::ProcessRegistry>,
    ) -> crate::session::RuntimeExecutionProcessEventContext {
        crate::session::RuntimeExecutionProcessEventContext {
            execution_write_authority: crate::ProcessExecutionWriteAuthority::invocation(
                process_id.clone(),
                "test-write-authority",
            ),
            process_work: crate::testing::process_work_wiring_for_registry(registry),
            store: None,
            session_store_factory: None,
            queued_work: Arc::new(crate::NoSessionWork::new()),
            process_wake_delivery_policy: crate::DeliveryPolicy::EarliestSafeBoundary,
            clock: Arc::new(crate::SystemClock),
        }
    }

    fn engine_start() -> crate::ProcessRegistration {
        crate::ProcessRegistration::new(
            crate::ProcessInput::Engine {
                kind: "test-engine".to_string(),
                payload: serde_json::json!({"program": "probe"}),
            },
            crate::ProcessProvenance::host(),
            crate::Lifetime::Detached,
        )
    }

    #[tokio::test]
    async fn a_session_path_start_holds_its_environment_before_journaling_its_digest() {
        let backend = crate::support::memory_store_backend().await;
        let context = crate::testing::TestExecutionContextBuilder::for_backend(&backend)
            .session_id("session")
            .build()
            .into_runtime();
        let prepared = crate::process_start_execution_env(&context, engine_start())
            .await
            .expect("capture");
        let env_ref = prepared
            .env_ref
            .expect("the command carries the published digest");
        crate::load_process_execution_env(backend.process_env_store().as_ref(), &env_ref)
            .await
            .expect("the execution holds durable bytes before journaling");
        backend
            .process_env_store()
            .end_process_env_referrer(&crate::ResolvedArtifactCleanup {
                referrer: context
                    .execution_claim()
                    .expect("execution claim")
                    .referrer()
                    .clone(),
                carries: Vec::new(),
            })
            .await
            .expect("journal ends");
        assert_eq!(
            backend
                .process_env_store()
                .get_process_execution_env(&env_ref)
                .await
                .expect("read"),
            None
        );
    }

    #[tokio::test]
    async fn a_start_inside_a_process_execution_inherits_the_recorded_env_ref() {
        let backend = crate::support::memory_store_backend().await;
        let spec = crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        );
        let pin = crate::testing::host_pin_claim_for_testing();
        let inherited =
            crate::publish_process_execution_env(backend.process_env_store().as_ref(), &pin, &spec)
                .await
                .expect("parent's environment");
        let parent = engine_start().with_execution_env_ref(Some(inherited.clone()));
        let context = crate::testing::TestExecutionContextBuilder::for_backend(&backend)
            .session_id("session")
            .build()
            .into_runtime()
            .with_process_execution(crate::ProcessId::fixture("parent"), &parent, None);
        let prepared = crate::process_start_execution_env(&context, engine_start())
            .await
            .expect("capture child");
        assert_eq!(prepared.env_ref, Some(inherited.clone()));
        backend
            .process_env_store()
            .end_process_env_referrer(&crate::ResolvedArtifactCleanup {
                referrer: pin.referrer().clone(),
                carries: Vec::new(),
            })
            .await
            .expect("parent's pin ends");
        crate::load_process_execution_env(backend.process_env_store().as_ref(), &inherited)
            .await
            .expect("child declaration holds inherited bytes independently");
    }

    #[tokio::test]
    async fn detached_child_start_carries_the_parents_recorded_render() {
        let backend = crate::support::memory_store_backend().await;
        let render = crate::RecordedRender {
            renderer_id: "parent.renderer".to_string(),
            params: serde_json::json!({"print": {"max_chars": 37}}),
        };
        let context = crate::testing::TestExecutionContextBuilder::for_backend(&backend)
            .session_id("session")
            .build()
            .into_runtime()
            .with_recorded_render(render.clone());
        let prepared = crate::process_start_execution_env(&context, engine_start())
            .await
            .expect("capture child");
        let spec = crate::load_process_execution_env(
            backend.process_env_store().as_ref(),
            &prepared.env_ref.expect("digest"),
        )
        .await
        .expect("load captured render");
        assert_eq!(spec.render, Some(render.clone()));
        assert_eq!(
            crate::RecordedRender::require_available(spec.render.as_ref(), "parent.renderer"),
            Ok(&render)
        );
        assert_eq!(
            crate::RecordedRender::require_available(spec.render.as_ref(), "another.renderer"),
            Err(crate::RuntimeErrorCode::RecordedRendererUnavailable)
        );
    }

    /// An execution that publishes a durable environment cannot publish again
    /// after its referrer is fenced; the public helper preserves that refusal.
    #[tokio::test]
    async fn an_ended_execution_referrer_still_fails_the_public_env_ref_publish() {
        let backend = crate::support::memory_store_backend().await;
        let env_store = backend.process_env_store();
        let context = crate::testing::TestExecutionContextBuilder::for_backend(&backend)
            .session_id("session")
            .build()
            .into_runtime();
        let claim = context.execution_claim().expect("execution claim");

        context
            .captured_process_execution_env_ref(&claim)
            .await
            .expect("first publish under a live owner");

        env_store
            .end_process_env_referrer(&crate::ResolvedArtifactCleanup {
                referrer: claim.referrer().clone(),
                carries: Vec::new(),
            })
            .await
            .expect("retire the durable owner");

        let error = context
            .captured_process_execution_env_ref(&claim)
            .await
            .expect_err("a fenced durable owner must not resolve to a reclaimed reference");
        assert!(
            crate::artifact_referrer_ended(&error) == Some(claim.referrer()),
            "unexpected error: {error}"
        );
    }

    /// A child started by a process is started by that process's minted id, even
    /// when the process was pruned and another process now runs the same
    /// definition: nothing on the derivation reads the registry.
    #[tokio::test]
    async fn a_child_parents_on_the_process_that_started_it() {
        let backend = crate::support::memory_store_backend().await;
        let registry: Arc<dyn crate::ProcessRegistry> = backend.process_registry();
        let retired = registry
            .register_process(registration_for_starter("worker"))
            .await
            .expect("first registration");
        registry
            .complete_process(
                &retired.id,
                crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                    serde_json::json!("old"),
                )),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete the first process");
        registry
            .prune_terminal_processes(u64::MAX, None, crate::ProjectionWatermark::NoProjector)
            .await
            .expect("prune the first process");
        let successor = registry
            .register_process(registration_for_starter("worker"))
            .await
            .expect("a later process of the same definition");
        assert_ne!(successor.id, retired.id, "a minted id is never reused");
        assert!(
            registry.get_process(&retired.id).await.is_err(),
            "a pruned process's id refuses; it never resolves to the later process"
        );

        let context = scoped_context(
            &backend,
            "session-1",
            crate::AdmittedScope::process(retired.id.clone()),
        )
        .with_process_execution(
            retired.id.clone(),
            &registration_for_starter("worker"),
            Some(process_event_context(&retired.id, Arc::clone(&registry))),
        );
        assert_eq!(
            context
                .start_cx()
                .expect("the starting process materializes a start context")
                .starter()
                .id(),
            &crate::ScopeId::process(retired.id.clone()),
        );
    }
}
