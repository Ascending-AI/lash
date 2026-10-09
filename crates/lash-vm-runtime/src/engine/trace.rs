//! Language observation of a process's actual VM steps. A settled step is
//! never run again; a continuation keeps the VM's occurrence numbering.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use lash_core::plugin::PluginExecutionTrace;
use lash_sansio::sync::MutexExt as _;
use lash_trace::{
    TraceEvent, TraceLanguageExecution, TraceLanguageExecutionIdentity,
    TraceLanguageExecutionPayload, TraceLanguageExecutionStatus, TraceRuntimeScope,
    TraceRuntimeSubject,
};

/// One occurrence of one site: occurrences count per site.
type OccurrenceKey = (lash_sansio::WorkflowSiteRef, u64);

fn occurrence_key(call_site: &lash_vm::LashVmExecutionCallSite) -> OccurrenceKey {
    (call_site.site.site_ref(), call_site.occurrence)
}

#[derive(Clone)]
pub(super) struct ProcessTrace {
    tracing: PluginExecutionTrace,
    identity: TraceLanguageExecutionIdentity,
    pending_resource_starts: Arc<Mutex<BTreeMap<OccurrenceKey, TraceLanguageExecutionPayload>>>,
    settled_calls: Arc<Mutex<BTreeMap<OccurrenceKey, lash_core::ToolCallId>>>,
}

impl ProcessTrace {
    pub(super) fn new(
        engine: &crate::LashVmProcessEngine,
        process: &lash_core::ProcessId,
        input: &crate::LashVmProcessInput,
        artifact: &lash_vm_client::InspectedArtifact,
    ) -> Option<Self> {
        let runtime = engine.trace_runtime.as_ref()?;
        let tracing = PluginExecutionTrace::new(runtime.unreplayed(None));
        if !tracing.observes_language() {
            return None;
        }
        Some(Self {
            tracing,
            pending_resource_starts: Arc::default(),
            settled_calls: Arc::default(),
            identity: TraceLanguageExecutionIdentity {
                scope: TraceRuntimeScope::none(),
                subject: TraceRuntimeSubject::Process {
                    process_id: process.clone(),
                },
                source_identity: artifact.source_identity(),
                module_ref: input.module_ref.to_string(),
                entry_kind: "process".to_owned(),
                entry_ref: Some(lash_vm::process_ref_key(&input.process_ref)),
                entry_name: input.process_name.clone(),
                engine_execution_id: Some(process.to_string()),
                // The durable actor has no execution-attempt fact. Its
                // minted process identity names the resumed lifetime.
                generation: None,
            },
        })
    }

    pub(super) fn started(&self, artifact: &lash_vm_client::InspectedArtifact) {
        if let Some(execution_map) =
            crate::trace_lashlang_process_map(&artifact.graph, &self.identity.entry_name)
        {
            self.emit(TraceLanguageExecutionPayload::ExecutionStarted { execution_map });
        }
    }

    pub(super) fn finished(&self, outcome: &lash_core::ProcessOutcome) {
        let status = match outcome.terminal_status() {
            Some(lash_core::TerminalProcessStatus::Completed) => {
                TraceLanguageExecutionStatus::Completed
            }
            Some(lash_core::TerminalProcessStatus::Cancelled) => {
                TraceLanguageExecutionStatus::Cancelled
            }
            _ => TraceLanguageExecutionStatus::Failed,
        };
        self.emit(TraceLanguageExecutionPayload::ExecutionFinished {
            status,
            error: None,
        });
    }

    pub(super) fn emit(&self, mut payload: TraceLanguageExecutionPayload) {
        use TraceLanguageExecutionPayload as Payload;
        let Some(key) = payload.occurrence_key() else {
            self.emit_payload(payload);
            return;
        };
        if let Payload::NodeCompleted { call_id, .. } | Payload::NodeFailed { call_id, .. } =
            &mut payload
        {
            *call_id = self.settled_calls.lock_recover().remove(&key);
        }
        match &payload {
            Payload::NodeStarted { node_kind, .. }
                if *node_kind == lash_vm::RESOURCE_OPERATION_EXECUTION_SITE_KIND =>
            {
                self.pending_resource_starts
                    .lock_recover()
                    .insert(key, payload);
                return;
            }
            Payload::NodeCompleted { .. }
            | Payload::NodeFailed { .. }
            | Payload::NodeCancelled { .. } => {
                let pending = self.pending_resource_starts.lock_recover().remove(&key);
                if let Some(started) = pending {
                    self.emit_payload(started);
                }
            }
            _ => {}
        }
        self.emit_payload(payload);
    }

    /// Bind reissued VM operations to the actor identities retained in their
    /// injection. VM observations name sites; only the actor admits calls.
    pub(super) fn bind_calls(&self, op: &lash_vm::AbilityOp, inject: &super::state::Injection) {
        let super::state::Injection::Leaves { leaves, .. } = inject else {
            return;
        };
        let sites = match op {
            lash_vm::AbilityOp::ResourceOperation(operation) => vec![operation.call_site.as_ref()],
            lash_vm::AbilityOp::ResourceOperationBatch(batch) => batch
                .leaves
                .iter()
                .map(|leaf| match leaf {
                    lash_vm::ResourceOperationBatchLeaf::Operation(operation) => {
                        operation.call_site.as_ref()
                    }
                    lash_vm::ResourceOperationBatchLeaf::Timer(_) => None,
                })
                .collect(),
            _ => return,
        };
        let mut calls = self.settled_calls.lock_recover();
        for (site, leaf) in sites.into_iter().zip(leaves) {
            if let (
                Some(site),
                super::state::Leaf::Step {
                    call_id: Some(call),
                    ..
                },
            ) = (site, leaf)
            {
                calls.insert(occurrence_key(site), call.clone());
            }
        }
    }

    /// Hand the observed start to the tool step. Its actor owns the call id;
    /// emitting here would publish an unbound start before admission.
    pub(super) fn resource_started(
        &self,
        call_site: &lash_vm::LashVmExecutionCallSite,
    ) -> TraceLanguageExecution {
        self.pending_resource_starts
            .lock_recover()
            .remove(&occurrence_key(call_site));
        TraceLanguageExecution {
            event_key: format!(
                "lash_vm_execution:{}:node:{}:{}:started",
                self.identity.graph_key(),
                call_site.site.site_ref(),
                call_site.occurrence
            ),
            identity: self.identity.clone(),
            payload: TraceLanguageExecutionPayload::NodeStarted {
                node_id: call_site.site.node_id.clone(),
                node_kind: call_site.site.node_kind,
                label: call_site.site.label.clone(),
                occurrence: call_site.occurrence,
                call_id: None,
                context: call_site.context(),
            },
        }
    }

    fn emit_payload(&self, payload: TraceLanguageExecutionPayload) {
        use TraceLanguageExecutionPayload as Payload;
        // The site, not the node, names an occurrence: two sites of one
        // node each count from 1.
        let at = payload
            .occurrence_key()
            .map(|(site, occurrence)| format!("{site}:{occurrence}"))
            .unwrap_or_default();
        let (suffix, node) = match &payload {
            Payload::ExecutionStarted { .. } => ("started".to_owned(), None),
            Payload::ExecutionFinished { .. } => ("finished".to_owned(), None),
            Payload::BranchSelected {
                node_id, edge_id, ..
            } => (format!("branch:{at}:{edge_id}"), Some(node_id)),
            Payload::ChildStarted {
                parent_node_id,
                child,
                ..
            } => (
                format!("child:{at}:{}", child.process_id),
                Some(parent_node_id),
            ),
            Payload::NodeStarted { node_id, .. } => (format!("node:{at}:started"), Some(node_id)),
            Payload::NodeCompleted { node_id, .. } => {
                (format!("node:{at}:completed"), Some(node_id))
            }
            Payload::NodeFailed { node_id, .. } => (format!("node:{at}:failed"), Some(node_id)),
            Payload::NodeCancelled { node_id, .. } => {
                (format!("node:{at}:cancelled"), Some(node_id))
            }
            Payload::NodeWaiting { node_id, .. } => (format!("node:{at}:waiting"), Some(node_id)),
            Payload::NodeResumed { node_id, .. } => (format!("node:{at}:resumed"), Some(node_id)),
        };
        let event_key = format!("lash_vm_execution:{}:{suffix}", self.identity.graph_key());
        let mut context = self.tracing.trace_runtime().base_context().clone();
        context.graph_node_id = node.cloned();
        let event = TraceLanguageExecution {
            event_key,
            identity: self.identity.clone(),
            payload,
        };
        self.tracing.observe_language(&event.event_key, || {
            (
                context.clone(),
                TraceEvent::LanguageExecution {
                    language: "typescript".to_owned(),
                    event: event.clone(),
                },
            )
        });
    }

    /// The host emits timer boundaries; the shared VM
    /// observation adapter emits child-process wait boundaries.
    pub(super) fn waiting(&self, op: &lash_vm::AbilityOp, now_ms: i64, resumed: bool) {
        let (site, awaited) = match op {
            lash_vm::AbilityOp::Sleep(sleep) => {
                let Some(site) = &sleep.call_site else { return };
                let deadline_ms =
                    crate::timer_duration_ms(sleep)
                        .ok()
                        .map(|value| match sleep.kind {
                            lash_vm::SleepKind::Until => value,
                            lash_vm::SleepKind::For => {
                                u64::try_from(now_ms).unwrap_or(0).saturating_add(value)
                            }
                        });
                (site, crate::TraceNodeAwaited::Sleep { deadline_ms })
            }
            _ => return,
        };
        let payload = if resumed {
            TraceLanguageExecutionPayload::NodeResumed {
                node_id: site.site.node_id.clone(),
                node_kind: site.site.node_kind,
                label: site.site.label.clone(),
                occurrence: site.occurrence,
                resolution: crate::TraceNodeWaitResolution::Resumed,
                context: site.context(),
            }
        } else {
            TraceLanguageExecutionPayload::NodeWaiting {
                node_id: site.site.node_id.clone(),
                node_kind: site.site.node_kind,
                label: site.site.label.clone(),
                occurrence: site.occurrence,
                awaited,
                context: site.context(),
            }
        };
        self.emit(payload);
    }
}
