mod tests {
    use crate::plugin::{SessionObservedProcessOutcome, SessionObserverIntent};
    use crate::runtime::{SessionObserverIntentSource, reconcile_session_process_observer_intents};
    use lash_execution_contracts::{
        ProcessEventLogTestSupport, ProcessLifecycle, ProcessObserverRegistry, ProcessRegistrar,
        ProcessRetention,
    };
    use lash_vocabulary::identity::SessionId;

    use crate::support::sqlite_memory_process_store_set;

    /// A session's observer intents add exactly the edges they name, each
    /// with its typed outcome and one replay-keyed observer event, and mint
    /// no edge to a process the host did not name (FIG-5310, ported from
    /// lash-core's `session_creation_applies_only_named_process_observers_with_typed_outcomes`).
    #[tokio::test]
    async fn named_observer_intents_add_only_their_edges_with_typed_outcomes() {
        let backend = sqlite_memory_process_store_set().await;
        let registry = backend.process_registry();
        let mut ids = std::collections::BTreeMap::new();
        for label in ["named", "unnamed", "pruned"] {
            let registered = registry
                .register_process(lash_execution_contracts::testing::held_engine_registration(
                    serde_json::Value::Null,
                    lash_execution_contracts::process::ProcessProvenance::host(),
                    lash_execution_contracts::process::Lifetime::Detached,
                ))
                .await
                .expect("register an observer test process");
            ids.insert(label, registered.id);
        }
        let pruned = registry
            .complete_process(
                &ids["pruned"],
                lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
                    crate::ToolCallOutput::success(serde_json::Value::Null),
                ),
                lash_execution_contracts::process::ProcessCompletionAuthority::workflow_key(
                    &ids["pruned"],
                ),
            )
            .await
            .expect("complete the process before pruning");
        crate::support::after_millisecond_tick(pruned.updated_at_ms).await;
        registry
            .prune_terminal_processes(
                pruned.updated_at_ms.saturating_add(1),
                None,
                lash_execution_contracts::process::ProjectionWatermark::NoProjector,
            )
            .await
            .expect("prune the terminal process");
        let session_id = SessionId::from("observer-child");

        let receipts = reconcile_session_process_observer_intents(
            Some(registry.as_ref()),
            &session_id,
            SessionObserverIntentSource::Unstored(vec![
                SessionObserverIntent::host_requested(ids["named"].clone()),
                SessionObserverIntent::host_requested(
                    lash_vocabulary::identity::ProcessId::fixture("missing-process"),
                ),
                SessionObserverIntent::host_requested(ids["pruned"].clone()),
            ]),
        )
        .await
        .expect("the intents settle");

        assert_eq!(
            receipts
                .iter()
                .map(|receipt| receipt.process_id.clone())
                .collect::<Vec<_>>(),
            [
                ids["named"].clone(),
                lash_vocabulary::identity::ProcessId::fixture("missing-process"),
                ids["pruned"].clone(),
            ],
            "one receipt per named process, in the order named"
        );
        assert_eq!(
            receipts[0].outcome,
            SessionObservedProcessOutcome::Observed {}
        );
        assert_eq!(receipts[1].outcome, SessionObservedProcessOutcome::NotFound);
        assert!(matches!(
            &receipts[2].outcome,
            SessionObservedProcessOutcome::NoLongerRetained { terminal_label, .. }
                if *terminal_label == lash_execution_contracts::process::RetiredProcessStatus::Completed
        ));
        assert!(
            registry
                .is_observer(&session_id, &ids["named"])
                .await
                .expect("read the named edge")
        );
        assert!(
            !registry
                .is_observer(&session_id, &ids["unnamed"])
                .await
                .expect("read the unnamed edge"),
            "an intent mints no edge the host did not name"
        );
        let observer_events = registry
            .full_event_window(&ids["named"], 0)
            .await
            .expect("read the observer audit events")
            .into_iter()
            .filter(|event| {
                event.kind() == lash_execution_contracts::process::ProcessEventKind::ObserverAdded
            })
            .count();
        assert_eq!(
            observer_events, 1,
            "the edge is added through the replay-keyed observer-event path"
        );
    }
    #[tokio::test]
    async fn a_selected_pruned_process_never_retargets_to_a_later_process() {
        let backend = sqlite_memory_process_store_set().await;
        let registry = backend.process_registry();
        let registration = lash_execution_contracts::testing::held_engine_registration(
            serde_json::Value::Null,
            lash_execution_contracts::process::ProcessProvenance::host(),
            lash_execution_contracts::process::Lifetime::Detached,
        );
        let first = registry
            .register_process(registration.clone())
            .await
            .expect("first run");
        let selected = first.id.clone();
        let terminal = registry
            .complete_process(
                &first.id,
                lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
                    crate::ToolCallOutput::success(serde_json::Value::Null),
                ),
                lash_execution_contracts::process::ProcessCompletionAuthority::workflow_key(
                    &first.id,
                ),
            )
            .await
            .expect("finish first run");
        crate::support::after_millisecond_tick(terminal.updated_at_ms).await;
        registry
            .prune_terminal_processes(
                terminal.updated_at_ms.saturating_add(1),
                None,
                lash_execution_contracts::process::ProjectionWatermark::NoProjector,
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
