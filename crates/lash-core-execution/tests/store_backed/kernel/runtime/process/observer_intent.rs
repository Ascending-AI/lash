mod tests {
    use crate::SessionId;
    use crate::plugin::{SessionObservedProcessOutcome, SessionObserverIntent};
    use crate::runtime::{SessionObserverIntentSource, reconcile_session_process_observer_intents};
    use crate::support::prelude::*;

    use crate::support::sqlite_memory_process_store_set;

    #[tokio::test]
    async fn noproc_receipts_preserve_missing_and_pruned_outcomes() {
        let backend = sqlite_memory_process_store_set().await;
        let registry = backend.process_registry();
        let registered = registry
            .register_process(crate::testing::held_engine_registration(
                serde_json::Value::Null,
                crate::ProcessProvenance::host(),
                crate::Lifetime::Detached,
            ))
            .await
            .expect("register process before pruning");
        let pruned = registry
            .complete_process(
                &registered.id,
                crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                    serde_json::Value::Null,
                )),
                crate::ProcessCompletionAuthority::workflow_key(&registered.id),
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
                .register_process(crate::testing::held_engine_registration(
                    serde_json::Value::Null,
                    crate::ProcessProvenance::host(),
                    crate::Lifetime::Detached,
                ))
                .await
                .expect("register an observer test process");
            ids.insert(label, registered.id);
        }
        let pruned = registry
            .complete_process(
                &ids["pruned"],
                crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                    serde_json::Value::Null,
                )),
                crate::ProcessCompletionAuthority::workflow_key(&ids["pruned"]),
            )
            .await
            .expect("complete the process before pruning");
        crate::support::after_millisecond_tick(pruned.updated_at_ms).await;
        registry
            .prune_terminal_processes(
                pruned.updated_at_ms.saturating_add(1),
                None,
                crate::ProjectionWatermark::NoProjector,
            )
            .await
            .expect("prune the terminal process");
        let session_id = SessionId::from("observer-child");

        let receipts = reconcile_session_process_observer_intents(
            Some(registry.as_ref()),
            &session_id,
            SessionObserverIntentSource::Unstored(vec![
                SessionObserverIntent::host_requested(ids["named"].clone()),
                SessionObserverIntent::host_requested(crate::ProcessId::fixture("missing-process")),
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
                crate::ProcessId::fixture("missing-process"),
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
                if *terminal_label == crate::RetiredProcessStatus::Completed
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
            .filter(|event| event.kind() == crate::ProcessEventKind::ObserverAdded)
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
        let registration = crate::testing::held_engine_registration(
            serde_json::Value::Null,
            crate::ProcessProvenance::host(),
            crate::Lifetime::Detached,
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
                crate::ProcessCompletionAuthority::workflow_key(&first.id),
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
