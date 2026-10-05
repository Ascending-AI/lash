use super::{LashlangHostError, LashlangProcessHost, resolve_lashlang_module_operation};
use lashlang::ExecutionHostError;

/// The attribution of one prepared operation, retained through settlement.
pub(super) struct PreparedResourceCall {
    pub(super) host_operation: String,
    pub(super) call_site: lashlang::LashlangExecutionCallSite,
    pub(super) operand_index: Option<usize>,
    pub(super) journal_key: String,
}

pub(super) enum PreparedResourceInvocation {
    Trigger {
        operation: lashlang::TriggerHostOperation,
        payload: serde_json::Value,
        call: PreparedResourceCall,
    },
    Tool {
        invocation: lash_core::facade_support::ToolInvocation,
        call: PreparedResourceCall,
    },
}

impl LashlangProcessHost<'_> {
    /// Resolves one resource operation to a trigger operation or a tool call,
    /// before anything reaches the host. `call_id` is the operation's
    /// positional id and `journal_key` the key a trigger operation journals
    /// under (FIG-3586); the call site is trace and summary metadata only.
    pub(super) fn prepare_resource_invocation(
        &self,
        operation: lashlang::ResourceOperation,
        call_id: lash_core::ToolCallId,
        journal_key: String,
        operand_index: Option<usize>,
    ) -> Result<PreparedResourceInvocation, ExecutionHostError> {
        let lashlang::ResourceOperation {
            operation,
            receiver,
            args,
            call_site,
        } = operation;
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
                call: PreparedResourceCall {
                    host_operation,
                    call_site: site.clone(),
                    operand_index,
                    journal_key,
                },
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
        invocation = invocation.with_issuing_language_node_id(site.site.node_id.clone());
        if self.lashlang_execution_trace.tracing.observes_language() {
            self.ctx.record_language_call_attribution(
                invocation.id.clone(),
                crate::LASHLANG_ENGINE_KIND,
                self.lashlang_execution_trace.identity(),
                site.site.node_id.clone(),
                site.occurrence,
            );
        }
        Ok(PreparedResourceInvocation::Tool {
            invocation,
            call: PreparedResourceCall {
                host_operation,
                call_site: site.clone(),
                operand_index,
                journal_key,
            },
        })
    }
}
