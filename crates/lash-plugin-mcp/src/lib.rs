//! MCP (Model Context Protocol) integration for `lash`, packaged as a plugin.
//!
//! `lash-plugin-mcp` exposes MCP-compatible servers as a normal lash tool
//! provider. Add the plugin once at [`LashCore::builder`] time, supply a
//! `BTreeMap<String, McpServerConfig>`, and every session built from the
//! core gets the configured servers' tools surfaced under
//! `mcp__<server>__<tool>` names.
//!
//! Supported transports (selected per server via the `transport` field):
//! - `stdio` — spawn a child process and speak JSON-RPC over its pipes.
//! - `streamable_http` — HTTP/JSON streaming transport (newer MCP spec). This
//!   is also how SSE-capable servers are reached: the current MCP HTTP
//!   transport negotiates SSE responses itself.
//!
//! Implementation note: the wire-level client is provided by the official
//! [`rmcp`] SDK. The plugin owns a single connection pool (`McpConnectionPool`)
//! that is shared across every session built from the same `LashCore`, so
//! e.g. stdio servers are spawned once per process rather than per session.

pub mod config;
pub mod error;
pub mod naming;
pub mod plugin;
pub mod pool;

pub use config::McpServerConfig;
pub use error::McpError;
pub use plugin::{McpDeferredToolProvider, McpPluginFactory, McpToolProvider};
pub use pool::{McpConnectionPool, McpServerStatus};
