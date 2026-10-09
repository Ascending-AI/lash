use std::collections::BTreeSet;
use std::sync::Mutex;

use lash_sansio::ExecutionNodeKind;
use lash_sansio::sync::MutexExt;
use lash_trace::{TraceBranchSelection, TraceLanguageExecutionPayload, TraceRuntimeScope};

use crate::TraceWaitBookkeeping;

pub fn trace_failure(
    failure: lash_vm::LashVmExecutionFailure,
) -> lash_trace::TraceLanguageExecutionFailure {
    match failure {
        lash_vm::LashVmExecutionFailure::Effect(effect) => {
            lash_trace::TraceLanguageExecutionFailure::Effect {
                class: effect.class,
                code: effect.code,
                message: effect.message,
                replay_key: effect.replay_key,
                source: effect.source,
                suggested_delay_ms: effect.suggested_delay_ms,
            }
        }
        lash_vm::LashVmExecutionFailure::Runtime { code, message } => {
            lash_trace::TraceLanguageExecutionFailure::Runtime { code, message }
        }
    }
}

/// Adapts the VM's internal execution observations to the public language
/// trace payload consumed by hosts. The wrapped host never receives an
/// internal execution-site descriptor.
///
/// Cancellation follows the engine hosts' rule: an in-flight occurrence that
/// ends while the wrapped host reports cancellation is reported as
/// `NodeCancelled`, preceded by a cancelled wait resolution when it was
/// parked. Occurrences that never started stay unobserved.
pub struct LanguageTraceHost<H, O> {
    host: H,
    observer: O,
    active_nodes: Mutex<BTreeSet<(lash_sansio::WorkflowSiteRef, ExecutionNodeKind, u64)>>,
    waiting_nodes: TraceWaitBookkeeping,
}

impl<H, O> LanguageTraceHost<H, O> {
    pub fn new(host: H, observer: O) -> Self {
        Self {
            host,
            observer,
            active_nodes: Mutex::new(BTreeSet::new()),
            waiting_nodes: TraceWaitBookkeeping::default(),
        }
    }

    pub fn host(&self) -> &H {
        &self.host
    }
}

impl<H, O> lash_vm::ExecutionHost for LanguageTraceHost<H, O>
where
    H: lash_vm::ExecutionHost,
    O: Fn(&H, TraceLanguageExecutionPayload) + Sync,
{
    async fn perform(
        &self,
        op: lash_vm::AbilityOp,
    ) -> Result<lash_vm::AbilityOutcome, lash_vm::ExecutionHostError> {
        self.host.perform(op).await
    }

    async fn cancel_checkpoint(&self, checkpoint: u64) {
        self.host.cancel_checkpoint(checkpoint).await;
    }

    fn execution_mode(&self) -> lash_vm::ExecutionMode {
        self.host.execution_mode()
    }

    fn projected_bindings(&self) -> lash_vm::ProjectedBindings {
        self.host.projected_bindings()
    }

    fn trace_runtime_errors(&self) -> bool {
        self.host.trace_runtime_errors()
    }

    fn profile_execution(&self) -> bool {
        self.host.profile_execution()
    }

    fn execution_bounds(&self) -> lash_vm::ExecutionBounds {
        self.host.execution_bounds()
    }

    fn is_cancelled(&self) -> bool {
        self.host.is_cancelled()
    }

    fn collect_heap_every_allocation(&self) -> bool {
        self.host.collect_heap_every_allocation()
    }

    fn take_scratch(&self) -> Option<lash_vm::ExecutionScratch> {
        self.host.take_scratch()
    }

    fn store_scratch(&self, scratch: lash_vm::ExecutionScratch) {
        self.host.store_scratch(scratch);
    }

    fn observe_runtime_failure(&self, failure: lash_vm::RuntimeFailure) {
        self.host.observe_runtime_failure(failure);
    }

    fn observe_profile(&self, profile: lash_vm::ProfileReport) {
        self.host.observe_profile(profile);
    }

    fn observes_lash_vm_execution(&self) -> bool {
        true
    }

    fn observe_lash_vm_execution(&self, observation: lash_vm::LashVmExecutionObservation) {
        use lash_vm::LashVmExecutionObservation as Observation;
        let active = |site: &lash_vm::LashVmExecutionSite, occurrence: u64| {
            (site.site_ref(), site.node_kind, occurrence)
        };
        match &observation {
            Observation::NodeStarted {
                site, occurrence, ..
            } => {
                self.active_nodes
                    .lock_recover()
                    .insert(active(site, *occurrence));
            }
            Observation::ChildProcessWaiting {
                site, occurrence, ..
            } => {
                self.waiting_nodes.mark_waiting(site, *occurrence);
            }
            Observation::NodeResumed {
                site, occurrence, ..
            } => {
                self.waiting_nodes.finish(site, *occurrence);
            }
            Observation::NodeFailed {
                site,
                occurrence,
                loops,
                ..
            } if self.host.is_cancelled() => {
                if self
                    .active_nodes
                    .lock_recover()
                    .remove(&active(site, *occurrence))
                {
                    self.emit_cancelled(site, *occurrence, loops);
                    return;
                }
            }
            Observation::NodeCompleted {
                site,
                occurrence,
                loops,
            } if self.host.is_cancelled() && self.waiting_nodes.is_waiting(site, *occurrence) => {
                self.active_nodes
                    .lock_recover()
                    .remove(&active(site, *occurrence));
                self.emit_cancelled(site, *occurrence, loops);
                return;
            }
            Observation::NodeCompleted {
                site, occurrence, ..
            }
            | Observation::NodeFailed {
                site, occurrence, ..
            } => {
                self.active_nodes
                    .lock_recover()
                    .remove(&active(site, *occurrence));
            }
            Observation::BranchSelected { .. } | Observation::ChildStarted { .. } => {}
        }
        (self.observer)(&self.host, public_payload(observation));
    }
}

impl<H, O> LanguageTraceHost<H, O>
where
    O: Fn(&H, TraceLanguageExecutionPayload),
{
    fn emit_cancelled(
        &self,
        site: &lash_vm::LashVmExecutionSite,
        occurrence: u64,
        loops: &[lash_sansio::WorkflowLoopFrame],
    ) {
        if self.waiting_nodes.finish(site, occurrence) {
            (self.observer)(
                &self.host,
                TraceLanguageExecutionPayload::NodeResumed {
                    node_id: site.node_id.clone(),
                    node_kind: site.node_kind,
                    label: site.label.clone(),
                    occurrence,
                    context: site.occurrence_context(loops),
                    resolution: lash_trace::TraceNodeWaitResolution::Cancelled,
                },
            );
        }
        (self.observer)(
            &self.host,
            TraceLanguageExecutionPayload::NodeCancelled {
                node_id: site.node_id.clone(),
                node_kind: site.node_kind,
                label: site.label.clone(),
                occurrence,
                context: site.occurrence_context(loops),
            },
        );
    }
}

fn public_payload(
    observation: lash_vm::LashVmExecutionObservation,
) -> TraceLanguageExecutionPayload {
    match observation {
        lash_vm::LashVmExecutionObservation::ChildProcessWaiting {
            site,
            occurrence,
            process_ids,
            loops,
        } => TraceLanguageExecutionPayload::NodeWaiting {
            context: site.occurrence_context(&loops),
            node_id: site.node_id,
            node_kind: site.node_kind,
            label: site.label,
            occurrence,
            awaited: lash_trace::TraceNodeAwaited::ChildProcesses { process_ids },
        },
        lash_vm::LashVmExecutionObservation::NodeResumed {
            site,
            occurrence,
            loops,
        } => TraceLanguageExecutionPayload::NodeResumed {
            context: site.occurrence_context(&loops),
            node_id: site.node_id,
            node_kind: site.node_kind,
            label: site.label,
            occurrence,
            resolution: lash_trace::TraceNodeWaitResolution::Resumed,
        },
        lash_vm::LashVmExecutionObservation::NodeStarted {
            site,
            occurrence,
            loops,
        } => TraceLanguageExecutionPayload::NodeStarted {
            context: site.occurrence_context(&loops),
            node_id: site.node_id,
            node_kind: site.node_kind,
            label: site.label,
            occurrence,
            call_id: None,
        },
        lash_vm::LashVmExecutionObservation::NodeCompleted {
            site,
            occurrence,
            loops,
        } => TraceLanguageExecutionPayload::NodeCompleted {
            context: site.occurrence_context(&loops),
            node_id: site.node_id,
            node_kind: site.node_kind,
            label: site.label,
            occurrence,
            call_id: None,
        },
        lash_vm::LashVmExecutionObservation::NodeFailed {
            site,
            occurrence,
            failure,
            loops,
        } => TraceLanguageExecutionPayload::NodeFailed {
            context: site.occurrence_context(&loops),
            node_id: site.node_id,
            node_kind: site.node_kind,
            label: site.label,
            occurrence,
            call_id: None,
            failure: trace_failure(failure),
        },
        lash_vm::LashVmExecutionObservation::BranchSelected {
            site,
            occurrence,
            edge_id,
            selected,
            loops,
        } => TraceLanguageExecutionPayload::BranchSelected {
            context: site.occurrence_context(&loops),
            node_id: site.node_id,
            occurrence,
            edge_id,
            selected: match selected {
                lash_vm::ProcessBranchSelection::Then => TraceBranchSelection::Then,
                lash_vm::ProcessBranchSelection::Else => TraceBranchSelection::Else,
            },
        },
        lash_vm::LashVmExecutionObservation::ChildStarted {
            site,
            occurrence,
            child,
            loops,
        } => TraceLanguageExecutionPayload::ChildStarted {
            context: site.occurrence_context(&loops),
            parent_node_id: site.node_id,
            occurrence,
            child: lash_trace::TraceLanguageChildExecution {
                scope: TraceRuntimeScope::none(),
                process_id: child.process_id,
                attempt: child.attempt,
                module_ref: Some(child.module_ref.to_string()),
                entry_ref: Some(lash_vm::process_ref_key(&child.process_ref)),
                entry_name: Some(child.process_name),
            },
        },
    }
}
