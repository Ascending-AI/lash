mod tests {
    use lash_core_execution::core_internal::RuntimeExecutionContextRuntimeOps as _;

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
        let backend = crate::support::sqlite_memory_store_backend().await;
        let context = crate::testing::TestExecutionContextBuilder::for_backend(&backend)
            .session_id("session")
            .build()
            .into_runtime();
        let prepared = crate::process_start_execution_env(&context, engine_start().into())
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
        let backend = crate::support::sqlite_memory_store_backend().await;
        let spec = crate::ProcessExecutionEnvSpec::new(
            crate::AdmittedPluginConfig::default(),
            crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
                lash_core_execution::NoProgressBudget::bounded(12),
            ),
            crate::SessionToolAccess::ambient(),
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
            .with_process_execution(
                &crate::ProcessRecord::from_registration(
                    parent.clone(),
                    crate::ProcessId::fixture("parent"),
                ),
                None,
            );
        let prepared = crate::process_start_execution_env(&context, engine_start().into())
            .await
            .expect("capture child");
        assert_eq!(prepared.env_ref, Some(inherited.clone()));
        backend
            .process_env_store()
            .end_process_env_referrer(&crate::ResolvedArtifactCleanup {
                referrer: pin.referrer(),
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
        let backend = crate::support::sqlite_memory_store_backend().await;
        let render = crate::RecordedRender {
            renderer_id: "parent.renderer".to_string(),
            params: serde_json::json!({"print": {"max_chars": 37}}),
        };
        let context = crate::testing::TestExecutionContextBuilder::for_backend(&backend)
            .session_id("session")
            .build()
            .into_runtime()
            .with_recorded_render(render.clone());
        let prepared = crate::process_start_execution_env(&context, engine_start().into())
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
    /// after its referrer is fenced; a child start's capture preserves that
    /// refusal.
    #[tokio::test]
    async fn an_ended_execution_referrer_still_fails_the_public_env_ref_publish() {
        let backend = crate::support::sqlite_memory_store_backend().await;
        let env_store = backend.process_env_store();
        let context = crate::testing::TestExecutionContextBuilder::for_backend(&backend)
            .session_id("session")
            .build()
            .into_runtime();
        let claim = context.execution_claim().expect("execution claim");

        crate::process_start_execution_env(&context, engine_start().into())
            .await
            .expect("first publish under a live owner");

        env_store
            .end_process_env_referrer(&crate::ResolvedArtifactCleanup {
                referrer: claim.referrer(),
                carries: Vec::new(),
            })
            .await
            .expect("retire the durable owner");

        let error = crate::process_start_execution_env(&context, engine_start().into())
            .await
            .expect_err("a fenced durable owner must not resolve to a reclaimed reference");
        assert!(
            crate::artifact_referrer_ended(&error) == Some(&claim.referrer()),
            "unexpected error: {error}"
        );
    }
}
