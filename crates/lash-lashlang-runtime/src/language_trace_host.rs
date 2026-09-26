use std::collections::BTreeSet;
use std::sync::Mutex;

use lash_sansio::ExecutionNodeKind;
use lash_sansio::sync::MutexExt;
use lash_trace::{TraceBranchSelection, TraceLanguageExecutionPayload, TraceRuntimeScope};

use crate::TraceWaitBookkeeping;

pub fn trace_failure(
    failure: lashlang::LashlangExecutionFailure,
) -> lash_trace::TraceLanguageExecutionFailure {
    match failure {
        lashlang::LashlangExecutionFailure::Effect(effect) => {
            lash_trace::TraceLanguageExecutionFailure::Effect {
                class: effect.class,
                code: effect.code,
                message: effect.message,
                replay_key: effect.replay_key,
                source: effect.source,
                retry: effect.retry,
            }
        }
        lashlang::LashlangExecutionFailure::Runtime { code, message } => {
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
    active_nodes: Mutex<BTreeSet<(String, ExecutionNodeKind, u64)>>,
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

impl<H, O> lashlang::ExecutionHost for LanguageTraceHost<H, O>
where
    H: lashlang::ExecutionHost,
    O: Fn(&H, TraceLanguageExecutionPayload) + Sync,
{
    async fn perform(
        &self,
        op: lashlang::AbilityOp,
    ) -> Result<lashlang::AbilityResult, lashlang::ExecutionHostError> {
        self.host.perform(op).await
    }

    async fn cancel_checkpoint(&self, checkpoint: u64) {
        self.host.cancel_checkpoint(checkpoint).await;
    }

    fn execution_mode(&self) -> lashlang::ExecutionMode {
        self.host.execution_mode()
    }

    fn projected_bindings(&self) -> lashlang::ProjectedBindings {
        self.host.projected_bindings()
    }

    fn trace_runtime_errors(&self) -> bool {
        self.host.trace_runtime_errors()
    }

    fn profile_execution(&self) -> bool {
        self.host.profile_execution()
    }

    fn execution_bounds(&self) -> lashlang::ExecutionBounds {
        self.host.execution_bounds()
    }

    fn is_cancelled(&self) -> bool {
        self.host.is_cancelled()
    }

    fn collect_heap_every_allocation(&self) -> bool {
        self.host.collect_heap_every_allocation()
    }

    fn take_scratch(&self) -> Option<lashlang::ExecutionScratch> {
        self.host.take_scratch()
    }

    fn store_scratch(&self, scratch: lashlang::ExecutionScratch) {
        self.host.store_scratch(scratch);
    }

    fn observe_runtime_failure(&self, failure: lashlang::RuntimeFailure) {
        self.host.observe_runtime_failure(failure);
    }

    fn observe_profile(&self, profile: lashlang::ProfileReport) {
        self.host.observe_profile(profile);
    }

    fn observes_lashlang_execution(&self) -> bool {
        true
    }

    fn observe_lashlang_execution(&self, observation: lashlang::LashlangExecutionObservation) {
        use lashlang::LashlangExecutionObservation as Observation;
        match &observation {
            Observation::NodeStarted { site, occurrence } => {
                self.active_nodes.lock_recover().insert((
                    site.node_id.clone(),
                    site.node_kind,
                    *occurrence,
                ));
            }
            Observation::ChildProcessWaiting {
                site, occurrence, ..
            } => {
                self.waiting_nodes
                    .mark_waiting(&site.node_id, site.node_kind, *occurrence);
            }
            Observation::NodeResumed { site, occurrence } => {
                self.waiting_nodes
                    .finish(&site.node_id, site.node_kind, *occurrence);
            }
            Observation::NodeFailed {
                site, occurrence, ..
            } if self.host.is_cancelled() => {
                if self.active_nodes.lock_recover().remove(&(
                    site.node_id.clone(),
                    site.node_kind,
                    *occurrence,
                )) {
                    self.emit_cancelled(site, *occurrence);
                    return;
                }
            }
            Observation::NodeCompleted { site, occurrence }
                if self.host.is_cancelled()
                    && self.waiting_nodes.is_waiting(
                        &site.node_id,
                        site.node_kind,
                        *occurrence,
                    ) =>
            {
                self.active_nodes.lock_recover().remove(&(
                    site.node_id.clone(),
                    site.node_kind,
                    *occurrence,
                ));
                self.emit_cancelled(site, *occurrence);
                return;
            }
            Observation::NodeCompleted { site, occurrence }
            | Observation::NodeFailed {
                site, occurrence, ..
            } => {
                self.active_nodes.lock_recover().remove(&(
                    site.node_id.clone(),
                    site.node_kind,
                    *occurrence,
                ));
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
    fn emit_cancelled(&self, site: &lashlang::LashlangExecutionSite, occurrence: u64) {
        if self
            .waiting_nodes
            .finish(&site.node_id, site.node_kind, occurrence)
        {
            (self.observer)(
                &self.host,
                TraceLanguageExecutionPayload::NodeResumed {
                    node_id: site.node_id.clone(),
                    node_kind: site.node_kind,
                    label: site.label.clone(),
                    occurrence,
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
            },
        );
    }
}

fn public_payload(
    observation: lashlang::LashlangExecutionObservation,
) -> TraceLanguageExecutionPayload {
    match observation {
        lashlang::LashlangExecutionObservation::ChildProcessWaiting {
            site,
            occurrence,
            process_ids,
        } => TraceLanguageExecutionPayload::NodeWaiting {
            node_id: site.node_id,
            node_kind: site.node_kind,
            label: site.label,
            occurrence,
            awaited: lash_trace::TraceNodeAwaited::ChildProcesses { process_ids },
        },
        lashlang::LashlangExecutionObservation::NodeResumed { site, occurrence } => {
            TraceLanguageExecutionPayload::NodeResumed {
                node_id: site.node_id,
                node_kind: site.node_kind,
                label: site.label,
                occurrence,
                resolution: lash_trace::TraceNodeWaitResolution::Resumed,
            }
        }
        lashlang::LashlangExecutionObservation::NodeStarted { site, occurrence } => {
            TraceLanguageExecutionPayload::NodeStarted {
                node_id: site.node_id,
                node_kind: site.node_kind,
                label: site.label,
                occurrence,
                call_id: None,
            }
        }
        lashlang::LashlangExecutionObservation::NodeCompleted { site, occurrence } => {
            TraceLanguageExecutionPayload::NodeCompleted {
                node_id: site.node_id,
                node_kind: site.node_kind,
                label: site.label,
                occurrence,
                call_id: None,
            }
        }
        lashlang::LashlangExecutionObservation::NodeFailed {
            site,
            occurrence,
            failure,
        } => TraceLanguageExecutionPayload::NodeFailed {
            node_id: site.node_id,
            node_kind: site.node_kind,
            label: site.label,
            occurrence,
            call_id: None,
            failure: trace_failure(failure),
        },
        lashlang::LashlangExecutionObservation::BranchSelected {
            site,
            occurrence,
            edge_id,
            selected,
        } => TraceLanguageExecutionPayload::BranchSelected {
            node_id: site.node_id,
            occurrence,
            edge_id,
            selected: match selected {
                lashlang::ProcessBranchSelection::Then => TraceBranchSelection::Then,
                lashlang::ProcessBranchSelection::Else => TraceBranchSelection::Else,
            },
        },
        lashlang::LashlangExecutionObservation::ChildStarted {
            site,
            occurrence,
            child,
        } => TraceLanguageExecutionPayload::ChildStarted {
            parent_node_id: site.node_id,
            occurrence,
            child: lash_trace::TraceLanguageChildExecution {
                scope: TraceRuntimeScope::none(),
                process_id: child.process_id,
                attempt: child.attempt,
                module_ref: Some(child.module_ref.to_string()),
                entry_ref: Some(lashlang::process_ref_key(&child.process_ref)),
                entry_name: Some(child.process_name),
            },
        },
    }
}
