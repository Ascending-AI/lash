mod tests {
    use std::sync::Arc;

    use crate::support::prelude::*;
    use crate::{ProcessId, RuntimeExecutionContext};

    fn registration_for_parent_scope(process_id: &str) -> crate::ProcessRegistration {
        crate::ProcessRegistration::new(
            ProcessId::from(process_id),
            crate::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            crate::RecoveryContract::ExternallyOwned,
            crate::ProcessProvenance::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        )
    }

    /// A context whose controller is the backend host's own, admitted under
    /// `admitted`.
    fn scoped_context(
        backend: &lash_sqlite_store::SqliteBackend,
        session_id: &str,
        admitted: crate::AdmittedScope,
    ) -> RuntimeExecutionContext<'static> {
        let controller = crate::EffectHost::scoped_static(backend.effect_host().as_ref(), admitted)
            .expect("the test scope validates")
            .expect("the backend host lends a static controller");
        crate::testing::TestExecutionContextBuilder::for_backend(&backend.clone().into())
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

    /// The session path publishes nothing before the process-start effect is journaled.
    ///
    /// This is the FIG-3028 / #1390 regression, re-pointed at the journaled publish (FIG-3050).
    /// #1390 kept the pre-journal staging publish and taught it to tolerate the permanently retired
    /// staging owner a replay revisits; the spec now travels in the command instead, so there is no
    /// pre-journal artifact and no owner to revisit. The journaled publish keeps the tolerance, and
    /// `process_start_transfers_environment_and_replays_after_staging_retirement`
    /// (`runtime::effect::executor::process_local`) exercises it there.
    #[tokio::test]
    async fn a_session_path_process_start_publishes_no_environment_before_its_journal() {
        let backend = crate::support::memory_backend().await;
        let env_store = backend.process_env_store();
        let context =
            crate::testing::TestExecutionContextBuilder::for_backend(&backend.clone().into())
                .session_id("session")
                .build()
                .into_runtime();
        let registration = crate::ProcessRegistration::new(
            "journaled-process",
            crate::ProcessInput::Engine {
                kind: "test-engine".to_string(),
                payload: serde_json::json!({"program": "probe"}),
            },
            crate::RecoveryContract::Rerunnable,
            crate::ProcessProvenance::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        );

        let (prepared, env_spec) = crate::process_start_execution_env(&context, registration);
        assert_eq!(
            prepared.env_ref, None,
            "a session-path start must not carry a reference its journal has not produced"
        );
        let env_spec = env_spec.expect("the captured spec rides the process-start command");
        let staged_ref = env_spec.stable_ref().expect("stable environment reference");
        assert_eq!(
            env_store
                .get_process_execution_env(&staged_ref)
                .await
                .expect("read the environment store"),
            None,
            "nothing is published before the process-start effect runs"
        );
    }

    /// The replay tolerance is scoped to process-start staging. A durable owner (trigger
    /// registration publishes under the execution's own artifact owner and then persists the
    /// reference) must still fail at publish time once that owner is fenced, rather than record a
    /// reference to bytes the retirement reclaimed.
    #[tokio::test]
    async fn a_retired_durable_owner_still_fails_the_public_env_ref_publish() {
        let backend = crate::support::memory_backend().await;
        let env_store = backend.process_env_store();
        let context =
            crate::testing::TestExecutionContextBuilder::for_backend(&backend.clone().into())
                .session_id("session")
                .build()
                .into_runtime();
        let owner = crate::ArtifactOwner::Execution(crate::ExecutionScope::runtime_operation(
            "durable-owner",
        ));

        context
            .captured_process_execution_env_ref(&owner)
            .await
            .expect("first publish under a live owner");

        env_store
            .retire_process_execution_env_owner(&owner)
            .await
            .expect("retire the durable owner");

        let error = context
            .captured_process_execution_env_ref(&owner)
            .await
            .expect_err("a fenced durable owner must not resolve to a reclaimed reference");
        assert!(
            crate::artifact_owner_is_permanently_retired(&error),
            "unexpected error: {error}"
        );
    }

    /// A process scope nobody bound an admitted incarnation to cannot be built —
    /// `AdmittedScope` refuses the unpinned pair at construction, so no execution
    /// context can ever carry the reusable name as a fallback. The registry in
    /// this fixture *could* resolve the name, which is what makes the construction
    /// refusal prove the derivation never asks it.
    #[tokio::test]
    async fn a_process_scope_without_an_admitted_incarnation_is_unconstructible() {
        let backend = crate::support::memory_backend().await;
        let registry: Arc<dyn crate::ProcessRegistry> = backend.process_registry();
        registry
            .register_process(registration_for_parent_scope("worker"))
            .await
            .expect("first registration");
        assert!(
            matches!(
                crate::AdmittedScope::new(crate::ExecutionScope::process("worker"), None),
                Err(crate::AdmittedScopeError::ProcessIncarnationMissing { .. })
            ),
            "the reusable name alone is never admitted"
        );
    }

    /// A same-name successor already retained in the registry does not rebind the
    /// pinned parent: a child started by incarnation 1 of `worker` parents on
    /// incarnation 1 even though the registry now holds incarnation 2.
    #[tokio::test]
    async fn a_child_started_from_a_process_incarnation_keeps_the_pinned_parent() {
        let backend = crate::support::memory_backend().await;
        let registry: Arc<dyn crate::ProcessRegistry> = backend.process_registry();
        let retired = registry
            .register_process(registration_for_parent_scope("worker"))
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
            .expect("complete the first incarnation");
        registry
            .prune_terminal_processes(u64::MAX, None, crate::ProjectionWatermark::NoProjector)
            .await
            .expect("prune the retired incarnation");
        let successor = registry
            .register_process(registration_for_parent_scope("worker"))
            .await
            .expect("same-name successor registration");
        assert_ne!(
            successor.incarnation, retired.incarnation,
            "the fixture must hold a successor incarnation under the same name"
        );
        // Recovery validates a retained pair with get_process_ref — and the
        // superseded incarnation is refused there, not rebound.
        assert!(
            registry
                .get_process_ref(&crate::ProcessRef::new(
                    retired.id.clone(),
                    retired.incarnation,
                ))
                .await
                .is_err(),
            "get_process_ref must refuse the superseded incarnation"
        );

        let context = scoped_context(
            &backend,
            "session-1",
            crate::AdmittedScope::process(crate::ProcessRef::new(
                retired.id.clone(),
                retired.incarnation,
            )),
        )
        .with_process_execution(
            &registration_for_parent_scope("worker"),
            Some(process_event_context(&retired.id, Arc::clone(&registry))),
        );
        assert_eq!(
            context
                .child_process_parent_scope()
                .expect("the pinned incarnation is the parent"),
            crate::ParentScope::process(crate::ProcessRef::from_record(&retired)),
        );
    }
}
