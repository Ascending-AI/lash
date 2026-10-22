//! The session's protocol turn options: a view of the protocol plugin's
//! recorded configuration namespace (FIG-4379).
//!
//! The protocol's namespace is recorded in the session's
//! [`PluginConfig`](crate::PluginConfig) like every other owner's; this type
//! is how protocol code reads it, derived wherever it is consumed and never
//! stored beside it. It serializes as the bare value. What a run states for
//! the protocol is that owner's typed run options, carried like any owner's
//! ([`OwnerRunOptions`](crate::OwnerRunOptions), FIG-6021).

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct ProtocolTurnOptions {
    pub payload: crate::plugin_state::ConfigJson,
}

impl Default for ProtocolTurnOptions {
    fn default() -> Self {
        Self::empty()
    }
}
impl ProtocolTurnOptions {
    pub fn empty() -> Self {
        Self::from_payload(serde_json::Value::Object(serde_json::Map::new()))
    }

    pub fn from_payload(payload: impl Into<crate::plugin_state::ConfigJson>) -> Self {
        Self {
            payload: payload.into(),
        }
    }

    /// Reports empty only for an empty JSON object so protocol implementors do not confuse scalar,
    /// list, or null payloads with absent options.
    pub fn is_empty(&self) -> bool {
        match self.payload.as_ref() {
            serde_json::Value::Object(map) => map.is_empty(),
            _ => false,
        }
    }

    /// Serializes typed protocol options for protocol implementors resolving
    /// their namespace.
    pub fn typed<T>(value: T) -> Result<Self, serde_json::Error>
    where
        T: serde::Serialize,
    {
        Ok(Self::from_payload(serde_json::to_value(value)?))
    }

    pub fn decode<T>(&self) -> Result<T, ProtocolTurnOptionsError>
    where
        T: serde::de::DeserializeOwned,
    {
        serde::Deserialize::deserialize(self.payload.as_ref())
            .map_err(ProtocolTurnOptionsError::Decode)
    }
}
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProtocolTurnOptionsError {
    #[error("failed to decode protocol turn options payload: {0}")]
    Decode(#[source] serde_json::Error),
}
