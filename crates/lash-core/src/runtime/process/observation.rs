use std::collections::BTreeSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::plugin::PluginError;

use super::events::{ProcessAwaitOutput, ProcessEvent};
use super::model::{
    AbandonRequest, ProcessExecutionEnvRef, ProcessExternalRef, ProcessHandleDescriptor, ProcessId,
    ProcessIdentity, ProcessInput, ProcessLease, ProcessLifecycleStatus, ProcessListFilter,
    ProcessOriginator, ProcessRecord, ProcessStarted, ProcessStatusFilter, RecoveryDisposition,
    SessionScope, WaitState,
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
    pub descriptor: ProcessHandleDescriptor,
    pub events: Vec<ObservedProcessEvent>,
    pub kind: String,
    pub label: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObservedProcess {
    pub process_id: ProcessId,
    pub graph_key: String,
    pub kind: String,
    pub lifecycle: ProcessLifecycleStatus,
    pub identity: ProcessIdentity,
    pub status_label: String,
    pub terminal: bool,
    /// Declared recovery contract (ADR 0019). Raw fact; hosts classify.
    pub disposition: RecoveryDisposition,
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
    pub wake_target: Option<SessionScope>,
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
        let session_scope = SessionScope::new(session_id.clone());
        let entries = self.registry.list_handle_grants(&session_scope).await?;
        let mut items = Vec::new();
        let mut seen_process_ids = BTreeSet::new();
        for (grant, record) in entries {
            seen_process_ids.insert(record.id.clone());
            items.push(self.work_item_from_record(record, grant.descriptor).await?);
        }
        let visible_records = self
            .registry
            .list_processes(&ProcessListFilter {
                status: ProcessStatusFilter::Any,
                ..ProcessListFilter::default()
            })
            .await?;
        for record in visible_records {
            if seen_process_ids.contains(&record.id)
                || !process_visible_to_session(&record, &session_id)
            {
                continue;
            }
            seen_process_ids.insert(record.id.clone());
            let descriptor = descriptor_from_process_identity(&record.identity);
            items.push(self.work_item_from_record(record, descriptor).await?);
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
    /// session handle grant and therefore continues to expose processes whose
    /// originating session has been deleted.
    /// Because grants are bypassed, the host must authorize access; routing
    /// identity is not authorization.
    pub async fn snapshot_all(
        &self,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ObservedWorkItem>, PluginError> {
        let records = self.registry.list_processes(filter).await?;
        let mut items = Vec::with_capacity(records.len());
        for record in records {
            let descriptor = descriptor_from_process_identity(&record.identity);
            items.push(self.work_item_from_record(record, descriptor).await?);
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
        descriptor: ProcessHandleDescriptor,
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
            .or_else(|| descriptor.label.clone())
            .unwrap_or_else(|| kind.clone());
        Ok(ObservedWorkItem {
            process,
            descriptor,
            events,
            kind,
            label,
        })
    }

    pub async fn process(&self, process_id: &str) -> Option<ObservedProcess> {
        let record = self.registry.get_process(process_id).await?;
        let lease = self
            .registry
            .get_process_lease(process_id)
            .await
            .ok()
            .flatten();
        Some(ObservedProcess::from_record(record, lease))
    }

    pub async fn list(
        &self,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ObservedProcess>, PluginError> {
        let records = self.registry.list_processes(filter).await?;
        self.observe_records(records).await
    }

    /// List processes a session may address — the grant filter (ADR 0019 /
    /// process design grill). "Granted to" is the security lens: a process is
    /// visible here only if `scope` holds a handle grant for it. This is the
    /// single home for the grant-scoped view; the session facade sugar is a thin
    /// caller of this method, never a parallel implementation.
    pub async fn list_granted_to(
        &self,
        scope: &SessionScope,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ObservedProcess>, PluginError> {
        let entries = self.registry.list_handle_grants(scope).await?;
        let records = entries
            .into_iter()
            .map(|(_, record)| record)
            .filter(|record| filter.matches_record(record))
            .collect::<Vec<_>>();
        self.observe_records(records).await
    }

    /// List processes a session originated — the provenance filter (ADR 0019 /
    /// process design grill). "Originated by" is the lineage lens, distinct from
    /// the grant lens: a process matches when its recorded originator is a
    /// session whose id equals `scope.session_id` (and its agent frame, when
    /// `scope` names one), regardless of who currently holds a grant.
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
        let mut observed = Vec::with_capacity(records.len());
        for record in records {
            let lease = self.registry.get_process_lease(&record.id).await?;
            observed.push(ObservedProcess::from_record(record, lease));
        }
        Ok(observed)
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
        let lifecycle = ProcessLifecycleStatus::from(&record.status);
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
            error: terminal_error(&record.status),
            created_at_ms: record.created_at_ms,
            updated_at_ms: record.updated_at_ms,
            first_started: record.first_started.map(|started| *started),
            lease_holder,
            lease_expires_at_ms,
            abandon_request: record.abandon_request.map(|request| *request),
            originator: record.provenance.originator,
            env_ref: record.env_ref,
            wake_target: record.wake_target,
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

fn terminal_error(status: &super::model::ProcessStatus) -> Option<String> {
    match status.await_output()? {
        ProcessAwaitOutput::Failure { message, .. }
        | ProcessAwaitOutput::Cancelled { message, .. } => Some(message.clone()),
        // Abandonment is not a reported failure; the status label conveys it and
        // the evidence rides the terminal event. No derived error string here.
        ProcessAwaitOutput::Success { .. } | ProcessAwaitOutput::Abandoned { .. } => None,
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

/// Whether `originator` names the session (or session+frame) identified by
/// `scope`. Frame is matched only when `scope` names one, so a session-level
/// provenance filter captures every frame the session originated.
fn originator_matches(originator: &ProcessOriginator, scope: &SessionScope) -> bool {
    match originator {
        ProcessOriginator::Host { .. } => false,
        ProcessOriginator::Session {
            scope: origin_scope,
        } => {
            origin_scope.session_id == scope.session_id
                && (scope.agent_frame_id.is_none()
                    || origin_scope.agent_frame_id == scope.agent_frame_id)
        }
    }
}

fn process_visible_to_session(record: &ProcessRecord, session_id: &str) -> bool {
    record
        .wake_target
        .as_ref()
        .is_some_and(|scope| scope.session_id == session_id)
}

fn descriptor_from_process_identity(identity: &ProcessIdentity) -> ProcessHandleDescriptor {
    ProcessHandleDescriptor::new(Some(identity.kind.clone()), identity.label.clone())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::{
        InputItem, PluginOptions, PreparedToolCall, ProcessEventAppendRequest,
        ProcessExecutionEnvRef, ProcessIdentity, ProcessProvenance, ProcessRegistration,
        SessionCreateRequest, SessionScope, SessionStartPoint, SubagentSessionContext,
        TestProcessRegistryWriteExt, ToolFailureClass, ToolOutputContract, TurnInput, WaitKind,
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
            RecoveryDisposition::ExternallyOwned,
            ProcessProvenance::host(),
        )
    }

    async fn register_visible(
        registry: &Arc<dyn ProcessRegistry>,
        scope: &SessionScope,
        registration: ProcessRegistration,
        descriptor: ProcessHandleDescriptor,
    ) {
        let process_id = registration.id.clone();
        registry
            .register_process(registration)
            .await
            .expect("register process");
        registry
            .grant_handle(scope, &process_id, descriptor)
            .await
            .expect("grant process handle");
    }

    #[tokio::test]
    async fn snapshot_for_session_reads_visible_grants_and_events_as_epoch_ms() {
        let registry =
            Arc::new(super::super::TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let visible_scope = SessionScope::new("visible");
        register_visible(
            &registry,
            &visible_scope,
            external_registration("visible-process", "Visible"),
            ProcessHandleDescriptor::new(Some("visible-kind"), Some("Visible descriptor")),
        )
        .await;
        register_visible(
            &registry,
            &SessionScope::new("other"),
            external_registration("hidden-process", "Hidden"),
            ProcessHandleDescriptor::new(Some("hidden-kind"), Some("Hidden")),
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
        assert_eq!(snapshot.items[0].events.len(), 1);
        assert_eq!(
            snapshot.items[0].events[0].event_type,
            "process.cancel_requested"
        );
        assert!(snapshot.items[0].events[0].occurred_at_ms > 0);
    }

    #[tokio::test]
    async fn runtime_snapshot_keeps_orphaned_processes_after_session_deletion() {
        let registry =
            Arc::new(super::super::TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        register_visible(
            &registry,
            &SessionScope::new("deleted-session"),
            external_registration("surviving-process", "Survivor"),
            ProcessHandleDescriptor::new(Some("test"), Some("Survivor")),
        )
        .await;

        let report = registry
            .delete_session_process_state("deleted-session")
            .await
            .expect("delete session process edges");
        assert_eq!(report.orphaned_process_ids, vec!["surviving-process"]);
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
    async fn snapshot_for_session_includes_frame_wake_targets_without_handle_grants() {
        let registry =
            Arc::new(super::super::TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let frame_scope = SessionScope::for_agent_frame("visible", "frame-a");
        registry
            .register_process(ProcessRegistration::new(
                "frame-originated",
                ProcessInput::External {
                    metadata: json!({ "label": "Frame originated" }),
                },
                RecoveryDisposition::ExternallyOwned,
                ProcessProvenance::session(frame_scope.clone()),
            ))
            .await
            .expect("register frame-originated process");
        registry
            .register_process(
                external_registration("frame-wake-targeted", "Frame wake targeted")
                    .with_wake_target(Some(frame_scope)),
            )
            .await
            .expect("register frame wake-targeted process");
        registry
            .register_process(
                external_registration("hidden-frame", "Hidden")
                    .with_wake_target(Some(SessionScope::for_agent_frame("other", "frame-b"))),
            )
            .await
            .expect("register hidden process");

        let snapshot = observer(Arc::clone(&registry))
            .snapshot_for_session("visible")
            .await
            .expect("snapshot");
        let visible_process_ids = snapshot
            .visible_process_ids
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(
            visible_process_ids,
            std::collections::BTreeSet::from(["frame-wake-targeted".to_string()])
        );
        assert_eq!(snapshot.items.len(), 1);
    }

    #[tokio::test]
    async fn snapshot_for_session_labels_engine_wake_targets_from_identity_without_handle_grants() {
        let registry =
            Arc::new(super::super::TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let scope = SessionScope::new("visible");
        registry
            .register_process(
                ProcessRegistration::new(
                    "engine-wake-targeted",
                    ProcessInput::Engine {
                        kind: "test-engine".to_string(),
                        payload: json!({}),
                    },
                    RecoveryDisposition::Rerunnable,
                    ProcessProvenance::host(),
                )
                .with_identity(
                    ProcessIdentity::new("test-engine").with_label(Some("remember".to_string())),
                )
                .with_execution_env_ref(Some(ProcessExecutionEnvRef::new("process-env:test")))
                .with_wake_target(Some(scope)),
            )
            .await
            .expect("register engine wake-targeted process");

        let snapshot = observer(Arc::clone(&registry))
            .snapshot_for_session("visible")
            .await
            .expect("snapshot");

        assert_eq!(snapshot.items.len(), 1);
        assert_eq!(snapshot.items[0].kind, "test-engine");
        assert_eq!(snapshot.items[0].label, "remember");
        assert_eq!(
            snapshot.items[0].descriptor.kind.as_deref(),
            Some("test-engine")
        );
        assert_eq!(
            snapshot.items[0].descriptor.label.as_deref(),
            Some("remember")
        );
        assert_eq!(snapshot.items[0].process.kind, "test-engine");
        assert_eq!(snapshot.items[0].process.label, "remember");
    }

    #[tokio::test]
    async fn snapshot_for_session_sorts_work_by_updated_then_created_descending() {
        let registry =
            Arc::new(super::super::TestLocalProcessRegistry::default()) as Arc<dyn ProcessRegistry>;
        let scope = SessionScope::new("sort");
        register_visible(
            &registry,
            &scope,
            external_registration("older", "Older"),
            ProcessHandleDescriptor::new(None::<String>, None::<String>),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        register_visible(
            &registry,
            &scope,
            external_registration("newer", "Newer"),
            ProcessHandleDescriptor::new(None::<String>, None::<String>),
        )
        .await;
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
                ProcessAwaitOutput::Failure {
                    class: ToolFailureClass::External,
                    code: "boom".to_string(),
                    message: "failed loudly".to_string(),
                    raw: None,
                    control: None,
                },
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("fail process");
        registry
            .complete_process(
                "cancelled",
                ProcessAwaitOutput::Cancelled {
                    message: "cancelled intentionally".to_string(),
                    raw: None,
                    control: None,
                },
                crate::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("cancel process");

        let observer = observer(Arc::clone(&registry));
        let failed = observer.process("failed").await.expect("failed process");
        let cancelled = observer
            .process("cancelled")
            .await
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
            ProcessHandleDescriptor::new(Some("external"), Some("Waiting")),
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
                ProcessInput::External { .. } => RecoveryDisposition::ExternallyOwned,
                _ => RecoveryDisposition::Rerunnable,
            };
            let mut registration =
                ProcessRegistration::new(process_id, input, disposition, ProcessProvenance::host())
                    .with_identity(ProcessIdentity::new(kind).with_label(Some(label.to_string())));
            if needs_env {
                registration = registration.with_execution_env_ref(Some(
                    ProcessExecutionEnvRef::new(format!("process-env:test:{process_id}")),
                ));
            }
            register_visible(
                &registry,
                &scope,
                registration,
                ProcessHandleDescriptor::new(Some("descriptor-kind"), Some("Descriptor label")),
            )
            .await;
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

        assert!(observer(registry).process("missing").await.is_none());
    }
}
