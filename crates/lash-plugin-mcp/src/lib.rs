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
//! rmcp 3.5.1 filters valid nested `x-mcp-header` annotations out of HTTP
//! tool catalogs before Lash receives them, so those tools are unavailable.
//! The 2026-07-28 Streamable HTTP specification permits nested paths reached
//! solely through `properties` (`basic/transports/streamable-http.mdx:388-398`),
//! but rmcp rejects them in `src/transport/common/mcp_headers.rs:155-163` and
//! only extracts top-level values at `:102-114`. Its HTTP worker filters the
//! list at `src/transport/streamable_http_client.rs:93-118`, emitting a warning
//! without exposing a structured rejection to Lash. This needs an upstream
//! SDK fix; hosts cannot recover a missing tool through a catalog diagnostic.
//!
//! Stdio receives retain partial lines across cancellation by the SDK service
//! loop. Framing and message compatibility use the SDK codec.
//!
//! Startup failures retain typed peer, runtime-clock deadline, exit and decode
//! observations through cleanup (`McpError::startup_observation`). The pool and
//! plugin expose the last startup failure through `startup_failure(server)`,
//! including when construction succeeds with an unavailable server. Raw content
//! is off by default. Hosts can opt into bounded stderr tails and invalid-line
//! prefixes with `diagnostics: { stderr_bytes, invalid_line_bytes }` on the
//! server configuration. The host owns the content it enables; Lash does not
//! redact it. Exit evidence distinguishes observations made before failure
//! from exits during cleanup, including forced termination.
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
//! Discovery instructions are server guidance, an admitted catalog resource:
//! every imported manifest pins its server's text, so a recorded catalog
//! carries the guidance it was admitted with. Tool refreshes retain it;
//! reconnects capture the new discovery response. The plugin's `server`
//! prompt section family contributes one section per offered server,
//! `mcp/server.<prefix>`, rendering the guidance the offered manifests pin
//! without contacting the server.
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
//! completion source or process adapter; deferred work and isolation refuse
//! before send.

mod call_failure;
pub mod config;
mod diagnostics;
pub mod error;
pub mod host;
pub mod naming;
pub mod plugin;
pub mod pool;
mod service_lifecycle;
mod stdio_transport;

pub use config::{
    McpCallPolicy, McpDataLimits, McpDiagnosticsPolicy, McpServerConfig, McpShutdownPolicy,
    McpStdioTransport, McpStreamableHttpTransport, McpTransport, ReconnectAttempts,
    TimeoutDisconnectPolicy,
};
pub use error::{
    McpDecodeCause, McpDiagnosticContent, McpError, McpExitObservation, McpInvalidLine, McpPeer,
    McpProtocolRevisionRefusal, McpStartupFailure, McpStartupObservation,
};
pub use host::{
    MCP_PROTOCOL_VERSION, McpElicitationHandler, McpElicitationParams, McpElicitationRequest,
    McpElicitationValidationError, McpRequestContext, McpRootsProvider, McpRootsRequest,
    McpSamplingHandler, McpSamplingRequest,
};
pub use plugin::{
    MCP_BINDING_FORMAT, MCP_RUN_OPTIONS, McpDeferredToolProvider, McpPluginFactory,
    McpPluginFactoryBuilder, McpToolProvider,
};
pub use pool::{McpConnectionPool, McpServerFault, McpServerHealth, McpServerStatus};
#[allow(
    deprecated,
    reason = "existing sampling and roots are retained during the MCP deprecation window"
)]
pub use rmcp::model::{
    CreateMessageRequestParams, CreateMessageResult as CreateMessageOutcome,
    ElicitResult as CreateElicitationOutcome, ElicitationAction, ElicitationCapability,
    ErrorData as McpProtocolError, FormElicitationCapability, Root, SamplingMessage,
    SamplingMessageContentBlock, UrlElicitationCapability,
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
