use lash_core::plugin::PluginSessionMaterialization;
use lash_core::{PluginError, ProtocolTurnOptions};

/// Session-pinned transport for RLM programs; both channels use the same engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RlmChannel {
    /// Programs appear in paired dialect cells.
    Cell,
    /// Programs appear in the provider's execute_code tool call.
    NativeTool,
}
impl std::str::FromStr for RlmChannel {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "cell" => Ok(Self::Cell),
            "native" | "native_tool" => Ok(Self::NativeTool),
            _ => Err(format!("RLM channel must be cell or native, got `{value}`")),
        }
    }
}

/// The recorded key of the session's dialect: the language id of the dialect
/// the host selected when the session materialized (ADR 0096).
const DIALECT_FIELD: &str = "dialect";

/// The bag without the session's transport and dialect pins: what remains is
/// the RLM create extras.
pub(super) fn without_session_pins(options: &ProtocolTurnOptions) -> ProtocolTurnOptions {
    let mut options = options.clone();
    if let Some(object) = options.payload.as_object_mut() {
        object.remove("channel");
        object.remove(DIALECT_FIELD);
    }
    options
}
#[expect(
    clippy::expect_used,
    reason = "RlmChannel is a crate-owned enum of strings, so serde_json encoding cannot fail"
)]
pub(super) fn record_channel(
    mut options: ProtocolTurnOptions,
    channel: RlmChannel,
) -> ProtocolTurnOptions {
    options.payload["channel"] = serde_json::to_value(channel).expect("channel serializes");
    options
}
#[expect(
    clippy::expect_used,
    reason = "RlmChannel is a crate-owned enum of strings, both sides decoded from the same recorded payload, so serialization cannot fail"
)]
pub(super) fn validate_channel(
    options: &ProtocolTurnOptions,
    requested: RlmChannel,
    materialization: PluginSessionMaterialization,
) -> Result<(), PluginError> {
    match options.payload.get("channel") {
        Some(value) => {
            let recorded: RlmChannel = serde_json::from_value(value.clone()).map_err(|error| {
                PluginError::Session(format!("invalid recorded RLM channel: {error}"))
            })?;
            if recorded != requested {
                return Err(PluginError::RecordedSessionConfigConflict {
                    plugin_id: super::RLM_PROTOCOL_PLUGIN_ID.to_string(),
                    field: "channel".to_string(),
                    recorded: serde_json::to_string(&recorded).expect("channel serializes"),
                    requested: serde_json::to_string(&requested).expect("channel serializes"),
                });
            }
            Ok(())
        }
        None if matches!(
            materialization,
            PluginSessionMaterialization::Rematerialization
        ) =>
        {
            Err(PluginError::MissingRecordedSessionConfig {
                plugin_id: super::RLM_PROTOCOL_PLUGIN_ID.to_string(),
                field: "channel".to_string(),
            })
        }
        None => Ok(()),
    }
}

/// Records the host's selected dialect on a materializing session. A session
/// keeps the id it first recorded; the factory refuses a later build under
/// another dialect ([`validate_dialect`]).
pub(super) fn record_dialect(
    mut options: ProtocolTurnOptions,
    language_id: &'static str,
) -> ProtocolTurnOptions {
    options.payload[DIALECT_FIELD] = serde_json::Value::String(language_id.to_string());
    options
}

/// The session's recorded dialect against the host's selection: a different
/// id is a typed conflict, and a rematerialized session that recorded none is
/// refused rather than read as whatever the host selects today.
pub(super) fn validate_dialect(
    options: &ProtocolTurnOptions,
    selected: &'static str,
    materialization: PluginSessionMaterialization,
) -> Result<(), PluginError> {
    match options.payload.get(DIALECT_FIELD) {
        Some(serde_json::Value::String(recorded)) if recorded == selected => Ok(()),
        Some(serde_json::Value::String(recorded)) => {
            Err(PluginError::RecordedSessionConfigConflict {
                plugin_id: super::RLM_PROTOCOL_PLUGIN_ID.to_string(),
                field: DIALECT_FIELD.to_string(),
                recorded: recorded.clone(),
                requested: selected.to_string(),
            })
        }
        Some(other) => Err(PluginError::Session(format!(
            "invalid recorded RLM dialect: {other}"
        ))),
        None if matches!(
            materialization,
            PluginSessionMaterialization::Rematerialization
        ) =>
        {
            Err(PluginError::MissingRecordedSessionConfig {
                plugin_id: super::RLM_PROTOCOL_PLUGIN_ID.to_string(),
                field: DIALECT_FIELD.to_string(),
            })
        }
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_accepts_all_channel_spellings() {
        assert_eq!("cell".parse(), Ok(RlmChannel::Cell));
        assert_eq!("native".parse(), Ok(RlmChannel::NativeTool));
        assert_eq!("native_tool".parse(), Ok(RlmChannel::NativeTool));
    }

    #[test]
    fn recorded_channel_refuses_substitution_and_missing_pin() {
        for channel in [RlmChannel::Cell, RlmChannel::NativeTool] {
            let options = record_channel(ProtocolTurnOptions::default(), channel);
            let options: ProtocolTurnOptions =
                serde_json::from_str(&serde_json::to_string(&options).unwrap()).unwrap();
            validate_channel(
                &options,
                channel,
                PluginSessionMaterialization::Rematerialization,
            )
            .unwrap();
            let other = if channel == RlmChannel::Cell {
                RlmChannel::NativeTool
            } else {
                RlmChannel::Cell
            };
            assert!(
                matches!(validate_channel(&options, other, PluginSessionMaterialization::Rematerialization), Err(PluginError::RecordedSessionConfigConflict { field, .. }) if field == "channel")
            );
        }
        assert!(
            matches!(validate_channel(&ProtocolTurnOptions::default(), RlmChannel::Cell, PluginSessionMaterialization::Rematerialization), Err(PluginError::MissingRecordedSessionConfig { field, .. }) if field == "channel")
        );
    }

    #[test]
    fn recorded_dialect_refuses_substitution_and_missing_pin() {
        let options = record_dialect(ProtocolTurnOptions::default(), "typescript");
        let options: ProtocolTurnOptions =
            serde_json::from_str(&serde_json::to_string(&options).unwrap()).unwrap();
        validate_dialect(
            &options,
            "typescript",
            PluginSessionMaterialization::Rematerialization,
        )
        .unwrap();
        assert!(matches!(
            validate_dialect(
                &options,
                "other-dialect",
                PluginSessionMaterialization::Rematerialization
            ),
            Err(PluginError::RecordedSessionConfigConflict { field, recorded, requested, .. })
                if field == "dialect" && recorded == "typescript" && requested == "other-dialect"
        ));
        assert!(matches!(
            validate_dialect(
                &ProtocolTurnOptions::default(),
                "typescript",
                PluginSessionMaterialization::Rematerialization
            ),
            Err(PluginError::MissingRecordedSessionConfig { field, .. }) if field == "dialect"
        ));
        validate_dialect(
            &ProtocolTurnOptions::default(),
            "typescript",
            PluginSessionMaterialization::Creation,
        )
        .unwrap();
        assert!(
            without_session_pins(&record_channel(options, RlmChannel::Cell))
                .payload
                .as_object()
                .is_none_or(serde_json::Map::is_empty)
        );
    }
}
