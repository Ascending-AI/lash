//! Durable protocol turn options carried on the session head.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolTurnOptions {
    pub payload: serde_json::Value,
}
impl serde::Serialize for ProtocolTurnOptions {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serialize_protocol_turn_options(self, serializer)
    }
}
impl<'de> serde::Deserialize<'de> for ProtocolTurnOptions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct ProtocolTurnOptionsWire {
            schema_version: Option<serde_json::Value>,
            #[serde(default = "empty_protocol_turn_payload")]
            payload: serde_json::Value,
        }

        let wire = ProtocolTurnOptionsWire::deserialize(deserializer)?;
        parse_protocol_turn_options_schema_version(wire.schema_version)
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            payload: wire.payload,
        })
    }
}
impl Default for ProtocolTurnOptions {
    fn default() -> Self {
        Self::empty()
    }
}
impl ProtocolTurnOptions {
    /// Constructs schema-current empty object options for protocol implementors materializing a
    /// turn with no protocol-specific overrides.
    pub fn empty() -> Self {
        Self {
            payload: serde_json::Value::Object(serde_json::Map::new()),
        }
    }

    /// Wraps an arbitrary JSON payload at the current schema version for protocol implementors
    /// materializing turn-specific state.
    pub fn from_payload(payload: serde_json::Value) -> Self {
        Self { payload }
    }

    /// Reports empty only for an empty JSON object so protocol implementors do not confuse scalar,
    /// list, or null payloads with absent options.
    pub fn is_empty(&self) -> bool {
        match &self.payload {
            serde_json::Value::Object(map) => map.is_empty(),
            _ => false,
        }
    }

    /// Serializes typed protocol options at the current schema version for protocol implementors
    /// materializing a turn.
    pub fn typed<T>(value: T) -> Result<Self, serde_json::Error>
    where
        T: serde::Serialize,
    {
        Ok(Self {
            payload: serde_json::to_value(value)?,
        })
    }
}
impl ProtocolTurnOptions {
    /// Deserializes typed protocol options payload for protocol implementors.
    pub fn decode<T>(&self) -> Result<T, ProtocolTurnOptionsError>
    where
        T: serde::de::DeserializeOwned,
    {
        serde_json::from_value(self.payload.clone()).map_err(ProtocolTurnOptionsError::Decode)
    }
}
    impl ProtocolTurnOptionsFacadeOps for ProtocolTurnOptions {
        fn merged_with_override(&self, override_options: &Self) -> Self {
            match (&self.payload, &override_options.payload) {
                (serde_json::Value::Object(base), serde_json::Value::Object(overrides)) => {
                    let mut payload = base.clone();
                    payload.extend(overrides.clone());
                    Self {
                        payload: serde_json::Value::Object(payload),
                    }
                }
                _ => override_options.clone(),
            }
        }
    }
