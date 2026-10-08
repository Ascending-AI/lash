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

#[derive(Clone)]
pub(super) struct ProcessTrace {
    tracing: PluginExecutionTrace,
    identity: TraceLanguageExecutionIdentity,
    pending_resource_starts: Arc<Mutex<BTreeMap<(String, u64), TraceLanguageExecutionPayload>>>,
}

impl ProcessTrace {
    pub(super) fn new(
        engine: &crate::LashlangProcessEngine,
        process: &lash_core::ProcessId,
        input: &crate::LashlangProcessInput,
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
            identity: TraceLanguageExecutionIdentity {
                scope: TraceRuntimeScope::none(),
                subject: TraceRuntimeSubject::Process {
                    process_id: process.clone(),
                },
                source_identity: artifact.source_identity(),
                module_ref: input.module_ref.to_string(),
                entry_kind: "process".to_owned(),
                entry_ref: Some(lashlang::process_ref_key(&input.process_ref)),
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

    pub(super) fn emit(&self, payload: TraceLanguageExecutionPayload) {
        use TraceLanguageExecutionPayload as Payload;
        match &payload {
            Payload::NodeStarted {
                node_id,
                node_kind,
                occurrence,
                ..
            } if *node_kind == lashlang::RESOURCE_OPERATION_EXECUTION_SITE_KIND => {
                self.pending_resource_starts
                    .lock_recover()
                    .insert((node_id.clone(), *occurrence), payload);
                return;
            }
            Payload::NodeCompleted {
                node_id,
                occurrence,
                ..
            }
            | Payload::NodeFailed {
                node_id,
                occurrence,
                ..
            }
            | Payload::NodeCancelled {
                node_id,
                occurrence,
                ..
            } => {
                let pending = self
                    .pending_resource_starts
                    .lock_recover()
                    .remove(&(node_id.clone(), *occurrence));
                if let Some(started) = pending {
                    self.emit_payload(started);
                }
            }
            _ => {}
        }
        self.emit_payload(payload);
    }

    /// Hand the observed start to the tool step. Its actor owns the call id;
    /// emitting here would publish an unbound start before admission.
    pub(super) fn resource_started(
        &self,
        call_site: &lashlang::LashlangExecutionCallSite,
    ) -> TraceLanguageExecution {
        self.pending_resource_starts
            .lock_recover()
            .remove(&(call_site.site.node_id.clone(), call_site.occurrence));
        TraceLanguageExecution {
            event_key: format!(
                "lashlang_execution:{}:node:{}:{}:started",
                self.identity.graph_key(),
                call_site.site.node_id,
                call_site.occurrence
            ),
            identity: self.identity.clone(),
            payload: TraceLanguageExecutionPayload::NodeStarted {
                node_id: call_site.site.node_id.clone(),
                node_kind: call_site.site.node_kind,
                label: call_site.site.label.clone(),
                occurrence: call_site.occurrence,
                call_id: None,
            },
        }
    }

    fn emit_payload(&self, payload: TraceLanguageExecutionPayload) {
        use TraceLanguageExecutionPayload as Payload;
        let (suffix, node) = match &payload {
            Payload::ExecutionStarted { .. } => ("started".to_owned(), None),
            Payload::ExecutionFinished { .. } => ("finished".to_owned(), None),
            Payload::BranchSelected {
                node_id,
                occurrence,
                edge_id,
                ..
            } => (
                format!("branch:{node_id}:{occurrence}:{edge_id}"),
                Some(node_id),
            ),
            Payload::ChildStarted {
                parent_node_id,
                occurrence,
                child,
            } => (
                format!("child:{parent_node_id}:{occurrence}:{}", child.process_id),
                Some(parent_node_id),
            ),
            Payload::NodeStarted {
                node_id,
                occurrence,
                ..
            } => (
                format!("node:{node_id}:{occurrence}:started"),
                Some(node_id),
            ),
            Payload::NodeCompleted {
                node_id,
                occurrence,
                ..
            } => (
                format!("node:{node_id}:{occurrence}:completed"),
                Some(node_id),
            ),
            Payload::NodeFailed {
                node_id,
                occurrence,
                ..
            } => (format!("node:{node_id}:{occurrence}:failed"), Some(node_id)),
            Payload::NodeCancelled {
                node_id,
                occurrence,
                ..
            } => (
                format!("node:{node_id}:{occurrence}:cancelled"),
                Some(node_id),
            ),
            Payload::NodeWaiting {
                node_id,
                occurrence,
                ..
            } => (
                format!("node:{node_id}:{occurrence}:waiting"),
                Some(node_id),
            ),
            Payload::NodeResumed {
                node_id,
                occurrence,
                ..
            } => (
                format!("node:{node_id}:{occurrence}:resumed"),
                Some(node_id),
            ),
        };
        let event_key = format!("lashlang_execution:{}:{suffix}", self.identity.graph_key());
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
    pub(super) fn waiting(&self, op: &lashlang::AbilityOp, now_ms: i64, resumed: bool) {
        let (site, awaited) = match op {
            lashlang::AbilityOp::Sleep(sleep) => {
                let Some(site) = &sleep.call_site else { return };
                let deadline_ms =
                    crate::timer_duration_ms(sleep)
                        .ok()
                        .map(|value| match sleep.kind {
                            lashlang::SleepKind::Until => value,
                            lashlang::SleepKind::For => {
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
            }
        } else {
            TraceLanguageExecutionPayload::NodeWaiting {
                node_id: site.site.node_id.clone(),
                node_kind: site.site.node_kind,
                label: site.site.label.clone(),
                occurrence: site.occurrence,
                awaited,
            }
        };
        self.emit(payload);
    }
}
