use std::collections::BTreeSet;
use std::sync::Mutex;

use lash_sansio::ExecutionNodeKind;
use lash_sansio::sync::MutexExt;
use lash_trace::{
    TraceBranchSelection, TraceLanguageExecutionPayload, TraceNodeFact, TraceRuntimeScope,
};

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
/// internal execution-site descriptor. The observer is also told the static
/// kind of the site the fact is about, which the payload does not repeat.
///
/// Cancellation follows the engine hosts' rule: an in-flight occurrence that
/// ends while the wrapped host reports cancellation is reported as
/// cancelled, preceded by a cancelled wait resolution when it was
/// parked. Occurrences that never started stay unobserved.
pub struct LanguageTraceHost<H, O> {
    host: H,
    observer: O,
    active_nodes: Mutex<BTreeSet<lash_sansio::WorkflowOccurrence>>,
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
    O: Fn(&H, TraceLanguageExecutionPayload, ExecutionNodeKind) + Sync,
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
        use lash_vm::LashVmExecutionFact as Fact;
        let call_site = &observation.call_site;
        let at = &call_site.at;
        match &observation.fact {
            Fact::NodeStarted => {
                self.active_nodes.lock_recover().insert(at.clone());
            }
            Fact::ChildProcessWaiting { .. } => self.waiting_nodes.mark_waiting(at),
            Fact::NodeResumed => {
                self.waiting_nodes.finish(at);
            }
            Fact::NodeFailed { .. } if self.host.is_cancelled() => {
                if self.active_nodes.lock_recover().remove(at) {
                    self.emit_cancelled(call_site);
                    return;
                }
            }
            Fact::NodeCompleted
                if self.host.is_cancelled() && self.waiting_nodes.is_waiting(at) =>
            {
                self.active_nodes.lock_recover().remove(at);
                self.emit_cancelled(call_site);
                return;
            }
            Fact::NodeCompleted | Fact::NodeFailed { .. } => {
                self.active_nodes.lock_recover().remove(at);
            }
            Fact::BranchSelected { .. } | Fact::ChildStarted { .. } => {}
        }
        let kind = call_site.kind;
        (self.observer)(&self.host, public_payload(observation), kind);
    }
}

impl<H, O> LanguageTraceHost<H, O>
where
    O: Fn(&H, TraceLanguageExecutionPayload, ExecutionNodeKind),
{
    fn emit_cancelled(&self, call_site: &lash_vm::LashVmExecutionCallSite) {
        let node = |fact| TraceLanguageExecutionPayload::Node {
            at: call_site.at.clone(),
            fact,
        };
        if self.waiting_nodes.finish(&call_site.at) {
            (self.observer)(
                &self.host,
                node(TraceNodeFact::Resumed {
                    resolution: lash_trace::TraceNodeWaitResolution::Cancelled,
                }),
                call_site.kind,
            );
        }
        (self.observer)(&self.host, node(TraceNodeFact::Cancelled), call_site.kind);
    }
}

fn public_payload(
    observation: lash_vm::LashVmExecutionObservation,
) -> TraceLanguageExecutionPayload {
    use lash_vm::LashVmExecutionFact as Fact;
    let fact = match observation.fact {
        Fact::ChildProcessWaiting { process_ids } => TraceNodeFact::Waiting {
            awaited: lash_trace::TraceNodeAwaited::ChildProcesses { process_ids },
        },
        Fact::NodeResumed => TraceNodeFact::Resumed {
            resolution: lash_trace::TraceNodeWaitResolution::Resumed,
        },
        Fact::NodeStarted => TraceNodeFact::Started { call_id: None },
        Fact::NodeCompleted => TraceNodeFact::Completed { call_id: None },
        Fact::NodeFailed { failure } => TraceNodeFact::Failed {
            call_id: None,
            failure: trace_failure(failure),
        },
        Fact::BranchSelected { selected } => TraceNodeFact::BranchSelected {
            selected: match selected {
                lash_vm::ProcessBranchSelection::Then => TraceBranchSelection::Then,
                lash_vm::ProcessBranchSelection::Else => TraceBranchSelection::Else,
            },
        },
        Fact::ChildStarted { child } => TraceNodeFact::ChildStarted {
            child: lash_trace::TraceLanguageChildExecution {
                scope: TraceRuntimeScope::none(),
                process_id: child.process_id,
                attempt: child.attempt,
                document: None,
            },
        },
    };
    TraceLanguageExecutionPayload::Node {
        at: observation.call_site.at,
        fact,
    }
}
