//! MCP (Model Context Protocol) integration for `lash`, packaged as a plugin.
//!
//! `lash-plugin-mcp` exposes MCP-compatible servers as a normal lash tool provider.
//!
//! Supported transports (selected per server via the `transport` field):
//! - `stdio` — spawn a child process and speak JSON-RPC over its pipes.
//! - `streamable_http` — HTTP/JSON streaming transport (newer MCP spec). This
//!   transport supports static headers only. Lash does not enable rmcp OAuth
//!   authentication or token refresh; hosts must supply and rotate credentials.
//!   It is also how SSE-capable servers are reached: the current MCP HTTP
//!   transport negotiates SSE responses itself.
//!
//! Stdio receives retain partial lines across cancellation by the SDK service
//! loop. Framing and message compatibility use the SDK codec.
//!
//! Implementation note: the wire-level client is provided by the official
//! [`rmcp`] SDK. The plugin owns a single connection pool (`McpConnectionPool`)
//! that is shared across every session built from the same `LashCore`, so
//! e.g. stdio servers are spawned once per process rather than per session.

//! Tool-list notifications return after marking one latest dirty signal. The
//! entry's lifecycle actor owns one refresh, with at most one pending repeat,
//! and cancels it on disconnect or shutdown. Startup and refresh discovery
//! accept at most 64 pages, 4,096 tools and 8 MiB of serialized page data in
//! total, including cursors and metadata. Cursor cycles and catalogs exceeding
//! a limit are refused without replacing the last installed catalog. A stalled
//! refresh closes its service at the startup timeout and reconnects, retaining
//! the last catalog while clearing unanswered protocol requests.
//!
//! Initialize instructions are captured as each server's tool-module metadata.
//! Tool refreshes retain them; reconnects capture the new initialize response.
//! Hosts can read them from advertised manifests for discovery. Standard and
//! RLM prompts render them once per visible module from the recorded catalog.
//!
//! Preparation seals the manifest's server, native tool, transport and peer
//! identity, tool contract, timeout policy and inline completion capability
//! in the canonical prepared payload. An attempt requires that sealed binding
//! to agree with both its manifest and its target; a missing payload refuses.
//! Replays refuse a changed binding before sending. Each `tools/call` carries
//! `lash.dev/tool-call-id` and `lash.dev/tool-attempt` in `_meta`; JSON-RPC ids
//! correlate deliveries only. Servers must implement deduplication by the Lash
//! call id to suppress repeated remote effects after an unrecorded result.
//! Transport timeouts are body failures. Cancellation is advisory to the
//! server and records a local cancelled attempt, with no remote termination
//! claim. Neither a socket nor a non-resident catalog grant supplies a durable
//! completion source or process adapter; required remote tasks, deferred work
//! and isolation refuse before send.

mod call_failure;
pub mod config;
pub mod error;
pub mod host;
pub mod naming;
pub mod plugin;
pub mod pool;
mod service_lifecycle;
mod stdio_transport;

pub use config::{
    McpCallPolicy, McpServerConfig, McpShutdownPolicy, McpStdioTransport,
    McpStreamableHttpTransport, McpTransport, TimeoutDisconnectPolicy,
};
pub use error::McpError;
pub use host::{
    MCP_PROTOCOL_VERSION, McpElicitationHandler, McpElicitationRequest,
    McpElicitationValidationError, McpNotificationContext, McpRequestContext, McpRootsProvider,
    McpRootsRequest, McpSamplingHandler, McpSamplingRequest, McpUrlElicitationComplete,
};
pub use plugin::{
    McpDeferredToolProvider, McpPluginFactory, McpPluginFactoryBuilder, McpToolProvider,
};
pub use pool::{McpConnectionPool, McpServerFault, McpServerHealth, McpServerStatus};
pub use rmcp::model::{
    CreateElicitationRequestParams, CreateElicitationResult as CreateElicitationOutcome,
    CreateMessageRequestParams, CreateMessageResult as CreateMessageOutcome, ElicitationAction,
    ElicitationCapability, ErrorData as McpProtocolError, FormElicitationCapability, Root,
    SamplingMessage, SamplingMessageContent, UrlElicitationCapability,
};

/// Model-facing names keyed by raw native tool name for one server's current
/// catalog. Cleanup preserves tool case; all members of a cleanup or length
/// collision receive an eight-character durable-id suffix. Refresh can rename
/// tools without changing their durable ids.
pub fn mcp_tool_names(
    server_name: &str,
    native_tool_names: &[&str],
) -> std::collections::BTreeMap<String, String> {
    naming::build_catalog_names(server_name, native_tool_names)
        .into_iter()
        .map(|(raw, (name, _))| (raw, name))
        .collect()
}

#[cfg(test)]
mod client_depth_tests;
