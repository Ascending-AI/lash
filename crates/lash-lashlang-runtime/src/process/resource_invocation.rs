use super::{LashlangHostError, LashlangProcessHost, resolve_lashlang_module_operation};
use lashlang::ExecutionHostError;

pub(super) enum PreparedResourceInvocation {
    Trigger {
        operation: lashlang::TriggerHostOperation,
        payload: serde_json::Value,
        effect_id: String,
        host_operation: String,
        call_site: lashlang::LashlangExecutionCallSite,
    },
    Tool {
        invocation: lash_core::facade_support::ToolInvocation,
        host_operation: String,
        call_site: lashlang::LashlangExecutionCallSite,
    },
}

impl LashlangProcessHost<'_> {
    pub(super) fn prepare_resource_invocation(
        &self,
        operation: String,
        receiver: lashlang::Value,
        args: Vec<lashlang::Value>,
        call_site: Option<lashlang::LashlangExecutionCallSite>,
        batch_index: Option<usize>,
    ) -> Result<PreparedResourceInvocation, ExecutionHostError> {
        let receiver = match &receiver {
            lashlang::Value::Resource(receiver) => receiver,
            _ => {
                return Err(LashlangHostError::ModuleAuthorityRequired { operation }.into());
            }
        };
        let host_operation =
            resolve_lashlang_module_operation(&self.host_environment, receiver, &operation)?;
        let payload = self.resource_payload(&args)?;
        let call_site = call_site.ok_or_else(|| {
            ExecutionHostError::from(LashlangHostError::OperationCallSiteMissing {
                operation: operation.clone(),
                host_operation: host_operation.clone(),
            })
        })?;
        let call_id = self.resource_tool_call_id(&host_operation, &call_site, batch_index);
        if let Some(operation) =
            lashlang::TriggerHostOperation::from_host_operation(&host_operation)
        {
            return Ok(PreparedResourceInvocation::Trigger {
                operation,
                payload,
                effect_id: call_id,
                host_operation,
                call_site,
            });
        }
        let tool_id = lash_core::ToolId::from(host_operation.as_str());
        let manifest = self
            .ctx
            .callable_tool_manifest_by_id(&tool_id)
            .ok_or_else(|| {
                ExecutionHostError::from(LashlangHostError::ResolvedOperationUnavailable {
                    operation,
                    host_operation: host_operation.clone(),
                })
            })?;
        let mut invocation =
            lash_core::facade_support::ToolInvocation::new(call_id, manifest.id.clone(), payload)
                .with_issuing_language_node_id(call_site.site.node_id.clone());
        if let Some(hook) = self
            .lashlang_execution_trace
            .tool_child_execution_trace_hook(call_site.clone())
        {
            invocation = invocation.with_child_execution_trace_hook(hook);
        }
        Ok(PreparedResourceInvocation::Tool {
            invocation,
            host_operation,
            call_site,
        })
    }
}
