use super::*;
use serde::{Deserialize, Serialize};

/// A socket supplies only inline attempts. MCP tasks need an authenticated
/// durable source adapter; advertising task support does not supply one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemoteCompletion {
    Inline,
}

/// The codec payload a link to an MCP tool records, in
/// [`crate::MCP_BINDING_FORMAT`]: the server and the durable tool id its
/// calls go to. Transport, peer and server version are instance facts: a
/// call is served by whichever connection the server has now, and nothing
/// compares them (the one dispatch check is the tool's contract id).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum McpLinkPayload {
    Mcp { server: String, tool_id: String },
}

fn digest(value: &impl Serialize) -> Result<String, McpError> {
    let value = serde_json::to_value(value)
        .map_err(|error| McpError::Config(format!("cannot tag an MCP tool: {error}")))?;
    let bytes = serde_json::to_vec(&value)
        .map_err(|error| McpError::Config(format!("cannot tag an MCP tool: {error}")))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

/// The host tag of `tool`, imported as `tool_id`: its durable id and a
/// digest of its name and schemas. Its description and annotations are not
/// part of it, so rewording a tool changes no contract.
pub(super) fn contract_tag(
    tool_id: &str,
    tool: &rmcp::model::Tool,
) -> Result<lash_core::HostTag, McpError> {
    let schemas = digest(&(&tool.name, &tool.input_schema, &tool.output_schema))?;
    Ok(lash_core::HostTag::new(format!("{tool_id}#{schemas}")))
}

/// `tools` with the guidance `peer`'s discover instructions pin.
pub(super) fn pinned_guidance(
    mut tools: BTreeMap<String, ImportedTool>,
    entry: &McpEntry,
    peer: &Peer<RoleClient>,
) -> Result<BTreeMap<String, ImportedTool>, McpError> {
    super::guidance::pin_guidance(&mut tools, entry, peer)?;
    Ok(tools)
}

/// The server a call of `tool_id` goes to, as its link's payload names it.
///
/// # Errors
///
/// The payload is absent, not MCP's, or names another tool.
pub(crate) fn linked_server(
    binding: Option<&lash_core::ExecutionBinding>,
    tool_id: &lash_core::ToolId,
) -> Result<String, ToolOutcome> {
    let payload = binding.map_or(&Value::Null, |binding| &binding.payload);
    match serde_json::from_value::<McpLinkPayload>(payload.clone()) {
        Ok(McpLinkPayload::Mcp {
            server,
            tool_id: linked,
        }) if linked == tool_id.as_str() => Ok(server),
        _ => Err(McpCallFailure::InvalidExecutionBinding {
            tool_id: tool_id.to_string(),
            binding: payload.clone(),
        }
        .into()),
    }
}

impl McpConnectionPool {
    /// The payload a link to the imported tool `tool_id` records.
    pub(crate) fn link_payload(&self, tool_id: &lash_core::ToolId) -> Option<Value> {
        let target = self.lookup_by_id(tool_id)?;
        serde_json::to_value(McpLinkPayload::Mcp {
            server: target.entry.server_name.clone(),
            tool_id: tool_id.to_string(),
        })
        .ok()
    }

    pub(crate) fn prepare_mcp_call(
        &self,
        call: lash_core::ToolPrepareCall<'_>,
    ) -> Result<lash_core::PreparedToolCall, ToolOutcome> {
        let target = self.lookup_by_id(&call.tool_id).ok_or_else(|| {
            ToolOutcome::from(McpCallFailure::UnknownToolId {
                tool_id: call.tool_id.to_string(),
            })
        })?;
        if target.completion != RemoteCompletion::Inline {
            return Err(McpCallFailure::UnsupportedRemoteCompletion {
                server: target.entry.server_name.clone(),
                tool_id: call.tool_id.to_string(),
            }
            .into());
        }
        Ok(lash_core::PreparedToolCall::identity(
            call.tool_id,
            call.pending,
        ))
    }

    /// Run an admitted call on the server its link names, as that server
    /// serves the tool now. An admitted call, `Once` included, may be
    /// redelivered to a redeployed server whose tool kept its contract.
    pub(crate) async fn call_admitted_tool(&self, call: lash_core::ToolCall<'_>) -> ToolOutcome {
        if self.shut_down.load(Ordering::SeqCst) {
            return pool_shut_down_failure();
        }
        let manifest = call.manifest();
        let server = match linked_server(call.context.tool_execution_binding(), &manifest.id) {
            Ok(server) => server,
            Err(failure) => return failure,
        };
        let Some(target) = self.lookup_by_id(&manifest.id) else {
            return McpCallFailure::UnknownToolId {
                tool_id: manifest.id.to_string(),
            }
            .into();
        };
        if target.entry.server_name != server {
            return McpCallFailure::InvalidExecutionBinding {
                tool_id: manifest.id.to_string(),
                binding: call
                    .context
                    .tool_execution_binding()
                    .map_or(Value::Null, |binding| binding.payload.clone()),
            }
            .into();
        }
        let declaration = manifest.declaration();
        if declaration.may_defer
            || declaration.isolated
            || target.completion != RemoteCompletion::Inline
        {
            return McpCallFailure::UnsupportedRemoteCompletion {
                server,
                tool_id: manifest.id.to_string(),
            }
            .into();
        }
        self.pause_after_target_resolution().await;
        self.call_resolved_tool(target, call.args, call.context)
            .await
    }
}
