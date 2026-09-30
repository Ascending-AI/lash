mod tests {
    use crate::support::prelude::*;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::SessionId;
    use crate::runtime::ProcessRegistryFaults;
    use crate::runtime::{WakeDeliveryDriver, WorkCadencePolicy};

    use crate::support::memory_store_set;

    fn external_registration() -> crate::ProcessRegistration {
        crate::ProcessRegistration::new(
            crate::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            crate::ProcessProvenance::host(),
            crate::Lifetime::Detached,
        )
    }

    #[tokio::test]
    async fn a_stale_wake_for_a_pruned_process_does_not_abort_delivery() {
        let backend = memory_store_set().await;
        let registry = Arc::new(ProcessRegistryFaults::new(backend.process_registry()));
        let old = registry
            .register_process(external_registration())
            .await
            .expect("register the first process");
        registry
            .complete_process(
                &old.id,
                crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                    serde_json::Value::Null,
                )),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete the first process");
        registry
            .prune_terminal_processes(u64::MAX, None, crate::ProjectionWatermark::NoProjector)
            .await
            .expect("prune the first process");
        let current = registry
            .register_process(external_registration())
            .await
            .expect("register a later process");
        assert_ne!(old.id, current.id, "a minted id is never reused");
        registry
            .inject_claimed_wake_delivery(crate::ProcessWakeDelivery {
                version: crate::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
                wake_id: format!("wake:v1:blake3:{}", "a".repeat(64)),
                target_session_id: SessionId::from("wake-target"),
                process_id: old.id.clone(),
                sequence: 1,
                event_type: "producer.wake".to_string(),
                event_invocation: crate::RuntimeInvocation::effect(
                    crate::EffectAddress::new(
                        crate::ExecutionScope::process(old.id.clone()),
                        "wake-replay",
                    )
                    .expect("valid wake delivery address"),
                    crate::RuntimeAttribution::none(),
                    "wake-effect",
                ),
                process_caused_by: None,
                authority: crate::QueuedWorkAuthority::default(),
                input: "wake".to_string(),
                created_at_ms: 0,
            })
            .expect("inject the pruned process's wake");

        let report = WakeDeliveryDriver::drive_pending_once(
            registry.clone(),
            backend.session_store_factory(),
            Arc::new(crate::NoSessionWork::new()),
            Arc::new(crate::SystemClock),
            1,
        )
        .await
        .expect("a stale delivery must not abort the claimed page");

        assert_eq!(report.inspected, 1);
        assert_eq!(report.enqueued, 0);
        assert_eq!(
            report.retryable_failures, 1,
            "the pruned delivery has no durable row left to settle"
        );
        assert!(matches!(
            registry.get_process(&old.id).await,
            Err(crate::PluginError::ProcessNoLongerRetained { .. })
        ));
        assert_eq!(
            registry
                .get_process(&current.id)
                .await
                .expect("read the later process"),
            Some(current)
        );
    }

    #[tokio::test]
    async fn terminal_constructor_rejects_zero_poll_delay_directly() {
        let backend = memory_store_set().await;
        let work_cadence = WorkCadencePolicy {
            poll_initial: Duration::ZERO,
            ..WorkCadencePolicy::default()
        };

        let Err(error) = WakeDeliveryDriver::with_work_cadence(
            backend.process_registry(),
            backend.session_store_factory(),
            Arc::new(crate::NoSessionWork::new()),
            Arc::new(crate::SystemClock),
            crate::DeliveryPolicy::EarliestSafeBoundary,
            work_cadence,
        ) else {
            panic!("terminal wake-delivery construction must reject zero-delay polling");
        };
        assert!(
            error.to_string().contains("work_cadence.poll_initial"),
            "error must identify the rejected poll field: {error}"
        );
    }
}
