mod tests {
    use std::sync::Arc;

    use crate::runtime::RuntimeEffectControllerHandle;
    use crate::support::prelude::*;
    use crate::{InternalProcessAdmin, ProcessId, SessionId};

    fn admin(processes: Arc<dyn crate::ProcessService>) -> InternalProcessAdmin<'static> {
        crate::internal_process_admin(
            SessionId::from("session"),
            crate::session_graph::frame_node_id(&SessionId::from("session"), "frame"),
            processes,
            RuntimeEffectControllerHandle::shared(Arc::new(
                crate::testing::UnavailableEffectController,
            )),
            crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            ),
        )
    }

    #[tokio::test]
    async fn await_process_requires_visibility_then_allows_observed_process() {
        let registry = crate::support::memory_backend().await.process_registry();
        let host = Arc::new(
            crate::testing::MockSessionManager::default().with_process_registry(registry.clone()),
        );
        registry
            .register_process(crate::ProcessRegistration::new(
                "process",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::host(),
                crate::ProcessLifecyclePolicy::new(
                    crate::ParentScope::Host,
                    crate::OnParentEnd::Abandon,
                ),
            ))
            .await
            .expect("register process");
        registry
            .complete_process(
                &ProcessId::from("process"),
                crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                    serde_json::json!("done"),
                )),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete process");
        let processes: Arc<dyn crate::ProcessService> = host.clone();
        let admin = admin(processes);

        let hidden = admin
            .await_process(&ProcessId::from("process"))
            .await
            .expect_err("unobserved process must be hidden");
        assert_eq!(
            hidden.to_string(),
            "plugin session error: process handle `process` is not live or visible in this session"
        );

        registry
            .add_observer(
                &SessionId::from("session"),
                &ProcessId::from("process"),
                crate::ProcessObserverBy::host("tool-provider-test"),
            )
            .await
            .expect("observe process");
        assert!(
            admin
                .await_process(&ProcessId::from("process"))
                .await
                .is_ok()
        );
    }
}
