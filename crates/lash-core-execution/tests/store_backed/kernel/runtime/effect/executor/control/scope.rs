mod admitted_scope_tests {
    use std::sync::Arc;

    use crate::support::prelude::*;
    use crate::{AdmittedScope, ProcessRef, RuntimeEffectController, ScopedEffectController};

    fn shared_controller() -> Arc<dyn RuntimeEffectController> {
        Arc::new(crate::testing::UnavailableEffectController)
    }

    /// A same-name successor incarnation is a different opener, so rescope
    /// refuses it outright rather than rebinding the pin: the admission a
    /// controller carries is fixed at construction (ADR 0099 §1). The
    /// incarnations here come from the real registry — one name registered,
    /// completed, pruned and registered again — not fabricated sequence
    /// numbers, so the pair the controller refuses is exactly the pair a
    /// stale pin would present.
    #[tokio::test]
    async fn a_rescope_onto_a_same_name_successor_incarnation_is_refused() {
        let registry = crate::support::memory_backend().await.process_registry();
        let process_id = crate::ProcessId::from("worker");
        let registration = || {
            crate::ProcessRegistration::new(
                process_id.clone(),
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
            .expect("register the old incarnation");
        registry
            .complete_process(
                &process_id,
                crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                    serde_json::json!("old"),
                )),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete the old incarnation");
        registry
            .prune_terminal_processes(u64::MAX, None, crate::ProjectionWatermark::NoProjector)
            .await
            .expect("prune the old incarnation so the name is reusable");
        let successor = registry
            .register_process(registration())
            .await
            .expect("register the same-name successor incarnation");
        let old_ref = ProcessRef::from_record(&old);
        let successor_ref = ProcessRef::from_record(&successor);
        assert_ne!(old_ref.incarnation, successor_ref.incarnation);

        let scoped = ScopedEffectController::shared(
            shared_controller(),
            AdmittedScope::process(old_ref.clone()),
        )
        .expect("process scope");

        // Rescoping onto the pin it already carries is still fine — that is
        // the same admission restated, not a repin.
        scoped
            .rescope(AdmittedScope::process(old_ref))
            .expect("rescope onto the same incarnation");

        let error = scoped
            .rescope(AdmittedScope::process(successor_ref))
            .err()
            .expect("the successor incarnation is a different opener");
        assert_eq!(
            error.code,
            crate::RuntimeErrorCode::ExecutionScopeAdmissionRefused
        );
        assert_eq!(
            scoped.admitted_process(),
            Some(&ProcessRef::from_record(&old)),
            "a refused rescope leaves the admitted pair untouched"
        );
    }
}
