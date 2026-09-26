mod tests {
    use crate::support::prelude::*;
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;

    use crate::runtime::process::{
        ObservedWorkItemState, ProcessAwaitOutput, ProcessInput, ProcessListFilter,
        ProcessRegistryFaults, ProcessWorkObserver, RecoveryContract, WaitState,
    };
    use crate::{
        InputItem, PluginOptions, PreparedToolCall, ProcessEventAppendRequest,
        ProcessExecutionEnvRef, ProcessIdentity, ProcessObserverBy, ProcessProvenance,
        ProcessRegistration, SessionCreateRequest, SessionScope, SessionStartPoint,
        SubagentSessionContext, ToolFailureClass, ToolOutputContract, TurnInput, WaitKind,
    };
    use crate::{ProcessId, ProcessRegistry, SessionId};

    async fn memory_registry() -> Arc<dyn ProcessRegistry> {
        crate::support::memory_store_set().await.process_registry()
    }

    fn observer(registry: Arc<dyn ProcessRegistry>) -> ProcessWorkObserver {
        ProcessWorkObserver::new(registry)
    }

    fn external_registration(process_id: &ProcessId, label: &str) -> ProcessRegistration {
        ProcessRegistration::new(
            process_id,
            ProcessInput::External {
                metadata: json!({ "label": label }),
            },
            RecoveryContract::ExternallyOwned,
            ProcessProvenance::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        )
    }

    async fn register_visible(
        registry: &Arc<dyn ProcessRegistry>,
        scope: &SessionScope,
        registration: ProcessRegistration,
    ) {
        let process_id = registration.id.clone();
        registry
            .register_process(registration)
            .await
            .expect("register process");
        registry
            .add_observer(
                &scope.session_id,
                &process_id,
                ProcessObserverBy::host("observation-test"),
            )
            .await
            .expect("add process observer");
    }

    #[tokio::test]
    async fn snapshot_for_session_reads_observed_processes_and_events_as_epoch_ms() {
        let registry = memory_registry().await;
        let visible_scope = SessionScope::new("visible");
        register_visible(
            &registry,
            &visible_scope,
            external_registration(&ProcessId::from("visible-process"), "Visible"),
        )
        .await;
        register_visible(
            &registry,
            &SessionScope::new("other"),
            external_registration(&ProcessId::from("hidden-process"), "Hidden"),
        )
        .await;
        registry
            .append_event(
                &ProcessId::from("visible-process"),
                ProcessEventAppendRequest::cancel_requested(
                    &registry
                        .resolve_process_ref(&ProcessId::from("visible-process"))
                        .await
                        .expect("retained observed target"),
                    &crate::CancelRequest::new(
                        crate::CancelOrigin::OperatorRequested,
                        "actor:observation-test",
                        11,
                    ),
                ),
            )
            .await
            .expect("append event");

        let snapshot = observer(Arc::clone(&registry))
            .snapshot_for_session("visible")
            .await
            .expect("snapshot");

        assert_eq!(snapshot.session_id, "visible");
        assert_eq!(
            snapshot.visible_processes,
            vec![crate::ProcessRef::new(
                "visible-process",
                snapshot.items[0].process.incarnation,
            )]
        );
        assert_eq!(snapshot.items.len(), 1);
        assert_eq!(snapshot.items[0].events.len(), 2);
        assert_eq!(
            snapshot.items[0].process.last_event_sequence,
            snapshot.items[0].event_tail_sequence(),
            "a stable observation must pair record and event-tail positions"
        );
        assert!(!snapshot.items[0].has_mispaired_event_tail());
        assert!(
            snapshot.items[0]
                .events
                .iter()
                .any(|event| event.event_type == "process.observer_added"),
            "observer membership changes are part of the durable audit tail"
        );
        assert_eq!(
            snapshot.items[0].process.cancel_request,
            Some(crate::CancelRequest::new(
                crate::CancelOrigin::OperatorRequested,
                "actor:observation-test",
                11
            )),
            "the observation carries the accepted cancellation fact"
        );
        let cancelled = snapshot.items[0]
            .events
            .iter()
            .find(|event| event.event_type == "process.cancel_requested")
            .expect("cancel event");
        assert!(
            cancelled.occurred_at_ms > 0,
            "event timestamps are epoch milliseconds"
        );
    }

    #[tokio::test]
    async fn work_item_retry_converges_after_a_record_event_tail_disagreement() {
        let registry = memory_registry().await;
        let process_id = ProcessId::from("retry-converges");
        registry
            .register_process(external_registration(&process_id, "Retry converges"))
            .await
            .expect("register process");
        let stale_record = registry
            .get_process(&process_id)
            .await
            .expect("read stale record")
            .expect("retained process");
        registry
            .complete_process(
                &process_id,
                ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(json!({}))),
                crate::ProcessCompletionAuthority::external_owner(),
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
        let process_id = ProcessId::from("retry-exhausted");
        registry
            .register_process(external_registration(&process_id, "Retry exhausted"))
            .await
            .expect("register process");
        let stale_record = registry
            .get_process(&process_id)
            .await
            .expect("read stale record")
            .expect("retained process");
        registry
            .complete_process(
                &process_id,
                ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(json!({}))),
                crate::ProcessCompletionAuthority::external_owner(),
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
        register_visible(
            &registry,
            &SessionScope::new("deleted-session"),
            external_registration(&ProcessId::from("surviving-process"), "Survivor"),
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
        assert_eq!(runtime_items[0].process.process_id, "surviving-process");
    }

    #[tokio::test]
    async fn list_batches_lease_reads_without_changing_mixed_results() {
        let registry = Arc::new(ProcessRegistryFaults::new(memory_registry().await));
        for process_id in ["batch-leased", "batch-unleased", "batch-terminal"] {
            registry
                .register_process(external_registration(
                    &ProcessId::from(process_id),
                    process_id,
                ))
                .await
                .expect("register batch observation fixture");
        }
        registry
            .claim_process_lease(
                &ProcessId::from("batch-leased"),
                &crate::LeaseOwnerIdentity::opaque("observer", "one"),
                60_000,
            )
            .await
            .expect("claim observed lease")
            .acquired()
            .expect("observed lease acquired");
        registry
            .complete_process(
                &ProcessId::from("batch-terminal"),
                ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(json!({}))),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete observed terminal process");

        let watched = crate::facade_support::watch_process_registry(
            Arc::clone(&registry) as Arc<dyn ProcessRegistry>
        );
        let observed = observer(Arc::clone(watched.registry()))
            .list(&ProcessListFilter {
                status: crate::ProcessStatusFilter::Any,
                ..ProcessListFilter::default()
            })
            .await
            .expect("observe mixed records");

        assert_eq!(observed.len(), 3);
        assert_eq!(
            observed
                .iter()
                .filter(|process| process.lease_holder.is_some())
                .count(),
            1
        );
        assert_eq!(
            observed.iter().filter(|process| process.terminal()).count(),
            1
        );
        assert_eq!(registry.lease_batch_reads(), 1);
        assert_eq!(registry.lease_point_reads(), 0);
    }

    #[tokio::test]
    async fn snapshot_for_session_sorts_work_by_updated_then_created_descending() {
        let registry = memory_registry().await;
        let scope = SessionScope::new("sort");
        register_visible(
            &registry,
            &scope,
            external_registration(&ProcessId::from("older"), "Older"),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        register_visible(
            &registry,
            &scope,
            external_registration(&ProcessId::from("newer"), "Newer"),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        registry
            .append_event(
                &ProcessId::from("older"),
                ProcessEventAppendRequest::cancel_requested(
                    &registry
                        .resolve_process_ref(&ProcessId::from("older"))
                        .await
                        .expect("retained observed target"),
                    &crate::CancelRequest::new(
                        crate::CancelOrigin::OperatorRequested,
                        "actor:observation-test",
                        11,
                    ),
                ),
            )
            .await
            .expect("update older process");

        let snapshot = observer(Arc::clone(&registry))
            .snapshot_for_session("sort")
            .await
            .expect("snapshot");

        assert_eq!(
            snapshot
                .visible_processes
                .iter()
                .map(|process| process.process_id.as_str())
                .collect::<Vec<_>>(),
            vec!["older", "newer"]
        );
    }

    #[tokio::test]
    async fn observed_process_reports_terminal_status_and_error_messages() {
        let registry = memory_registry().await;
        for process_id in ["failed", "cancelled"] {
            registry
                .register_process(external_registration(
                    &ProcessId::from(process_id),
                    process_id,
                ))
                .await
                .expect("register");
        }
        registry
            .complete_process(
                &ProcessId::from("failed"),
                ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::failure(
                    crate::ToolFailure::runtime(
                        ToolFailureClass::External,
                        "boom",
                        "failed loudly",
                    ),
                )),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("fail process");
        registry
            .complete_process(
                &ProcessId::from("cancelled"),
                ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::cancelled(
                    crate::ToolCancellation::runtime("cancelled intentionally"),
                )),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("cancel process");

        let observer = observer(Arc::clone(&registry));
        let failed = observer
            .process(&ProcessId::from("failed"))
            .await
            .expect("read failed process")
            .expect("failed process");
        let cancelled = observer
            .process(&ProcessId::from("cancelled"))
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

    #[tokio::test]
    async fn observed_process_exposes_current_wait_state() {
        let registry = memory_registry().await;
        let scope = SessionScope::new("wait");
        register_visible(
            &registry,
            &scope,
            external_registration(&ProcessId::from("waiting-process"), "Waiting"),
        )
        .await;
        let wait = WaitState {
            since_ms: 1234,
            kind: WaitKind::Signal {
                name: "ready".to_string(),
                event_type: "signal.ready".to_string(),
                key: "process:waiting-process:signal.ready:1".to_string(),
                ordinal: 1,
            },
        };
        registry
            .set_process_wait(&ProcessId::from("waiting-process"), wait.clone())
            .await
            .expect("set wait");

        let observer = observer(Arc::clone(&registry));
        let observed = observer
            .process(&ProcessId::from("waiting-process"))
            .await
            .expect("read waiting process")
            .expect("waiting process");
        let snapshot = observer
            .snapshot_for_session("wait")
            .await
            .expect("snapshot");

        assert_eq!(observed.wait, Some(wait.clone()));
        assert_eq!(snapshot.items.len(), 1);
        assert_eq!(snapshot.items[0].process.wait, Some(wait));
    }

    #[tokio::test]
    async fn snapshot_for_session_prefers_typed_labels_and_extracts_child_session_id() {
        let registry = memory_registry().await;
        let scope = SessionScope::new("labels");
        let mut child_request = SessionCreateRequest::child_session(
            "labels",
            SessionStartPoint::Empty,
            PluginOptions::default(),
        )
        .with_session_id("child-session");
        child_request.subagent = Some(SubagentSessionContext {
            parent_session_id: SessionId::from("labels"),
            capability: "researcher".to_string(),
            depth: 1,
            max_depth: 4,
        });
        let cases = [
            (
                "tool",
                ProcessInput::ToolCall {
                    call: PreparedToolCall::from_parts(
                        "call-1",
                        "tool:files.read",
                        "files.read",
                        json!({}),
                        None,
                        serde_json::Value::Null,
                    ),
                },
                "tool",
                "files.read",
                None,
            ),
            (
                "engine",
                ProcessInput::Engine {
                    kind: "test-engine".to_string(),
                    payload: json!({}),
                },
                "test-engine",
                "remember",
                None,
            ),
            (
                "session",
                ProcessInput::SessionTurn {
                    definition_key: "observation-test-session-turn:v1".to_string(),
                    create_request: Box::new(child_request),
                    turn_input: Box::new(TurnInput::items([InputItem::text("run child")])),
                    output_contract: ToolOutputContract::Static,
                },
                "session_turn",
                "researcher",
                Some("child-session"),
            ),
            (
                "external",
                ProcessInput::External {
                    metadata: json!({ "label": "external job" }),
                },
                "external",
                "external job",
                None,
            ),
        ];
        for (process_id, input, kind, label, _child_session_id) in cases {
            let needs_env = matches!(
                input,
                ProcessInput::ToolCall { .. } | ProcessInput::Engine { .. }
            );
            let disposition = match input {
                ProcessInput::External { .. } => RecoveryContract::ExternallyOwned,
                _ => RecoveryContract::Rerunnable,
            };
            let mut registration = ProcessRegistration::new(
                process_id,
                input,
                disposition,
                ProcessProvenance::host(),
                crate::ProcessLifecyclePolicy::new(
                    crate::ParentScope::Host,
                    crate::OnParentEnd::Abandon,
                ),
            )
            .with_admitted_identity(crate::AdmittedProcessIdentity::for_testing(
                ProcessIdentity::labelled(kind, Some(label.to_string())),
            ));
            if needs_env {
                registration = registration.with_execution_env_ref(Some(
                    ProcessExecutionEnvRef::new(format!("process-env:test:{process_id}")),
                ));
            }
            register_visible(&registry, &scope, registration).await;
        }

        let snapshot = observer(Arc::clone(&registry))
            .snapshot_for_session("labels")
            .await
            .expect("snapshot");
        let by_id = snapshot
            .items
            .iter()
            .map(|item| (item.process.process_id.as_str(), item))
            .collect::<std::collections::BTreeMap<_, _>>();

        assert_eq!(by_id["tool"].label(), "files.read");
        assert_eq!(by_id["engine"].label(), "remember");
        assert_eq!(by_id["engine"].process.kind(), "test-engine");
        assert_eq!(by_id["session"].label(), "researcher");
        assert_eq!(
            by_id["session"].process.child_session_id.as_deref(),
            Some("child-session")
        );
        assert_eq!(by_id["external"].label(), "external job");
    }

    #[tokio::test]
    async fn observed_process_missing_lookup_returns_none() {
        let registry = memory_registry().await;

        assert!(
            observer(registry)
                .process(&ProcessId::from("missing"))
                .await
                .expect("read missing process")
                .is_none()
        );
    }
}
