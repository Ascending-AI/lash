mod tests {
    use crate::SessionId;
    use crate::plugin::{SessionObservedProcessOutcome, SessionObserverIntent};
    use crate::runtime::{SessionObserverIntentSource, reconcile_session_process_observer_intents};
    use crate::support::prelude::*;

    use crate::support::memory_store_set;

    #[tokio::test]
    async fn noproc_receipts_preserve_missing_and_pruned_outcomes() {
        let backend = memory_store_set().await;
        let registry = backend.process_registry();
        let registered = registry
            .register_process(crate::ProcessRegistration::new(
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
            .expect("register process before pruning");
        let pruned = registry
            .complete_process(
                &registered.id,
                crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                    serde_json::Value::Null,
                )),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete process before pruning");
        crate::support::after_millisecond_tick(pruned.updated_at_ms).await;
        registry
            .prune_terminal_processes(
                pruned.updated_at_ms.saturating_add(1),
                None,
                crate::ProjectionWatermark::NoProjector,
            )
            .await
            .expect("prune terminal process");

        let receipts = reconcile_session_process_observer_intents(
            Some(registry.as_ref()),
            &SessionId::from("noproc-session"),
            SessionObserverIntentSource::Unstored(vec![
                SessionObserverIntent::host_requested(crate::ProcessId::fixture("unknown-host")),
                SessionObserverIntent::host_requested(crate::ProcessId::fixture("unknown-fork")),
                SessionObserverIntent::host_requested(pruned.id.clone()),
                SessionObserverIntent::host_requested(pruned.id.clone()),
            ]),
        )
        .await
        .expect("noproc settlement remains best effort");

        assert_eq!(receipts.len(), 4);
        assert!(
            receipts[..2]
                .iter()
                .all(|receipt| receipt.outcome == SessionObservedProcessOutcome::NotFound)
        );
        assert!(receipts[2..].iter().all(|receipt| matches!(
            receipt.outcome,
            SessionObservedProcessOutcome::NoLongerRetained { .. }
        )));
    }
    #[tokio::test]
    async fn a_selected_pruned_process_never_retargets_to_a_later_process() {
        let backend = memory_store_set().await;
        let registry = backend.process_registry();
        let registration = crate::ProcessRegistration::new(
            crate::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            crate::RecoveryContract::ExternallyOwned,
            crate::ProcessProvenance::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        );
        let first = registry
            .register_process(registration.clone())
            .await
            .expect("first run");
        let selected = first.id.clone();
        let terminal = registry
            .complete_process(
                &first.id,
                crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                    serde_json::Value::Null,
                )),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("finish first run");
        crate::support::after_millisecond_tick(terminal.updated_at_ms).await;
        registry
            .prune_terminal_processes(
                terminal.updated_at_ms.saturating_add(1),
                None,
                crate::ProjectionWatermark::NoProjector,
            )
            .await
            .expect("prune selected run");
        let newer = registry
            .register_process(registration)
            .await
            .expect("a later process");
        let session_id = SessionId::from("selected-observer");
        let receipts = reconcile_session_process_observer_intents(
            Some(registry.as_ref()),
            &session_id,
            SessionObserverIntentSource::Unstored(vec![SessionObserverIntent::host_requested(
                selected.clone(),
            )]),
        )
        .await
        .expect("best effort receipt");
        assert_ne!(newer.id, selected, "a minted id is never reused");
        assert!(
            matches!(
                receipts[0].outcome,
                SessionObservedProcessOutcome::NoLongerRetained { .. }
            ),
            "a pruned selection is reported as pruned: {:?}",
            receipts[0].outcome
        );
        assert!(
            !registry
                .is_observer(&session_id, &newer.id)
                .await
                .expect("read newer observers")
        );
    }
}
