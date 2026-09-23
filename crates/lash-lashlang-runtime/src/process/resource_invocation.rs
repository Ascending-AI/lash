use super::{LashlangHostError, LashlangProcessHost, resolve_lashlang_module_operation};
use lashlang::ExecutionHostError;

pub(super) enum PreparedResourceInvocation {
    Trigger {
        operation: lashlang::TriggerHostOperation,
        payload: serde_json::Value,
        effect_id: String,
        host_operation: String,
        call_site: Option<lashlang::LashlangExecutionCallSite>,
    },
    Tool {
        invocation: lash_core::facade_support::ToolInvocation,
        host_operation: String,
        call_site: Option<lashlang::LashlangExecutionCallSite>,
    },
}

impl LashlangProcessHost<'_> {
    /// Resolves one resource operation to a trigger operation or a tool call,
    /// before anything reaches the host. `call_id` is the operation's
    /// positional id and `journal_key` the key a trigger operation journals
    /// under (FIG-3586); the call site is trace and summary metadata only.
    pub(super) fn prepare_resource_invocation(
        &self,
        operation: String,
        receiver: lashlang::Value,
        args: Vec<lashlang::Value>,
        call_site: Option<lashlang::LashlangExecutionCallSite>,
        call_id: String,
        journal_key: String,
    ) -> Result<PreparedResourceInvocation, ExecutionHostError> {
        let receiver = match &receiver {
            lashlang::Value::Resource(receiver) => receiver,
            _ => {
                return Err(LashlangHostError::ModuleAuthorityRequired { operation }.into());
            }
        };
        let host_operation =
            resolve_lashlang_module_operation(&self.host_environment, receiver, &operation)?;
        // Every leaf carries its call site (the one compile entry tracks
        // execution sites): the trace correlates the positional id with the
        // node through it, and a leaf without one is a defect upstream.
        let Some(site) = call_site.as_ref() else {
            return Err(LashlangHostError::OperationCallSiteMissing {
                operation,
                host_operation,
            }
            .into());
        };
        let payload = self.resource_payload(&args)?;
        self.lashlang_execution_trace
            .record_resource_call(site, &call_id);
        if let Some(operation) =
            lashlang::TriggerHostOperation::from_host_operation(&host_operation)
        {
            return Ok(PreparedResourceInvocation::Trigger {
                operation,
                payload,
                effect_id: journal_key,
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
            lash_core::facade_support::ToolInvocation::new(call_id, manifest.id.clone(), payload);
        if let Some(call_site) = &call_site {
            invocation = invocation.with_issuing_language_node_id(call_site.site.node_id.clone());
            if let Some(hook) = self
                .lashlang_execution_trace
                .tool_child_execution_trace_hook(call_site.clone())
            {
                invocation = invocation.with_child_execution_trace_hook(hook);
            }
        }
        Ok(PreparedResourceInvocation::Tool {
            invocation,
            host_operation,
            call_site,
        })
    }
}
