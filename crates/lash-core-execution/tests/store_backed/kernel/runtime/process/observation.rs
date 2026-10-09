mod tests {
    use crate::support::prelude::*;
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;

    use crate::runtime::process::{
        ObservedWorkItemState, ProcessAwaitOutput, ProcessListFilter, ProcessRegistryFaults,
        ProcessWorkObserver,
    };
    use crate::{ProcessId, ProcessRegistry, SessionId};
    use crate::{
        ProcessObserverBy, ProcessProvenance, ProcessRegistration, SessionScope, ToolFailureClass,
    };

    async fn memory_registry() -> Arc<dyn ProcessRegistry> {
        crate::support::sqlite_memory_process_store_set()
            .await
            .process_registry()
    }

    fn observer(registry: Arc<dyn ProcessRegistry>) -> ProcessWorkObserver {
        ProcessWorkObserver::new(registry)
    }

    fn external_registration(label: &str) -> ProcessRegistration {
        crate::testing::held_engine_registration(
            json!({ "label": label }),
            ProcessProvenance::host(),
            crate::Lifetime::Detached,
        )
    }

    async fn register_visible(
        registry: &Arc<dyn ProcessRegistry>,
        scope: &SessionScope,
        registration: ProcessRegistration,
    ) -> ProcessId {
        let process_id = registry
            .register_process(registration)
            .await
            .expect("register process")
            .id;
        registry
            .add_observer(
                &scope.session_id,
                &process_id,
                ProcessObserverBy::host("observation-test"),
            )
            .await
            .expect("add process observer");
        process_id
    }

    #[tokio::test]
    async fn work_item_retry_converges_after_a_record_event_tail_disagreement() {
        let registry = memory_registry().await;
        let retry_converges_record = registry
            .register_process(external_registration("Retry converges"))
            .await
            .expect("register process");
        let process_id = retry_converges_record.id.clone();
        let stale_record = registry
            .get_process(&process_id)
            .await
            .expect("read stale record")
            .expect("retained process");
        registry
            .complete_process(
                &process_id,
                ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(json!({}))),
                crate::ProcessCompletionAuthority::workflow_key(&process_id),
            )
            .await
            .expect("complete process between record and event-tail reads");

        let item = crate::work_item_from_record(
            &observer(Arc::clone(&registry) as Arc<dyn ProcessRegistry>),
            stale_record,
        )
        .await
        .expect("retry observation");

        assert_eq!(item.state(), ObservedWorkItemState::Coherent);
        assert_eq!(
            item.process.last_event_sequence,
            item.event_tail_sequence(),
            "the retry must pair the refreshed terminal record with its event tail"
        );
        assert!(item.process.terminal().is_some());
    }

    #[tokio::test]
    async fn work_item_retry_surfaces_typed_mismatch_when_bound_is_exhausted() {
        let registry = Arc::new(ProcessRegistryFaults::new(memory_registry().await));
        let retry_exhausted_record = registry
            .register_process(external_registration("Retry exhausted"))
            .await
            .expect("register process");
        let process_id = retry_exhausted_record.id.clone();
        let stale_record = registry
            .get_process(&process_id)
            .await
            .expect("read stale record")
            .expect("retained process");
        registry
            .complete_process(
                &process_id,
                ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(json!({}))),
                crate::ProcessCompletionAuthority::workflow_key(&process_id),
            )
            .await
            .expect("complete process between record and event-tail reads");
        registry.set_process_read_override(stale_record.clone());

        let item = crate::work_item_from_record(
            &observer(Arc::clone(&registry) as Arc<dyn ProcessRegistry>),
            stale_record.clone(),
        )
        .await
        .expect("bounded observation");

        assert_eq!(
            item.state(),
            ObservedWorkItemState::EventTailMismatch {
                record_sequence: stale_record.last_event_sequence,
                event_tail_sequence: item.event_tail_sequence(),
            }
        );
        assert!(item.has_mispaired_event_tail());
        assert_ne!(
            item.process.last_event_sequence,
            item.event_tail_sequence(),
            "the exhausted retry must expose rather than hide the torn snapshot"
        );
    }

    #[tokio::test]
    async fn runtime_snapshot_keeps_orphaned_processes_after_session_deletion() {
        let registry = memory_registry().await;
        let surviving_process_id = register_visible(
            &registry,
            &SessionScope::new("deleted-session"),
            external_registration("Survivor"),
        )
        .await;

        let report = registry
            .delete_session_process_state(&SessionId::from("deleted-session"))
            .await
            .expect("delete session process edges");
        assert_eq!(report.removed_observer_count, 1);
        assert!(
            observer(Arc::clone(&registry))
                .snapshot_for_session("deleted-session")
                .await
                .expect("deleted session snapshot")
                .items
                .is_empty()
        );

        let runtime_items = observer(registry)
            .snapshot_all(&ProcessListFilter {
                status: crate::ProcessStatusFilter::Any,
                ..ProcessListFilter::default()
            })
            .await
            .expect("runtime process snapshot");
        assert_eq!(runtime_items.len(), 1);
        assert_eq!(
            runtime_items[0].process.process_id,
            surviving_process_id.clone()
        );
    }

    #[tokio::test]
    async fn snapshot_for_session_sorts_work_by_updated_then_created_descending() {
        let registry = memory_registry().await;
        let scope = SessionScope::new("sort");
        let older_id = register_visible(&registry, &scope, external_registration("Older")).await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        let newer_id = register_visible(&registry, &scope, external_registration("Newer")).await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        registry
            .request_process_cancel(
                &older_id,
                crate::CancelOrigin::OperatorRequested,
                "actor:observation-test".to_string(),
                None,
            )
            .await
            .expect("update older process");

        let snapshot = observer(Arc::clone(&registry))
            .snapshot_for_session("sort")
            .await
            .expect("snapshot");

        assert_eq!(snapshot.visible_processes, vec![older_id, newer_id]);
    }

    /// A process's observed state is its canonical lifecycle state: an
    /// ended process is read with its typed outcome (the success output,
    /// the failure, or the abandonment evidence) and a waiting one with its
    /// wait, in the one read (FIG-5564).
    #[tokio::test]
    async fn an_observed_process_carries_its_typed_outcome_or_its_wait_in_one_read() {
        let stores = crate::support::sqlite_memory_process_store_set().await;
        let registry: Arc<dyn ProcessRegistry> = stores.process_registry();
        let observer = observer(Arc::clone(&registry));
        let actor_observer = observer
            .clone()
            .with_actor_parks(Arc::new(stores.durable_store()));
        let failure =
            crate::ToolFailure::runtime(ToolFailureClass::External, "boom", "failed loudly");
        let evidence = crate::AbandonEvidence {
            writer: crate::AbandonWriter::Producer,
            owner: None,
            epoch_ms: 7,
        };
        let outcomes = [
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                json!({ "answer": 42 }),
            )),
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::failure(failure)),
            ProcessAwaitOutput::Abandoned {
                evidence: Box::new(evidence),
                control: None,
            },
        ];
        for (index, outcome) in outcomes.into_iter().enumerate() {
            let id = registry
                .register_process(external_registration(&format!("terminal-{index}")))
                .await
                .expect("register")
                .id;
            registry
                .complete_process(
                    &id,
                    outcome.clone(),
                    crate::ProcessCompletionAuthority::workflow_key(&id),
                )
                .await
                .expect("end the process");
            let seen = observer
                .process(&id)
                .await
                .expect("read the ended process")
                .expect("the ended process is retained");
            assert_eq!(seen.park, crate::ProcessParkState::NotRead);
            assert_eq!(
                actor_observer
                    .process(&id)
                    .await
                    .expect("read actor park")
                    .expect("retained process")
                    .park,
                crate::ProcessParkState::NotParked
            );
            let expected =
                crate::ProcessTerminal::try_from(outcome).expect("the outcome is terminal");
            assert_eq!(
                seen.terminal(),
                Some(&expected),
                "an ended process is observed with the outcome it ended in"
            );
            assert_eq!(seen.status(), crate::ProcessStatus::from(expected.status()));
        }

        let id = registry
            .register_process(external_registration("waiting"))
            .await
            .expect("register")
            .id;
        let authority = crate::ProcessExecutionWriteAuthority::invocation(&id, "observation-law")
            .bind_attempt(1);
        registry
            .record_first_started_with_authority(
                &id,
                authority.invocation_started().expect("an invocation start"),
                &authority,
            )
            .await
            .expect("start the process");
        let wait = crate::WaitState {
            kind: crate::WaitKind::Call {
                call_id: crate::ToolCallId::fixture("observation-law"),
                tool_id: crate::ToolId::from("observation_law"),
            },
            since_ms: 3,
            site: None,
        };
        registry
            .set_process_wait_with_authority(&id, wait.clone(), Vec::new(), &authority)
            .await
            .expect("enter the wait");
        let seen = observer
            .process(&id)
            .await
            .expect("read the waiting process")
            .expect("the waiting process is retained");
        assert_eq!(seen.park, crate::ProcessParkState::NotRead);
        assert_eq!(
            actor_observer
                .process(&id)
                .await
                .expect("read actor park")
                .expect("retained process")
                .park,
            crate::ProcessParkState::NotParked
        );
        assert_eq!(
            seen.lifecycle,
            crate::ProcessLifecycleState::Waiting {
                waits: crate::ProcessWaits::new(wait)
            },
            "a waiting process is observed with what it waits on"
        );
    }

    /// An ended process keeps the time of the committed fact that ended it:
    /// a fact appended afterwards moves the row's update time and leaves the
    /// terminal time alone (FIG-5564).
    #[tokio::test]
    async fn a_fact_after_the_terminal_leaves_the_terminal_time_unchanged() {
        let registry = memory_registry().await;
        let observer = observer(Arc::clone(&registry));
        let id = registry
            .register_process(external_registration("ended"))
            .await
            .expect("register")
            .id;
        registry
            .complete_process(
                &id,
                ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(json!({}))),
                crate::ProcessCompletionAuthority::workflow_key(&id),
            )
            .await
            .expect("end the process");
        let terminal_fact = registry
            .recent_events(&id, 1)
            .await
            .expect("read the terminal fact")
            .pop()
            .expect("the terminal fact is retained");
        assert!(terminal_fact.fact.terminal().is_some());
        let ended = observer
            .process(&id)
            .await
            .expect("read the ended process")
            .expect("the ended process is retained");
        assert_eq!(
            ended.lifecycle.terminal_at_ms(),
            Some(terminal_fact.occurred_at)
        );

        tokio::time::sleep(Duration::from_millis(20)).await;
        registry
            .add_observer(
                &SessionId::from("late-observer"),
                &id,
                ProcessObserverBy::host("observation-test"),
            )
            .await
            .expect("append a fact after the terminal");
        let later = observer
            .process(&id)
            .await
            .expect("read the ended process again")
            .expect("the ended process is retained");
        assert!(
            later.last_event_sequence > ended.last_event_sequence
                && later.updated_at_ms > terminal_fact.occurred_at,
            "the later fact folded into the row at a later time"
        );
        assert_eq!(
            later.lifecycle, ended.lifecycle,
            "a fact after the terminal changes neither the outcome nor its time"
        );
    }
}
