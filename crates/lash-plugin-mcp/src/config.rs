use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::McpError;

const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_CALL_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_CALL_MAX_TOTAL_TIMEOUT_MS: u64 = 600_000;
const DEFAULT_LIVENESS_PROBE_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_CONSECUTIVE_TIMEOUTS_BEFORE_DISCONNECT: u64 = 3;
const DEFAULT_RECONNECT_INITIAL_BACKOFF_MS: u64 = 500;
const DEFAULT_RECONNECT_MAX_BACKOFF_MS: u64 = 30_000;

/// How an idle tool-call timeout affects the MCP connection.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TimeoutDisconnectPolicy {
    /// A timeout never changes connection state.
    Never,
    /// Probe the peer with MCP `ping`; disconnect only when the probe fails.
    #[default]
    PingProbe,
    /// Disconnect after the configured number of consecutive call timeouts.
    ConsecutiveTimeouts,
}

fn default_startup_timeout_ms() -> u64 {
    DEFAULT_STARTUP_TIMEOUT_MS
}

fn default_call_timeout_ms() -> u64 {
    DEFAULT_CALL_TIMEOUT_MS
}

fn default_call_max_total_timeout_ms() -> u64 {
    DEFAULT_CALL_MAX_TOTAL_TIMEOUT_MS
}

fn default_true() -> bool {
    true
}

fn default_timeout_disconnect_policy() -> TimeoutDisconnectPolicy {
    TimeoutDisconnectPolicy::default()
}

fn default_liveness_probe_timeout_ms() -> u64 {
    DEFAULT_LIVENESS_PROBE_TIMEOUT_MS
}

fn default_consecutive_timeouts_before_disconnect() -> u64 {
    DEFAULT_CONSECUTIVE_TIMEOUTS_BEFORE_DISCONNECT
}

fn default_reconnect_initial_backoff_ms() -> u64 {
    DEFAULT_RECONNECT_INITIAL_BACKOFF_MS
}

fn default_reconnect_max_backoff_ms() -> u64 {
    DEFAULT_RECONNECT_MAX_BACKOFF_MS
}

fn is_default_startup_timeout_ms(value: &u64) -> bool {
    *value == DEFAULT_STARTUP_TIMEOUT_MS
}

fn is_default_call_timeout_ms(value: &u64) -> bool {
    *value == DEFAULT_CALL_TIMEOUT_MS
}

fn is_default_call_max_total_timeout_ms(value: &u64) -> bool {
    *value == DEFAULT_CALL_MAX_TOTAL_TIMEOUT_MS
}

fn is_true(value: &bool) -> bool {
    *value
}

fn is_default_timeout_disconnect_policy(value: &TimeoutDisconnectPolicy) -> bool {
    *value == TimeoutDisconnectPolicy::default()
}

fn is_default_liveness_probe_timeout_ms(value: &u64) -> bool {
    *value == DEFAULT_LIVENESS_PROBE_TIMEOUT_MS
}

fn is_default_consecutive_timeouts_before_disconnect(value: &u64) -> bool {
    *value == DEFAULT_CONSECUTIVE_TIMEOUTS_BEFORE_DISCONNECT
}

fn is_default_reconnect_initial_backoff_ms(value: &u64) -> bool {
    *value == DEFAULT_RECONNECT_INITIAL_BACKOFF_MS
}

fn is_default_reconnect_max_backoff_ms(value: &u64) -> bool {
    *value == DEFAULT_RECONNECT_MAX_BACKOFF_MS
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Timeout, liveness, and reconnect behavior shared by every MCP transport.
///
/// This policy is flattened into [`McpServerConfig`]'s serialized shape, so
/// existing configuration keys remain at the server level while future policy
/// additions do not add fields to every transport variant.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpCallPolicy {
    /// Idle timeout for one tool call, in milliseconds.
    #[serde(
        default = "default_call_timeout_ms",
        skip_serializing_if = "is_default_call_timeout_ms"
    )]
    pub call_timeout_ms: u64,
    /// Mandatory total wall-clock cap for one tool call, in milliseconds.
    #[serde(
        default = "default_call_max_total_timeout_ms",
        skip_serializing_if = "is_default_call_max_total_timeout_ms"
    )]
    pub call_max_total_timeout_ms: u64,
    /// Whether matching progress notifications reset the idle timeout.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub reset_call_timeout_on_progress: bool,
    /// How an idle timeout affects connection health.
    #[serde(
        default = "default_timeout_disconnect_policy",
        skip_serializing_if = "is_default_timeout_disconnect_policy"
    )]
    pub timeout_disconnect_policy: TimeoutDisconnectPolicy,
    /// Maximum wait for a liveness-probe answer, in milliseconds.
    #[serde(
        default = "default_liveness_probe_timeout_ms",
        skip_serializing_if = "is_default_liveness_probe_timeout_ms"
    )]
    pub liveness_probe_timeout_ms: u64,
    /// Consecutive idle timeouts allowed before disconnecting.
    #[serde(
        default = "default_consecutive_timeouts_before_disconnect",
        skip_serializing_if = "is_default_consecutive_timeouts_before_disconnect"
    )]
    pub consecutive_timeouts_before_disconnect: u64,
    /// Background liveness-probe interval; zero disables keepalive.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub liveness_probe_interval_ms: u64,
    /// Initial reconnect backoff ceiling, in milliseconds.
    /// Must be greater than zero and no greater than the maximum ceiling.
    #[serde(
        default = "default_reconnect_initial_backoff_ms",
        skip_serializing_if = "is_default_reconnect_initial_backoff_ms"
    )]
    pub reconnect_initial_backoff_ms: u64,
    /// Maximum reconnect backoff ceiling, in milliseconds.
    /// Must be greater than or equal to the initial ceiling.
    #[serde(
        default = "default_reconnect_max_backoff_ms",
        skip_serializing_if = "is_default_reconnect_max_backoff_ms"
    )]
    pub reconnect_max_backoff_ms: u64,
    /// Maximum reconnect attempts before pausing; zero retries indefinitely.
    ///
    /// When interval keepalive is enabled, it re-arms an exhausted reconnect
    /// loop. With keepalive disabled, a bounded-attempts server stays down
    /// until it is attached again.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reconnect_max_attempts: u64,
}

impl Default for McpCallPolicy {
    fn default() -> Self {
        Self {
            call_timeout_ms: default_call_timeout_ms(),
            call_max_total_timeout_ms: default_call_max_total_timeout_ms(),
            reset_call_timeout_on_progress: default_true(),
            timeout_disconnect_policy: default_timeout_disconnect_policy(),
            liveness_probe_timeout_ms: default_liveness_probe_timeout_ms(),
            consecutive_timeouts_before_disconnect: default_consecutive_timeouts_before_disconnect(
            ),
            liveness_probe_interval_ms: 0,
            reconnect_initial_backoff_ms: default_reconnect_initial_backoff_ms(),
            reconnect_max_backoff_ms: default_reconnect_max_backoff_ms(),
            reconnect_max_attempts: 0,
        }
    }
}

impl McpCallPolicy {
    pub(crate) fn call_timeout(&self) -> Duration {
        Duration::from_millis(self.call_timeout_ms)
    }

    pub(crate) fn call_max_total_timeout(&self) -> Duration {
        Duration::from_millis(self.call_max_total_timeout_ms)
    }

    pub(crate) fn liveness_probe_timeout(&self) -> Duration {
        Duration::from_millis(self.liveness_probe_timeout_ms)
    }

    pub(crate) fn liveness_probe_interval(&self) -> Duration {
        Duration::from_millis(self.liveness_probe_interval_ms)
    }

    pub(crate) fn reconnect_initial_backoff(&self) -> Duration {
        Duration::from_millis(self.reconnect_initial_backoff_ms)
    }

    pub(crate) fn reconnect_max_backoff(&self) -> Duration {
        Duration::from_millis(self.reconnect_max_backoff_ms)
    }
}

/// Connection configuration for one MCP server. Tag (`transport`) selects
/// the wire transport; per-variant fields configure that transport.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpServerConfig {
    /// Spawn a child process and speak JSON-RPC over stdio.
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        #[serde(
            default = "default_startup_timeout_ms",
            skip_serializing_if = "is_default_startup_timeout_ms"
        )]
        startup_timeout_ms: u64,
        #[serde(flatten)]
        call_policy: McpCallPolicy,
        /// Persist non-image MCP binary content as model attachments.
        #[serde(default, skip_serializing_if = "is_false")]
        binary_content_attachments: bool,
    },
    /// Newer MCP spec HTTP/JSON streaming transport.
    ///
    /// `headers` are static values installed when the transport connects.
    /// Lash does not enable rmcp's `auth` feature and does not perform OAuth,
    /// token acquisition, or token refresh.
    StreamableHttp {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        #[serde(
            default = "default_startup_timeout_ms",
            skip_serializing_if = "is_default_startup_timeout_ms"
        )]
        startup_timeout_ms: u64,
        #[serde(flatten)]
        call_policy: McpCallPolicy,
        /// Persist non-image MCP binary content as model attachments.
        #[serde(default, skip_serializing_if = "is_false")]
        binary_content_attachments: bool,
    },
}

impl McpServerConfig {
    /// Convenience constructor for stdio servers.
    pub fn stdio(command: impl Into<String>, args: Vec<String>) -> Self {
        Self::Stdio {
            command: command.into(),
            args,
            env: BTreeMap::new(),
            cwd: None,
            startup_timeout_ms: default_startup_timeout_ms(),
            call_policy: McpCallPolicy::default(),
            binary_content_attachments: false,
        }
    }

    /// Convenience constructor for streamable-HTTP servers.
    pub fn streamable_http(url: impl Into<String>) -> Self {
        Self::StreamableHttp {
            url: url.into(),
            headers: BTreeMap::new(),
            startup_timeout_ms: default_startup_timeout_ms(),
            call_policy: McpCallPolicy::default(),
            binary_content_attachments: false,
        }
    }

    /// Set static HTTP headers for a streamable-HTTP server.
    ///
    /// These values are reused unchanged on reconnect. This is suitable for
    /// fixed API keys and host-managed tokens, but it does not enable OAuth or
    /// token refresh; rmcp's `auth` feature is not enabled by this crate.
    ///
    /// # Panics
    ///
    /// Panics when called on a stdio configuration.
    pub fn with_headers<K, V>(mut self, headers: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        let Self::StreamableHttp {
            headers: configured,
            ..
        } = &mut self
        else {
            panic!("MCP HTTP headers can only be configured for streamable-HTTP servers");
        };
        *configured = headers
            .into_iter()
            .map(|(name, value)| (name.into(), value.into()))
            .collect();
        self
    }

    /// Set environment variables for a stdio server child process.
    ///
    /// # Panics
    ///
    /// Panics when called on a streamable-HTTP configuration.
    pub fn with_env<K, V>(mut self, env: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        let Self::Stdio {
            env: configured, ..
        } = &mut self
        else {
            panic!("MCP child environment can only be configured for stdio servers");
        };
        *configured = env
            .into_iter()
            .map(|(name, value)| (name.into(), value.into()))
            .collect();
        self
    }

    /// Set startup, idle call, and total call timeouts for either transport.
    pub fn with_timeouts(
        mut self,
        startup_timeout: Duration,
        call_timeout: Duration,
        call_max_total_timeout: Duration,
    ) -> Self {
        let startup_timeout_ms = duration_millis(startup_timeout);
        match &mut self {
            Self::Stdio {
                startup_timeout_ms: configured,
                call_policy,
                ..
            }
            | Self::StreamableHttp {
                startup_timeout_ms: configured,
                call_policy,
                ..
            } => {
                *configured = startup_timeout_ms;
                call_policy.call_timeout_ms = duration_millis(call_timeout);
                call_policy.call_max_total_timeout_ms = duration_millis(call_max_total_timeout);
            }
        }
        self
    }

    pub fn startup_timeout(&self) -> Duration {
        Duration::from_millis(match self {
            Self::Stdio {
                startup_timeout_ms, ..
            }
            | Self::StreamableHttp {
                startup_timeout_ms, ..
            } => *startup_timeout_ms,
        })
    }

    pub fn call_timeout(&self) -> Duration {
        self.call_policy().call_timeout()
    }

    pub(crate) fn call_max_total_timeout(&self) -> Duration {
        self.call_policy().call_max_total_timeout()
    }

    pub(crate) fn reset_call_timeout_on_progress(&self) -> bool {
        self.call_policy().reset_call_timeout_on_progress
    }

    pub(crate) fn timeout_disconnect_policy(&self) -> TimeoutDisconnectPolicy {
        self.call_policy().timeout_disconnect_policy
    }

    pub(crate) fn liveness_probe_timeout(&self) -> Duration {
        self.call_policy().liveness_probe_timeout()
    }

    pub(crate) fn consecutive_timeouts_before_disconnect(&self) -> u64 {
        self.call_policy().consecutive_timeouts_before_disconnect
    }

    pub(crate) fn liveness_probe_interval(&self) -> Duration {
        self.call_policy().liveness_probe_interval()
    }

    pub(crate) fn reconnect_initial_backoff(&self) -> Duration {
        self.call_policy().reconnect_initial_backoff()
    }

    pub(crate) fn reconnect_max_backoff(&self) -> Duration {
        self.call_policy().reconnect_max_backoff()
    }

    pub(crate) fn reconnect_max_attempts(&self) -> u64 {
        self.call_policy().reconnect_max_attempts
    }

    /// Return the timeout, liveness, and reconnect policy for this server.
    pub fn call_policy(&self) -> &McpCallPolicy {
        match self {
            Self::Stdio { call_policy, .. } | Self::StreamableHttp { call_policy, .. } => {
                call_policy
            }
        }
    }

    pub fn with_binary_content_attachments(mut self, enabled: bool) -> Self {
        match &mut self {
            Self::Stdio {
                binary_content_attachments,
                ..
            }
            | Self::StreamableHttp {
                binary_content_attachments,
                ..
            } => *binary_content_attachments = enabled,
        }
        self
    }

    pub(crate) fn binary_content_attachments(&self) -> bool {
        match self {
            Self::Stdio {
                binary_content_attachments,
                ..
            }
            | Self::StreamableHttp {
                binary_content_attachments,
                ..
            } => *binary_content_attachments,
        }
    }

    pub(crate) fn validate(&self, server_name: &str) -> Result<(), McpError> {
        if server_name.trim().is_empty() {
            return Err(McpError::Config(
                "MCP server name cannot be empty".to_string(),
            ));
        }
        if server_name.contains("__") {
            return Err(McpError::Config(format!(
                "MCP server `{server_name}` cannot contain `__`"
            )));
        }
        let policy = self.call_policy();
        if policy.reconnect_initial_backoff_ms == 0 {
            return Err(McpError::Config(format!(
                "MCP server `{server_name}` reconnect_initial_backoff_ms must be greater than zero"
            )));
        }
        if policy.reconnect_max_backoff_ms < policy.reconnect_initial_backoff_ms {
            return Err(McpError::Config(format!(
                "MCP server `{server_name}` reconnect_max_backoff_ms must be greater than or equal to reconnect_initial_backoff_ms"
            )));
        }
        if policy.call_max_total_timeout_ms <= policy.call_timeout_ms {
            return Err(McpError::Config(format!(
                "MCP server `{server_name}` call_max_total_timeout_ms must be greater than call_timeout_ms"
            )));
        }
        match self {
            Self::Stdio { command, .. } if command.trim().is_empty() => Err(McpError::Config(
                format!("MCP server `{server_name}` command cannot be empty"),
            )),
            Self::StreamableHttp { url, .. } if url.trim().is_empty() => Err(McpError::Config(
                format!("MCP server `{server_name}` URL cannot be empty"),
            )),
            _ => Ok(()),
        }
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis())
        .expect("MCP timeout exceeds the supported millisecond range")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_attachment_opt_in_is_explicit_and_defaults_off() {
        let default = McpServerConfig::stdio("mcp-server", Vec::new());
        assert!(!default.binary_content_attachments());
        let json = serde_json::to_value(&default).unwrap();
        assert!(json.get("binary_content_attachments").is_none());

        let enabled = default.with_binary_content_attachments(true);
        assert!(enabled.binary_content_attachments());
        assert_eq!(
            serde_json::to_value(enabled).unwrap()["binary_content_attachments"],
            true
        );
    }

    #[test]
    fn transport_builders_cover_headers_env_and_timeouts() {
        let http = McpServerConfig::streamable_http("https://mcp.example.test")
            .with_headers([("Authorization", "Bearer static")])
            .with_timeouts(
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(3),
            );
        let McpServerConfig::StreamableHttp {
            headers,
            startup_timeout_ms,
            call_policy,
            ..
        } = http
        else {
            panic!("streamable constructor changed transport")
        };
        assert_eq!(headers["Authorization"], "Bearer static");
        assert_eq!(startup_timeout_ms, 1_000);
        assert_eq!(call_policy.call_timeout_ms, 2_000);
        assert_eq!(call_policy.call_max_total_timeout_ms, 3_000);

        let stdio = McpServerConfig::stdio("server", Vec::new()).with_env([("TOKEN", "static")]);
        let McpServerConfig::Stdio { env, .. } = stdio else {
            panic!("stdio constructor changed transport")
        };
        assert_eq!(env["TOKEN"], "static");
    }

    #[test]
    fn liveness_and_reconnect_policy_defaults_round_trip_for_both_transports() {
        for json in [
            serde_json::json!({"transport": "stdio", "command": "srv"}),
            serde_json::json!({"transport": "streamable_http", "url": "http://localhost/mcp"}),
        ] {
            let config: McpServerConfig = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(config.call_timeout(), Duration::from_millis(60_000));
            assert_eq!(
                config.call_max_total_timeout(),
                Duration::from_millis(600_000)
            );
            assert!(config.reset_call_timeout_on_progress());
            assert_eq!(
                config.timeout_disconnect_policy(),
                TimeoutDisconnectPolicy::PingProbe
            );
            assert_eq!(
                config.liveness_probe_timeout(),
                Duration::from_millis(5_000)
            );
            assert_eq!(config.consecutive_timeouts_before_disconnect(), 3);
            assert!(config.liveness_probe_interval().is_zero());
            assert_eq!(
                config.reconnect_initial_backoff(),
                Duration::from_millis(500)
            );
            assert_eq!(
                config.reconnect_max_backoff(),
                Duration::from_millis(30_000)
            );
            assert_eq!(config.reconnect_max_attempts(), 0);
            assert_eq!(serde_json::to_value(config).unwrap(), json);
        }

        let custom: McpServerConfig = serde_json::from_value(serde_json::json!({
            "transport": "stdio",
            "command": "srv",
            "call_max_total_timeout_ms": 42,
            "reset_call_timeout_on_progress": false,
            "timeout_disconnect_policy": "consecutive_timeouts",
            "liveness_probe_timeout_ms": 43,
            "consecutive_timeouts_before_disconnect": 4,
            "liveness_probe_interval_ms": 44,
            "reconnect_initial_backoff_ms": 45,
            "reconnect_max_backoff_ms": 46,
            "reconnect_max_attempts": 5
        }))
        .unwrap();
        let encoded = serde_json::to_value(custom).unwrap();
        assert_eq!(encoded["call_max_total_timeout_ms"], 42);
        assert_eq!(encoded["reset_call_timeout_on_progress"], false);
        assert_eq!(encoded["timeout_disconnect_policy"], "consecutive_timeouts");
        assert_eq!(encoded["liveness_probe_interval_ms"], 44);
        assert_eq!(encoded["reconnect_max_attempts"], 5);
    }

    /// The accepted `transport` tags are exactly the two that can connect.
    /// A config naming any other transport — notably the legacy `sse` one this
    /// crate never supported — is rejected at deserialization, so an operator
    /// learns about it from their config rather than from a connect failure.
    #[test]
    fn only_stdio_and_streamable_http_transports_deserialize() {
        let stdio: McpServerConfig =
            serde_json::from_value(serde_json::json!({"transport": "stdio", "command": "srv"}))
                .expect("stdio config must deserialize");
        assert!(matches!(stdio, McpServerConfig::Stdio { .. }));

        let http: McpServerConfig = serde_json::from_value(
            serde_json::json!({"transport": "streamable_http", "url": "http://localhost:1/mcp"}),
        )
        .expect("streamable_http config must deserialize");
        assert!(matches!(http, McpServerConfig::StreamableHttp { .. }));

        for transport in ["sse", "http", "websocket"] {
            let err = serde_json::from_value::<McpServerConfig>(
                serde_json::json!({"transport": transport, "url": "http://localhost:1/mcp"}),
            )
            .expect_err("unsupported transport must be rejected");
            let msg = err.to_string();
            assert!(
                msg.contains("stdio") && msg.contains("streamable_http"),
                "rejection should name the supported transports: {msg}"
            );
        }
    }

    #[test]
    fn validation_rejects_zero_initial_reconnect_backoff() {
        let config: McpServerConfig = serde_json::from_value(serde_json::json!({
            "transport": "stdio",
            "command": "srv",
            "reconnect_initial_backoff_ms": 0
        }))
        .unwrap();
        let error = config.validate("srv").expect_err("zero backoff rejected");
        assert!(
            error
                .to_string()
                .contains("reconnect_initial_backoff_ms must be greater than zero")
        );
    }

    #[test]
    fn validation_rejects_zero_or_sub_initial_max_reconnect_backoff() {
        for max_ms in [0, 9] {
            let config: McpServerConfig = serde_json::from_value(serde_json::json!({
                "transport": "stdio",
                "command": "srv",
                "reconnect_initial_backoff_ms": 10,
                "reconnect_max_backoff_ms": max_ms
            }))
            .unwrap();
            let error = config
                .validate("srv")
                .expect_err("maximum below the non-zero initial delay must be rejected");
            assert!(
                error.to_string().contains(
                    "reconnect_max_backoff_ms must be greater than or equal to reconnect_initial_backoff_ms"
                ),
                "unexpected validation error: {error}"
            );
        }

        let boundary: McpServerConfig = serde_json::from_value(serde_json::json!({
            "transport": "stdio",
            "command": "srv",
            "reconnect_initial_backoff_ms": 1,
            "reconnect_max_backoff_ms": 1
        }))
        .unwrap();
        boundary.validate("srv").expect("one millisecond is valid");
    }

    #[test]
    fn validation_requires_wall_cap_to_exceed_idle_timeout() {
        for wall_cap_ms in [49, 50] {
            let config: McpServerConfig = serde_json::from_value(serde_json::json!({
                "transport": "stdio",
                "command": "srv",
                "call_timeout_ms": 50,
                "call_max_total_timeout_ms": wall_cap_ms
            }))
            .unwrap();
            let error = config
                .validate("srv")
                .expect_err("ambiguous wall cap rejected");
            assert!(
                error
                    .to_string()
                    .contains("call_max_total_timeout_ms must be greater than call_timeout_ms")
            );
        }
    }
}
