//! A tool body's annotations of its own tool span (§7.4 rule 2).

use std::sync::Arc;

use lash_trace::{AttrKey, TraceOmission, TraceValue};

use super::body::BodyTraceState;

/// The one bounded exception to "a plugin cannot change a lash-owned span":
/// a tool body may annotate its own `execute_tool` span, as the MCP
/// semantic conventions require of an MCP client under an outer tool span.
///
/// Accepted keys are `mcp.*`, `jsonrpc.*`, `rpc.*`, `network.*`,
/// `server.*`, and the refinable tool defaults `gen_ai.tool.type` and
/// `error.type`; any other key is refused with
/// [`TraceOmission::DisallowedKey`]. Each annotation is charged to the
/// body's budget, to `trace_limits.tool_span_attributes` and to the tool
/// span's contribution budget; a refinable key charges bytes only. The
/// annotations stay in memory with the attempt that set them, and the
/// span's closer stamps them: they are never persisted.
#[derive(Clone)]
pub struct ToolSpanAttributes {
    body: Option<Arc<BodyTraceState>>,
}

impl ToolSpanAttributes {
    pub(super) fn new(body: Option<Arc<BodyTraceState>>) -> Self {
        Self { body }
    }

    /// Sets one annotation of the tool span.
    pub fn set(
        &self,
        key: impl Into<AttrKey>,
        value: impl Into<TraceValue>,
    ) -> Result<(), TraceOmission> {
        match &self.body {
            Some(body) => body.annotate(key.into(), value.into()),
            None => Err(TraceOmission::DisallowedKey),
        }
    }
}

impl std::fmt::Debug for ToolSpanAttributes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolSpanAttributes")
            .field("bound", &self.body.is_some())
            .finish()
    }
}
