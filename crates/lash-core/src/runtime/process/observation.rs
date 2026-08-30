use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::plugin::PluginError;

use super::events::{ProcessAwaitOutput, ProcessEvent};
use super::model::{
    AbandonRequest, ProcessExecutionEnvRef, ProcessExternalRef, ProcessId, ProcessIdentity,
    ProcessInput, ProcessLease, ProcessListFilter, ProcessOriginator, ProcessRecord,
    ProcessStarted, ProcessStatus, RecoveryContract, SessionScope, WaitState,
};
use super::registry::ProcessRegistry;
use super::time::epoch_ms_from_system_time;

#[derive(Clone)]
pub struct ProcessWorkObserver {
    registry: Arc<dyn ProcessRegistry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessWorkSnapshot {
    pub session_id: String,
    pub visible_process_ids: Vec<ProcessId>,
    pub items: Vec<ObservedWorkItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObservedWorkItem {
    pub process: ObservedProcess,
    pub events: Vec<ObservedProcessEvent>,
    pub kind: String,
    pub label: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObservedProcess {
    pub process_id: ProcessId,
    pub graph_key: String,
    pub kind: String,
    pub lifecycle: ProcessStatus,
    pub identity: ProcessIdentity,
    pub status_label: String,
    pub terminal: bool,
    /// Declared recovery contract (ADR 0019). Raw fact; hosts classify.
    pub disposition: RecoveryContract,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// Durable execution-started fact, if the row has begun executing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_started: Option<ProcessStarted>,
    /// Current lease holder identity, if the row is leased (ADR 0019). Raw
    /// fact for host-side staleness classification — no derived "stuck" verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_holder: Option<crate::LeaseOwnerIdentity>,
    /// Current lease expiry, paired with `lease_holder`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at_ms: Option<u64>,
    /// Pending Abandon Request the sweep reconciles once the lease lapses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandon_request: Option<AbandonRequest>,
    pub input: ProcessInput,
    pub originator: ProcessOriginator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_ref: Option<ProcessExecutionEnvRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<crate::CausalRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<ProcessExternalRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<WaitState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<String>,
    pub label: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObservedProcessEvent {
    pub sequence: u64,
    pub event_type: String,
    pub occurred_at_ms: u64,
    pub payload: serde_json::Value,
}

/// Per-item event tail in session snapshots. Snapshots are polled by
/// docks/UIs, so per-poll cost must stay bounded instead of growing with a
/// process's full event history; detail views page through `events_after`
/// with a cursor.
pub const SNAPSHOT_EVENT_TAIL: usize = 32;

impl ProcessWorkObserver {
    pub fn new(registry: Arc<dyn ProcessRegistry>) -> Self {
        Self { registry }
    }

    pub async fn snapshot_for_session(
        &self,
        session_id: impl Into<String>,
    ) -> Result<ProcessWorkSnapshot, PluginError> {
        let session_id = session_id.into();
        let entries = self.registry.list_observed_by(&session_id).await?;
        let mut items = Vec::new();
        for record in entries {
            items.push(self.work_item_from_record(record).await?);
        }
        items.sort_by(|left, right| {
            right
                .process
                .updated_at_ms
                .cmp(&left.process.updated_at_ms)
                .then_with(|| right.process.created_at_ms.cmp(&left.process.created_at_ms))
                .then_with(|| left.process.process_id.cmp(&right.process.process_id))
        });
        let visible_process_ids = items
            .iter()
            .map(|item| item.process.process_id.clone())
            .collect();
        Ok(ProcessWorkSnapshot {
            session_id,
            visible_process_ids,
            items,
        })
    }

    /// Snapshot every process matching `filter`, including the bounded event
    /// tail used by host work rails. Unlike [`Self::snapshot_for_session`],
    /// this is the runtime-wide observation surface: it does not depend on a
    /// session observer edge and therefore continues to expose processes whose
    /// originating session has been deleted.
    /// Because observer edges are bypassed, the host must authorize access; routing
    /// identity is not authorization.
    pub async fn snapshot_all(
        &self,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ObservedWorkItem>, PluginError> {
        let records = self.registry.list_processes(filter).await?;
        let mut items = Vec::with_capacity(records.len());
        for record in records {
            items.push(self.work_item_from_record(record).await?);
        }
        items.sort_by(|left, right| {
            right
                .process
                .updated_at_ms
                .cmp(&left.process.updated_at_ms)
                .then_with(|| right.process.created_at_ms.cmp(&left.process.created_at_ms))
                .then_with(|| left.process.process_id.cmp(&right.process.process_id))
        });
        Ok(items)
    }

    async fn work_item_from_record(
        &self,
        record: ProcessRecord,
    ) -> Result<ObservedWorkItem, PluginError> {
        let events = self
            .registry
            .recent_events(&record.id, SNAPSHOT_EVENT_TAIL)
            .await?
            .into_iter()
            .map(ObservedProcessEvent::from)
            .collect();
        let lease = self.registry.get_process_lease(&record.id).await?;
        let process = ObservedProcess::from_record(record, lease);
        let kind = process.identity.kind.clone();
        let label = process
            .identity
            .label
            .clone()
            .unwrap_or_else(|| kind.clone());
        Ok(ObservedWorkItem {
            process,
            events,
            kind,
            label,
        })
    }

    pub async fn process(&self, process_id: &str) -> Result<Option<ObservedProcess>, PluginError> {
        let Some(record) = self.registry.get_process(process_id).await? else {
            return Ok(None);
        };
        let lease = self.registry.get_process_lease(process_id).await?;
        Ok(Some(ObservedProcess::from_record(record, lease)))
    }

    pub async fn list(
        &self,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ObservedProcess>, PluginError> {
        let records = self.registry.list_processes(filter).await?;
        self.observe_records(records).await
    }

    /// List processes a session may address — the observer filter. A process is
    /// visible here only if `scope.session_id` has an observer edge. This is the
    /// single home for the observer-scoped view; the session facade sugar is a thin
    /// caller of this method, never a parallel implementation.
    pub async fn list_observed_by(
        &self,
        scope: &SessionScope,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ObservedProcess>, PluginError> {
        let entries = self.registry.list_observed_by(&scope.session_id).await?;
        let records = entries
            .into_iter()
            .filter(|record| filter.matches_record(record))
            .collect::<Vec<_>>();
        self.observe_records(records).await
    }

    /// List processes a session originated — the provenance filter (ADR 0019 /
    /// process design grill). "Originated by" is the lineage lens, distinct from
    /// the observer lens: a process matches when its recorded originator is a
    /// session whose id equals `scope.session_id` (and its agent frame, when
    /// `scope` names one), regardless of which sessions currently observe it.
    pub async fn list_originated_by(
        &self,
        scope: &SessionScope,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ObservedProcess>, PluginError> {
        let records = self
            .registry
            .list_processes(filter)
            .await?
            .into_iter()
            .filter(|record| originator_matches(&record.provenance.originator, scope))
            .collect::<Vec<_>>();
        self.observe_records(records).await
    }

    async fn observe_records(
        &self,
        records: Vec<ProcessRecord>,
    ) -> Result<Vec<ObservedProcess>, PluginError> {
        let process_ids = records
            .iter()
            .map(|record| record.id.clone())
            .collect::<Vec<_>>();
        let leases = self.registry.get_process_leases(&process_ids).await?;
        if records.len() != leases.len() {
            return Err(PluginError::Session(format!(
                "process registry batch lease read returned {} rows for {} process ids",
                leases.len(),
                records.len()
            )));
        }
        Ok(records
            .into_iter()
            .zip(leases)
            .map(|(record, lease)| ObservedProcess::from_record(record, lease))
            .collect())
    }

    pub async fn events_after(
        &self,
        process_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<ObservedProcessEvent>, PluginError> {
        Ok(self
            .registry
            .events_after(process_id, after_sequence)
            .await?
            .into_iter()
            .map(ObservedProcessEvent::from)
            .collect())
    }
}

impl ObservedProcess {
    /// Build a read-side view of a process. `lease` is the current lease row (if
    /// any), read separately so the observer exposes holder identity and expiry
    /// as raw facts — no derived "stuck" classification (ADR 0019).
    fn from_record(record: ProcessRecord, lease: Option<ProcessLease>) -> Self {
        let lifecycle = record.status;
        let input = record.input.as_ref().clone();
        let identity = record.identity;
        let kind = identity.kind.clone();
        let label = identity.label.clone().unwrap_or_else(|| kind.clone());
        let process_id = record.id;
        let (lease_holder, lease_expires_at_ms) = match lease {
            Some(lease) => (Some(lease.owner), Some(lease.expires_at_epoch_ms)),
            None => (None, None),
        };
        Self {
            graph_key: format!("process:{process_id}"),
            process_id,
            kind,
            lifecycle,
            identity,
            status_label: lifecycle.label().to_string(),
            terminal: lifecycle.is_terminal(),
            disposition: record.disposition,
            error: terminal_error(record.outcome.as_ref()),
            created_at_ms: record.created_at_ms,
            updated_at_ms: record.updated_at_ms,
            first_started: record.first_started.map(|started| *started),
            lease_holder,
            lease_expires_at_ms,
            abandon_request: record.abandon_request.map(|request| *request),
            originator: record.provenance.originator,
            env_ref: record.env_ref,
            caused_by: record.provenance.caused_by,
            external_ref: record.external_ref,
            wait: record.wait,
            child_session_id: child_session_id(&input),
            input,
            label,
        }
    }
}

impl From<ProcessEvent> for ObservedProcessEvent {
    fn from(event: ProcessEvent) -> Self {
        Self {
            sequence: event.sequence,
            event_type: event.event_type,
            occurred_at_ms: epoch_ms_from_system_time(event.occurred_at),
            payload: event.payload,
        }
    }
}

fn terminal_error(outcome: Option<&ProcessAwaitOutput>) -> Option<String> {
    match outcome? {
        ProcessAwaitOutput::Settled { output } => match &output.outcome {
            crate::ToolCallOutcome::Failure(failure) => Some(failure.message.clone()),
            crate::ToolCallOutcome::Cancelled(cancellation) => Some(cancellation.message.clone()),
            crate::ToolCallOutcome::Success(_) => None,
        },
        // Abandonment is not a reported failure; the status label conveys it and
        // the evidence rides the terminal event. No derived error string here.
        ProcessAwaitOutput::Abandoned { .. } | ProcessAwaitOutput::NoLongerRetained { .. } => None,
    }
}

fn child_session_id(input: &ProcessInput) -> Option<String> {
    match input {
        ProcessInput::SessionTurn { create_request, .. } => create_request.session_id.clone(),
        ProcessInput::ToolCall { .. }
        | ProcessInput::Engine { .. }
        | ProcessInput::External { .. } => None,
    }
}

/// Whether `originator` names the session identified by `scope`.
fn originator_matches(originator: &ProcessOriginator, scope: &SessionScope) -> bool {
    match originator {
        ProcessOriginator::Host { .. } => false,
        ProcessOriginator::Session { session_id, .. } => session_id == &scope.session_id,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::{
        InputItem, PluginOptions, PreparedToolCall, ProcessEventAppendRequest,
        ProcessExecutionEnvRef, ProcessIdentity, ProcessObserverBy, ProcessProvenance,
        ProcessRegistration, SessionCreateRequest, SessionScope, SessionStartPoint,
        SubagentSessionContext, TestProcessRegistryWriteExt, ToolFailureClass, ToolOutputContract,
        TurnInput, WaitKind,
    };

    fn observer(registry: Arc<dyn ProcessRegistry>) -> ProcessWorkObserver {
        ProcessWorkObserver::new(registry)
    }

    fn external_registration(process_id: &str, label: &str) -> ProcessRegistration {
        ProcessRegistration::new(
            process_id,
            ProcessInput::External {
                metadata: json!({ "label": label }),
            },
            RecoveryContract::ExternallyOwned,
            ProcessProvenance::host(),
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
        let registry =
            Arc::new(super::super::TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let visible_scope = SessionScope::new("visible");
        register_visible(
            &registry,
            &visible_scope,
            external_registration("visible-process", "Visible"),
        )
        .await;
        register_visible(
            &registry,
            &SessionScope::new("other"),
            external_registration("hidden-process", "Hidden"),
        )
        .await;
        registry
            .append_event(
                "visible-process",
                ProcessEventAppendRequest::new("process.cancel_requested", json!({"why": "test"}))
                    .with_replay_key("visible-process:cancel-requested"),
            )
            .await
            .expect("append event");

        let snapshot = observer(Arc::clone(&registry))
            .snapshot_for_session("visible")
            .await
            .expect("snapshot");

        assert_eq!(snapshot.session_id, "visible");
        assert_eq!(snapshot.visible_process_ids, vec!["visible-process"]);
        assert_eq!(snapshot.items.len(), 1);
        assert_eq!(snapshot.items[0].events.len(), 2);
        assert!(
            snapshot.items[0]
                .events
                .iter()
                .any(|event| event.event_type == "process.observer_added"),
            "observer membership changes are part of the durable audit tail"
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
    async fn runtime_snapshot_keeps_orphaned_processes_after_session_deletion() {
        let registry =
            Arc::new(super::super::TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        register_visible(
            &registry,
            &SessionScope::new("deleted-session"),
            external_registration("surviving-process", "Survivor"),
        )
        .await;

        let report = registry
            .delete_session_process_state("deleted-session")
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
                status: super::super::ProcessStatusFilter::Any,
                ..ProcessListFilter::default()
            })
            .await
            .expect("runtime process snapshot");
        assert_eq!(runtime_items.len(), 1);
        assert_eq!(runtime_items[0].process.process_id, "surviving-process");
    }

    #[tokio::test]
    async fn list_batches_lease_reads_without_changing_mixed_results() {
        let registry = Arc::new(super::super::TestLocalProcessRegistry::default());
        for process_id in ["batch-leased", "batch-unleased", "batch-terminal"] {
            registry
                .register_process(external_registration(process_id, process_id))
                .await
                .expect("register batch observation fixture");
        }
        registry
            .claim_process_lease(
                "batch-leased",
                &crate::LeaseOwnerIdentity::opaque("observer", "one"),
                60_000,
            )
            .await
            .expect("claim observed lease")
            .acquired()
            .expect("observed lease acquired");
        registry
            .complete_process(
                "batch-terminal",
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
                status: super::super::ProcessStatusFilter::Any,
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
            observed.iter().filter(|process| process.terminal).count(),
            1
        );
        assert_eq!(*registry.process_lease_batch_reads.lock().await, 1);
        assert_eq!(*registry.process_lease_point_reads.lock().await, 0);
    }

    #[tokio::test]
    async fn snapshot_for_session_sorts_work_by_updated_then_created_descending() {
        let registry =
            Arc::new(super::super::TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let scope = SessionScope::new("sort");
        register_visible(&registry, &scope, external_registration("older", "Older")).await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        register_visible(&registry, &scope, external_registration("newer", "Newer")).await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        registry
            .append_event(
                "older",
                ProcessEventAppendRequest::new("process.cancel_requested", json!({}))
                    .with_replay_key("older:cancel-requested"),
            )
            .await
            .expect("update older process");

        let snapshot = observer(Arc::clone(&registry))
            .snapshot_for_session("sort")
            .await
            .expect("snapshot");

        assert_eq!(snapshot.visible_process_ids, vec!["older", "newer"]);
    }

    #[tokio::test]
    async fn observed_process_reports_terminal_status_and_error_messages() {
        let registry =
            Arc::new(super::super::TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        for process_id in ["failed", "cancelled"] {
            registry
                .register_process(external_registration(process_id, process_id))
                .await
                .expect("register");
        }
        registry
            .complete_process(
                "failed",
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
                "cancelled",
                ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::cancelled(
                    crate::ToolCancellation::runtime("cancelled intentionally"),
                )),
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("cancel process");

        let observer = observer(Arc::clone(&registry));
        let failed = observer
            .process("failed")
            .await
            .expect("read failed process")
            .expect("failed process");
        let cancelled = observer
            .process("cancelled")
            .await
            .expect("read cancelled process")
            .expect("cancelled process");

        assert_eq!(failed.status_label, "failed");
        assert!(failed.terminal);
        assert_eq!(failed.error.as_deref(), Some("failed loudly"));
        assert_eq!(cancelled.status_label, "cancelled");
        assert!(cancelled.terminal);
        assert_eq!(cancelled.error.as_deref(), Some("cancelled intentionally"));
    }

    #[tokio::test]
    async fn observed_process_exposes_current_wait_state() {
        let registry =
            Arc::new(super::super::TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let scope = SessionScope::new("wait");
        register_visible(
            &registry,
            &scope,
            external_registration("waiting-process", "Waiting"),
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
            .set_process_wait("waiting-process", wait.clone())
            .await
            .expect("set wait");

        let observer = observer(Arc::clone(&registry));
        let observed = observer
            .process("waiting-process")
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
        let registry =
            Arc::new(super::super::TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let scope = SessionScope::new("labels");
        let mut child_request = SessionCreateRequest::child_session(
            "labels",
            SessionStartPoint::Empty,
            PluginOptions::default(),
        )
        .with_session_id("child-session");
        child_request.subagent = Some(SubagentSessionContext {
            parent_session_id: "labels".to_string(),
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
                        "tool:shell.run",
                        "shell.run",
                        json!({}),
                        None,
                        serde_json::Value::Null,
                    ),
                },
                "tool",
                "shell.run",
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
            let mut registration =
                ProcessRegistration::new(process_id, input, disposition, ProcessProvenance::host())
                    .with_identity(ProcessIdentity::new(kind).with_label(Some(label.to_string())));
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

        assert_eq!(by_id["tool"].label, "shell.run");
        assert_eq!(by_id["engine"].label, "remember");
        assert_eq!(by_id["engine"].process.kind, "test-engine");
        assert_eq!(by_id["session"].label, "researcher");
        assert_eq!(
            by_id["session"].process.child_session_id.as_deref(),
            Some("child-session")
        );
        assert_eq!(by_id["external"].label, "external job");
    }

    #[tokio::test]
    async fn observed_process_missing_lookup_returns_none() {
        let registry =
            Arc::new(super::super::TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;

        assert!(
            observer(registry)
                .process("missing")
                .await
                .expect("read missing process")
                .is_none()
        );
    }
}
