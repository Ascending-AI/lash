//! Durable protocol turn options carried on the session head.

use serde::Deserialize;

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
impl facade_ops::ProtocolTurnOptionsFacadeOps for ProtocolTurnOptions {
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

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProtocolTurnOptionsError {
    #[error(
        "protocol turn options are missing schema_version and were written by unsupported pre-versioned state (expected {expected})"
    )]
    MissingSchemaVersion { expected: u32 },
    #[error(
        "protocol turn options schema_version {actual} is not supported by this binary (expected {expected})"
    )]
    UnsupportedSchemaVersion { actual: u32, expected: u32 },
    #[error(
        "protocol turn options schema_version {actual} is invalid (expected integer {expected})"
    )]
    InvalidSchemaVersion { actual: String, expected: u32 },
    #[error("failed to decode protocol turn options payload: {0}")]
    Decode(#[source] serde_json::Error),
}

/// Emits the persisted wire shape for [`ProtocolTurnOptions`]: the schema version is stamped from
/// the constant rather than carried in memory, so this body is the sole definition of the emitted
/// field names, their order, and the stamped version's type. It is a named free function so the
/// version-bump guard can cover it by symbol.
fn serialize_protocol_turn_options<S>(
    options: &ProtocolTurnOptions,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeStruct;
    let mut state = serializer.serialize_struct("ProtocolTurnOptions", 2)?;
    state.serialize_field("schema_version", &PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION)?;
    state.serialize_field("payload", &options.payload)?;
    state.end()
}
fn empty_protocol_turn_payload() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}
fn parse_protocol_turn_options_schema_version(
    value: Option<serde_json::Value>,
) -> Result<u32, ProtocolTurnOptionsError> {
    let expected = PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION;
    let Some(value) = value else {
        return Err(ProtocolTurnOptionsError::MissingSchemaVersion { expected });
    };
    let Some(actual) = value
        .as_u64()
        .and_then(|version| u32::try_from(version).ok())
    else {
        return Err(ProtocolTurnOptionsError::InvalidSchemaVersion {
            actual: value.to_string(),
            expected,
        });
    };
    ensure_protocol_turn_options_schema_version(actual)?;
    Ok(actual)
}

pub(crate) const PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION: u32 = 1;
fn ensure_protocol_turn_options_schema_version(
    actual: u32,
) -> Result<(), ProtocolTurnOptionsError> {
    let expected = PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION;
    if actual == expected {
        Ok(())
    } else {
        Err(ProtocolTurnOptionsError::UnsupportedSchemaVersion { actual, expected })
    }
}

pub mod facade_ops {

    /// Facade-internal operations for [`ProtocolTurnOptions`].
    ///
    /// This is not integrator surface, carries no stability promise, and exists
    /// only for the `lash` facade. See [ADR 0051](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0051-the-facade-is-the-host-api-core-is-integrator-seams.md).
    pub trait ProtocolTurnOptionsFacadeOps {
        fn merged_with_override(&self, override_options: &Self) -> Self;
    }
}

#[cfg(test)]
mod schema_version_tests {
    use super::*;

    #[test]
    fn protocol_turn_options_missing_payload_deserializes_to_empty_object() {
        let options: ProtocolTurnOptions = serde_json::from_value(serde_json::json!({
            "schema_version": PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION
        }))
        .expect("deserialize options");

        assert!(options.is_empty());
        assert_eq!(options.payload, serde_json::json!({}));
    }

    #[test]
    fn protocol_turn_options_explicit_null_is_not_empty() {
        let options: ProtocolTurnOptions = serde_json::from_value(serde_json::json!({
            "schema_version": PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION,
            "payload": null
        }))
        .expect("deserialize options");

        assert!(!options.is_empty());
        assert_eq!(options.payload, serde_json::Value::Null);
    }

    #[test]
    fn protocol_turn_options_missing_schema_version_rejects_preversioned_state() {
        let err =
            serde_json::from_value::<ProtocolTurnOptions>(serde_json::json!({ "payload": {} }))
                .expect_err("pre-versioned options should fail");

        assert!(
            err.to_string().contains(
                "missing schema_version and were written by unsupported pre-versioned state"
            ),
            "{err}"
        );
    }

    #[test]
    fn protocol_turn_options_unsupported_schema_version_rejects_state() {
        let err = serde_json::from_value::<ProtocolTurnOptions>(serde_json::json!({
            "schema_version": PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION + 1,
            "payload": {}
        }))
        .expect_err("unsupported options version should fail");

        assert!(
            err.to_string().contains("is not supported by this binary"),
            "{err}"
        );
    }

    #[test]
    fn protocol_turn_options_serialization_preserves_wire_shape() {
        let options = ProtocolTurnOptions::from_payload(serde_json::json!({
            "mode": "test"
        }));
        // Byte-level: `serde_json::Value` compares as a `BTreeMap` here, so only the emitted
        // string pins field order — the property the persisted envelope actually depends on.
        let encoded = serde_json::to_string(&options).expect("serialize options");
        assert_eq!(encoded, r#"{"schema_version":1,"payload":{"mode":"test"}}"#);
        assert_eq!(PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION, 1);

        let round_tripped: ProtocolTurnOptions =
            serde_json::from_str(&encoded).expect("deserialize roundtrip");
        assert_eq!(round_tripped.payload, serde_json::json!({ "mode": "test" }));
    }
}
