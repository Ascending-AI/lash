//! Binding assessment against the activation's prepared composition.

use super::*;
use lash_core_execution::runtime::process::steps::{InvocationTarget, ProcessStepTools};
use lash_execution_contracts::process::StepName;
use lash_kernel_value::names::OperationUse;

pub(super) enum StepBinding {
    Current {
        manifest: Box<crate::ToolManifest>,
        binding: Box<lash_core_execution::tool_run::AdmittedBinding>,
    },
    Drifted {
        tool: lash_vocabulary::tool_contract::ToolId,
        kind: crate::ToolSurfaceDriftKind,
    },
}

pub(super) fn linked(
    tools: &ProcessStepTools,
    step: &StepName,
    effect: &OperationUse,
    target: Option<&InvocationTarget>,
) -> Result<StepBinding, StepRefusal> {
    let unavailable =
        |error: lash_execution_contracts::plugin_error::PluginError| StepRefusal::Unavailable {
            step: step.0.clone(),
            reason: error.to_string(),
        };
    let link = match target {
        Some(target) => {
            let matched = crate::DeferredMatch {
                requested: target
                    .dispatch_requested
                    .clone()
                    .unwrap_or_else(|| effect.to_string()),
                outcome: crate::DeferredOutcome::Resolved {
                    link: target.route.clone(),
                },
            };
            let (link, binding) = tools.context.matched_link(&matched).map_err(unavailable)?;
            if let Err(reason) = binding {
                return Err(StepRefusal::CompositionUnavailable { reason });
            }
            link
        }
        None => tools.context.step_link(effect).map_err(unavailable)?,
    };
    match link {
        crate::plugin::StepLink::Served {
            manifest, binding, ..
        } => Ok(StepBinding::Current { manifest, binding }),
        crate::plugin::StepLink::Drifted { tool, kind } => Ok(StepBinding::Drifted { tool, kind }),
        crate::plugin::StepLink::Unlinked => Err(StepRefusal::UnknownTool {
            step: step.0.clone(),
            tool: effect.to_string(),
        }),
        crate::plugin::StepLink::Unreadable { reason, .. } => {
            Err(StepRefusal::CompositionUnavailable { reason })
        }
        crate::plugin::StepLink::ProviderUnregistered { provider } => {
            Err(StepRefusal::CompositionUnavailable {
                reason: format!("the call's provider `{provider}` is not registered on this node"),
            })
        }
    }
}
