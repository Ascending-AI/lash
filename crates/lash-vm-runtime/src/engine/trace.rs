//! Language observation of a process's actual VM steps. A settled step is
//! never run again; a continuation keeps the VM's occurrence numbering.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use lash_core::plugin::PluginExecutionTrace;
use lash_sansio::sync::MutexExt as _;
use lash_trace::{
    TraceEvent, TraceLanguageExecution, TraceLanguageExecutionIdentity,
    TraceLanguageExecutionPayload, TraceLanguageExecutionStatus, TraceNodeFact, TraceRuntimeScope,
    TraceRuntimeSubject,
};

#[derive(Clone)]
pub(super) struct ProcessTrace {
    tracing: PluginExecutionTrace,
    identity: TraceLanguageExecutionIdentity,
    pending_resource_starts:
        Arc<Mutex<BTreeMap<lash_sansio::WorkflowOccurrence, TraceLanguageExecutionPayload>>>,
    settled_calls: Arc<Mutex<BTreeMap<lash_sansio::WorkflowOccurrence, lash_core::ToolCallId>>>,
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
                document: lash_trace::WorkflowDocumentRef {
                    source_identity: artifact.source_identity(),
                    module_ref: input.module_ref.clone(),
                    entry: lash_trace::WorkflowDocumentEntry::Process {
                        process_ref: lash_vm::process_ref_key(&input.process_ref),
                    },
                    ir_version: artifact.graph.ir_version,
                },
                entry_name: input.process_name.clone(),
                engine_execution_id: Some(process.to_string()),
                // The durable actor has no execution-attempt fact. Its
                // minted process identity names the resumed lifetime.
                generation: None,
            },
        })
    }

    /// The execution began: it names the document it runs and the process
    /// it enters it by.
    pub(super) fn started(&self) {
        self.emit_payload(TraceLanguageExecutionPayload::ExecutionStarted);
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
        self.emit_payload(TraceLanguageExecutionPayload::ExecutionFinished {
            status,
            error: None,
        });
    }

    /// Emit a fact about an occurrence of a site of static kind `kind`.
    pub(super) fn emit(
        &self,
        mut payload: TraceLanguageExecutionPayload,
        kind: lash_sansio::ExecutionNodeKind,
    ) {
        let TraceLanguageExecutionPayload::Node { at, fact } = &mut payload else {
            self.emit_payload(payload);
            return;
        };
        if let TraceNodeFact::Completed { call_id } | TraceNodeFact::Failed { call_id, .. } = fact {
            *call_id = self.settled_calls.lock_recover().remove(at);
        }
        match fact {
            TraceNodeFact::Started { .. }
                if kind == lash_vm::RESOURCE_OPERATION_EXECUTION_SITE_KIND =>
            {
                let at = at.clone();
                self.pending_resource_starts
                    .lock_recover()
                    .insert(at, payload);
                return;
            }
            TraceNodeFact::Completed { .. }
            | TraceNodeFact::Failed { .. }
            | TraceNodeFact::Cancelled => {
                let pending = self.pending_resource_starts.lock_recover().remove(at);
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
                calls.insert(site.at.clone(), call.clone());
            }
        }
    }

    /// The operation at `call_site` became a tool step. Its actor reports
    /// the body's start once it admits the step, bound to its call; a start
    /// published here would claim a call that admission can still refuse.
    pub(super) fn resource_issued(&self, call_site: &lash_vm::LashVmExecutionCallSite) {
        self.pending_resource_starts
            .lock_recover()
            .remove(&call_site.at);
    }

    fn emit_payload(&self, payload: TraceLanguageExecutionPayload) {
        use TraceLanguageExecutionPayload as Payload;
        let (suffix, node) = match &payload {
            Payload::ExecutionStarted => ("started".to_owned(), None),
            Payload::ExecutionFinished { .. } => ("finished".to_owned(), None),
            Payload::Node { at, fact } => {
                // The site, not the node, names an occurrence: two sites of
                // one node each count from 1.
                let occurrence = format!("{}:{}", at.site, at.occurrence);
                let suffix = match fact {
                    TraceNodeFact::BranchSelected { .. } => format!("branch:{occurrence}"),
                    TraceNodeFact::ChildStarted { child } => {
                        format!("child:{occurrence}:{}", child.process_id)
                    }
                    TraceNodeFact::Started { .. } => format!("node:{occurrence}:started"),
                    TraceNodeFact::Completed { .. } => format!("node:{occurrence}:completed"),
                    TraceNodeFact::Failed { .. } => format!("node:{occurrence}:failed"),
                    TraceNodeFact::Cancelled => format!("node:{occurrence}:cancelled"),
                    TraceNodeFact::Waiting { .. } => format!("node:{occurrence}:waiting"),
                    TraceNodeFact::Resumed { .. } => format!("node:{occurrence}:resumed"),
                };
                (suffix, Some(at.site.node_id.to_string()))
            }
        };
        let event_key = format!("lash_vm_execution:{}:{suffix}", self.identity.graph_key());
        let mut context = self.tracing.trace_runtime().base_context().clone();
        context.graph_node_id = node;
        let event = TraceLanguageExecution {
            event_key,
            identity: self.identity.clone(),
            payload,
        };
        self.tracing.observe_language(&event.event_key, || {
            (
                context.clone(),
                TraceEvent::LanguageExecution {
                    language: None,
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
        let fact = if resumed {
            TraceNodeFact::Resumed {
                resolution: crate::TraceNodeWaitResolution::Resumed,
            }
        } else {
            TraceNodeFact::Waiting { awaited }
        };
        self.emit(
            TraceLanguageExecutionPayload::Node {
                at: site.at.clone(),
                fact,
            },
            site.kind,
        );
    }
}
