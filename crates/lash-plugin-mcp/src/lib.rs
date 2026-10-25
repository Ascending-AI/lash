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
//! The upstream `rmcp` SDK validates and mirrors nested `x-mcp-header`
//! annotations reached solely through `properties` at their exact paths.
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
//! Host seams for server-to-client requests (sampling, roots, elicitation) take
//! and return Lash-owned values: [`McpSamplingParams`], [`McpSamplingResult`],
//! [`McpRoot`], [`McpElicitationResult`] and the shared [`McpHostRefusal`].
//! The SDK is an implementation detail of this adapter and is not re-exported:
//! conversion and validation happen at the wire, and SDK input these values
//! cannot carry is refused to the server as invalid params.
//!
//! Implementation note: the wire-level client is provided by the official
//! `rmcp` SDK. The plugin owns a single connection pool (`McpConnectionPool`)
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

#![deny(clippy::future_not_send)]

mod call_failure;
pub mod config;
mod diagnostics;
pub mod error;
pub mod events;
pub mod host;
pub mod naming;
pub mod plugin;
pub mod pool;
mod service_lifecycle;
mod stdio_transport;

pub use config::{
    McpCallPolicy, McpDataLimits, McpDiagnosticsPolicy, McpServerConfig, McpShutdownPolicy,
    McpStdioTransport, McpStreamableHttpTransport, McpTracePropagation, McpTransport,
    ReconnectAttempts, TimeoutDisconnectPolicy,
};
pub use error::{
    McpDecodeCause, McpDiagnosticContent, McpError, McpExitObservation, McpInvalidLine, McpPeer,
    McpProtocolRevisionRefusal, McpStartupFailure, McpStartupObservation,
};
pub use host::protocol::{
    McpContextInclusion, McpElicitationModes, McpElicitationResult, McpHostRefusal,
    McpModelPreferences, McpRefusalKind, McpRole, McpRoot, McpSamplingContent, McpSamplingMessage,
    McpSamplingParams, McpSamplingResult, McpStopReason, McpToolChoice,
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

/// Each raw native tool name of one server's current catalog as the
/// effect name of its canonical operation (`alpha.get_user`): the server's
/// canonical segment, then the tool's canonical name. Every member of a
/// cleanup or length collision receives an `_h` and eight-character
/// durable-id suffix. Refresh can rename tools without changing their
/// durable ids.
pub fn mcp_tool_names(
    server_name: &str,
    native_tool_names: &[&str],
) -> std::collections::BTreeMap<String, String> {
    let server = naming::server_segment(server_name);
    naming::build_catalog_names(server_name, native_tool_names)
        .into_iter()
        .map(|(raw, name)| (raw, format!("{server}.{name}")))
        .collect()
}

#[cfg(test)]
mod client_depth_tests;

#[cfg(test)]
mod header_path_tests;

#[cfg(test)]
mod observation_tests;

mod observations;

pub use observations::{McpRoutePolicy, McpRoutePolicyError};
