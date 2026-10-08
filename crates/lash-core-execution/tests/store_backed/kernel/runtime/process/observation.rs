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
        assert!(item.process.terminal());
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

    #[tokio::test]
    async fn observed_process_reports_terminal_status_and_error_messages() {
        let registry = memory_registry().await;
        let mut ids = std::collections::BTreeMap::new();
        for process_id in ["failed", "cancelled"] {
            let registered = registry
                .register_process(external_registration(process_id))
                .await
                .expect("register");
            ids.insert(process_id, registered.id.clone());
        }
        registry
            .complete_process(
                &ids["failed"],
                ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::failure(
                    crate::ToolFailure::runtime(
                        ToolFailureClass::External,
                        "boom",
                        "failed loudly",
                    ),
                )),
                crate::ProcessCompletionAuthority::workflow_key(&ids["failed"]),
            )
            .await
            .expect("fail process");
        registry
            .complete_process(
                &ids["cancelled"],
                ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::cancelled(
                    crate::ToolCancellation::runtime("cancelled intentionally"),
                )),
                crate::ProcessCompletionAuthority::workflow_key(&ids["cancelled"]),
            )
            .await
            .expect("cancel process");

        let observer = observer(Arc::clone(&registry));
        let failed = observer
            .process(&ids["failed"])
            .await
            .expect("read failed process")
            .expect("failed process");
        let cancelled = observer
            .process(&ids["cancelled"])
            .await
            .expect("read cancelled process")
            .expect("cancelled process");

        assert_eq!(failed.status_label(), "failed");
        assert!(failed.terminal());
        assert_eq!(failed.error.as_deref(), Some("failed loudly"));
        assert_eq!(cancelled.status_label(), "cancelled");
        assert!(cancelled.terminal());
        assert_eq!(cancelled.error.as_deref(), Some("cancelled intentionally"));

        // FIG-3094: a polling host reads the typed classification beside the
        // display string instead of matching the prose.
        assert_eq!(
            failed.error_code,
            Some(crate::ObservedProcessFailure::Failed {
                class: ToolFailureClass::External,
                code: "boom".to_string(),
            })
        );
        assert_eq!(
            cancelled.error_code,
            Some(crate::ObservedProcessFailure::Cancelled { origin: None })
        );
    }
}
