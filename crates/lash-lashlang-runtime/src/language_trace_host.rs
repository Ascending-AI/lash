use lash_trace::{TraceBranchSelection, TraceLanguageExecutionPayload, TraceRuntimeScope};

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
                retry_policy: effect.retry_policy,
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
pub struct LanguageTraceHost<H, O> {
    host: H,
    observer: O,
}

impl<H, O> LanguageTraceHost<H, O> {
    pub fn new(host: H, observer: O) -> Self {
        Self { host, observer }
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

    async fn yield_now(&self) {
        self.host.yield_now().await;
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

    fn observe_lashlang_execution(&self, observation: lashlang::LashlangExecutionObservation) {
        let payload = match observation {
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
                    incarnation: child.incarnation,
                    attempt: child.attempt,
                    module_ref: Some(child.module_ref.to_string()),
                    entry_ref: Some(lashlang::process_ref_key(&child.process_ref)),
                    entry_name: Some(child.process_name),
                },
            },
        };
        (self.observer)(&self.host, payload);
    }

    fn take_lashlang_effect_failure(
        &self,
        call_site: &lashlang::LashlangExecutionCallSite,
    ) -> Option<lashlang::LashlangEffectFailure> {
        self.host.take_lashlang_effect_failure(call_site)
    }
}
