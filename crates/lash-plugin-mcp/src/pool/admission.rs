use super::*;
use serde::{Deserialize, Serialize};

pub(crate) const MCP_BINDING_KEY: &str = "lash.mcp";

/// A socket supplies only inline attempts. MCP tasks need an authenticated
/// durable source adapter; advertising task support does not supply one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemoteCompletion {
    Inline,
    UnsupportedTask,
}

/// Recorded with K1's manifest. Connection generations and JSON-RPC request
/// ids are deliberately absent: a reconnect may redeliver the same attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpToolBinding {
    pub server: String,
    pub native_tool: String,
    pub transport_digest: String,
    pub peer_digest: String,
    pub tool_digest: String,
    pub call_policy: crate::McpCallPolicy,
    pub completion: RemoteCompletion,
}

fn digest(value: &impl Serialize) -> Result<String, McpError> {
    let value = serde_json::to_value(value)
        .map_err(|error| McpError::Config(format!("cannot record MCP binding: {error}")))?;
    let bytes = serde_json::to_vec(&value)
        .map_err(|error| McpError::Config(format!("cannot record MCP binding: {error}")))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

pub(super) fn peer_digest(peer: &Peer<RoleClient>) -> Result<String, McpError> {
    let info = peer
        .peer_info()
        .ok_or_else(|| McpError::Protocol("MCP initialize identity is unavailable".into()))?;
    digest(&(
        &info.protocol_version,
        &info.server_info,
        &info.capabilities,
    ))
}

pub(super) fn bind_imported_tools(
    mut tools: BTreeMap<String, ImportedTool>,
    entry: &McpEntry,
    peer: &Peer<RoleClient>,
) -> Result<BTreeMap<String, ImportedTool>, McpError> {
    let transport_digest = digest(&entry.config.transport)?;
    let peer_digest = peer_digest(peer)?;
    for tool in tools.values_mut() {
        let binding = McpToolBinding {
            server: entry.server_name.clone(),
            native_tool: tool.original_name.clone(),
            transport_digest: transport_digest.clone(),
            peer_digest: peer_digest.clone(),
            tool_digest: tool.tool_digest.clone(),
            call_policy: entry.config.call_policy().clone(),
            completion: tool.completion.clone(),
        };
        tool.definition.manifest.bindings.insert(
            MCP_BINDING_KEY.into(),
            serde_json::to_value(binding)
                .map_err(|error| McpError::Config(format!("cannot record MCP binding: {error}")))?,
        );
    }
    super::guidance::pin_guidance(&mut tools, entry, peer)?;
    Ok(tools)
}

pub(super) fn tool_digest(tool: &rmcp::model::Tool) -> Result<String, McpError> {
    digest(tool)
}

pub(crate) fn admitted_binding(
    manifest: &lash_core::ToolManifest,
) -> Result<McpToolBinding, ToolOutcome> {
    let binding = manifest
        .bindings
        .get(MCP_BINDING_KEY)
        .cloned()
        .unwrap_or(Value::Null);
    serde_json::from_value(binding.clone()).map_err(|_| {
        McpCallFailure::InvalidExecutionBinding {
            tool_id: manifest.id.to_string(),
            binding,
        }
        .into()
    })
}

impl McpConnectionPool {
    pub(crate) fn prepare_mcp_call(
        &self,
        call: lash_core::ToolPrepareCall<'_>,
    ) -> Result<lash_core::PreparedToolCall, ToolOutcome> {
        let target = self.lookup_by_id(&call.tool_id).ok_or_else(|| {
            ToolOutcome::from(McpCallFailure::UnknownToolId {
                tool_id: call.tool_id.to_string(),
            })
        })?;
        let binding = target.binding.ok_or_else(|| {
            ToolOutcome::from(McpCallFailure::InvalidExecutionBinding {
                tool_id: call.tool_id.to_string(),
                binding: Value::Null,
            })
        })?;
        if binding.completion != RemoteCompletion::Inline {
            return Err(McpCallFailure::UnsupportedRemoteCompletion {
                server: binding.server,
                tool_id: call.tool_id.to_string(),
            }
            .into());
        }
        let payload = serde_json::to_value(&binding).map_err(|_| {
            ToolOutcome::from(McpCallFailure::InvalidExecutionBinding {
                tool_id: call.tool_id.to_string(),
                binding: Value::Null,
            })
        })?;
        Ok(
            lash_core::PreparedToolCall::identity(call.tool_id, call.pending)
                .with_prepared_payload(payload),
        )
    }

    pub(crate) async fn call_admitted_tool(&self, call: lash_core::ToolCall<'_>) -> ToolOutcome {
        if self.shut_down.load(Ordering::SeqCst) {
            return pool_shut_down_failure();
        }
        let manifest = call.manifest();
        let binding = match admitted_binding(manifest) {
            Ok(binding) => binding,
            Err(failure) => return failure,
        };
        let declaration = manifest.declaration();
        if declaration.may_defer
            || declaration.isolated
            || binding.completion != RemoteCompletion::Inline
        {
            return McpCallFailure::UnsupportedRemoteCompletion {
                server: binding.server,
                tool_id: manifest.id.to_string(),
            }
            .into();
        }
        let prepared = match call.context.decode_prepared_payload::<McpToolBinding>() {
            Ok(prepared) => prepared,
            Err(_) => {
                return McpCallFailure::InvalidExecutionBinding {
                    tool_id: manifest.id.to_string(),
                    binding: call.context.prepared_payload().clone(),
                }
                .into();
            }
        };
        if prepared != binding {
            return McpCallFailure::ExecutionBindingChanged {
                server: prepared.server,
                tool_id: manifest.id.to_string(),
            }
            .into();
        }
        let Some(target) = self.lookup_by_id(&manifest.id) else {
            return McpCallFailure::UnknownToolId {
                tool_id: manifest.id.to_string(),
            }
            .into();
        };
        if target.binding.as_ref() != Some(&binding) {
            return McpCallFailure::ExecutionBindingChanged {
                server: binding.server,
                tool_id: manifest.id.to_string(),
            }
            .into();
        }
        self.pause_after_target_resolution().await;
        self.call_resolved_tool(target, call.args, call.context)
            .await
    }
}
