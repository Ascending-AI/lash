use serde::Serialize;

const DEFAULT_CONTROL_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_ATTACH_CEILING_MS: u64 = 6 * 60 * 60 * 1_000;

const fn default_control_timeout_ms() -> u64 {
    DEFAULT_CONTROL_TIMEOUT_MS
}

const fn default_attach_ceiling_ms() -> u64 {
    DEFAULT_ATTACH_CEILING_MS
}

/// Deadline classes for HTTP operations issued through a [`super::RestateConnection`].
///
/// Control operations should fail quickly so callers can retry or report a
/// degraded substrate. Attach operations can legitimately remain parked for a
/// durable workflow's lifetime, so they receive a separate generous ceiling.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestateConnectionConfig {
    /// Submit, cancel, status, query, and patch deadline. Default 30 seconds.
    #[serde(
        default = "default_control_timeout_ms",
        deserialize_with = "deserialize_control_timeout_ms"
    )]
    pub control_timeout_ms: u64,
    /// Await/attach request ceiling. Default 6 hours.
    #[serde(
        default = "default_attach_ceiling_ms",
        deserialize_with = "deserialize_attach_ceiling_ms"
    )]
    pub attach_ceiling_ms: u64,
    /// Raw response byte budget for success and error bodies on both deadline
    /// classes. Default 16 MiB; zero permits only an empty body.
    #[serde(default = "default_response_body_bytes")]
    pub response_body_bytes: usize,
}

const fn default_response_body_bytes() -> usize {
    16 * 1024 * 1024
}

fn deserialize_control_timeout_ms<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <u64 as serde::Deserialize>::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom(
            "control_timeout_ms must be greater than zero",
        ));
    }
    Ok(value)
}

fn deserialize_attach_ceiling_ms<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <u64 as serde::Deserialize>::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom(
            "attach_ceiling_ms must be greater than zero",
        ));
    }
    Ok(value)
}

impl Default for RestateConnectionConfig {
    fn default() -> Self {
        Self {
            control_timeout_ms: default_control_timeout_ms(),
            attach_ceiling_ms: default_attach_ceiling_ms(),
            response_body_bytes: default_response_body_bytes(),
        }
    }
}
