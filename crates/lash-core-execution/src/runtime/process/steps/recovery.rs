//! Current repeat veto and binding drift for an already admitted step.

use super::*;

#[derive(Clone, Debug)]
pub enum StepAssessment {
    Current {
        tool: ToolId,
        policy: ExecutionPolicy,
    },
    Drifted {
        tool: ToolId,
        kind: crate::ToolSurfaceDriftKind,
    },
}

/// The typed pre-body failure recorded at an existing step's ordinal.
#[expect(
    clippy::expect_used,
    reason = "a typed tool failure is serializable data"
)]
pub fn binding_drift_output(
    process: &ProcessId,
    effect: &lash_kernel_value::names::OperationUse,
    tool: ToolId,
    kind: crate::ToolSurfaceDriftKind,
) -> SettledOutput {
    let effect = effect.to_string();
    let (code, cause, message) = match kind {
        crate::ToolSurfaceDriftKind::Missing => (
            "linked_tool_missing",
            crate::ToolFailureCause::LinkedToolMissing {
                effect: effect.clone(),
                tool: tool.clone(),
            },
            format!(
                "the effect `{effect}` is linked to tool `{tool}`, which is missing from what this node's provider serves since the process's env linked it; nothing ran"
            ),
        ),
        crate::ToolSurfaceDriftKind::ContractChanged { pinned, live } => (
            "operation_contract_changed",
            crate::ToolFailureCause::OperationContractChanged {
                effect: effect.clone(),
                tool: tool.clone(),
                pinned: pinned.clone(),
                live: live.clone(),
            },
            crate::operation_contract_changed(&effect, &tool, &pinned, &live).message,
        ),
    };
    let failure = crate::ToolFailure::runtime(crate::ToolFailureClass::Internal, code, message)
        .with_cause(cause);
    let text = serde_json::to_string(&crate::Durable(&crate::ToolCallOutput::failure(failure)))
        .expect("a binding drift failure encodes");
    SettledOutput::Failed(process_material(process, text).failure(
        lash_core_store::tool_run::KnownFailureReason::Reported,
        None,
    ))
}
