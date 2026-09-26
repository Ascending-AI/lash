mod admitted_scope_tests {
    use std::sync::Arc;

    use crate::support::prelude::*;
    use crate::{AdmittedScope, RuntimeEffectController, ScopedEffectController};

    fn shared_controller() -> Arc<dyn RuntimeEffectController> {
        Arc::new(crate::testing::UnavailableEffectController)
    }

    /// Another process is a different opener, so rescope refuses it outright
    /// rather than rebinding the admission: the admission a controller carries
    /// is fixed at construction (ADR 0099 §1). The ids come from the real
    /// registry — one process registered, completed and pruned, then another
    /// registered — so the pair refused is exactly what a stale admission
    /// would present.
    #[tokio::test]
    async fn a_rescope_onto_another_process_is_refused() {
        let registry = crate::support::memory_store_set().await.process_registry();
        let registration = || {
            crate::ProcessRegistration::new(
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
        };
        let old = registry
            .register_process(registration())
            .await
            .expect("register the first process");
        registry
            .complete_process(
                &old.id,
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
            .register_process(registration())
            .await
            .expect("register a later process");
        let old_ref = old.id.clone();
        let successor_ref = successor.id.clone();
        assert_ne!(old_ref, successor_ref, "a minted id is never reused");

        let scoped = ScopedEffectController::shared(
            shared_controller(),
            AdmittedScope::process(old_ref.clone()),
        )
        .expect("process scope");

        // Rescoping onto the pin it already carries is still fine — that is
        // the same admission restated, not a repin.
        scoped
            .rescope(AdmittedScope::process(old_ref))
            .expect("rescope onto the same process");

        let error = scoped
            .rescope(AdmittedScope::process(successor_ref))
            .err()
            .expect("another process is a different opener");
        assert_eq!(
            error.code,
            crate::RuntimeErrorCode::ExecutionScopeAdmissionRefused
        );
        assert_eq!(
            scoped.admitted_process(),
            Some(&old.id),
            "a refused rescope leaves the admitted pair untouched"
        );
    }
}
