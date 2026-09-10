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

pub(super) fn without_channel(options: &ProtocolTurnOptions) -> ProtocolTurnOptions {
    let mut options = options.clone();
    if let Some(object) = options.payload.as_object_mut() {
        object.remove("channel");
    }
    options
}
pub(super) fn record_channel(
    mut options: ProtocolTurnOptions,
    channel: RlmChannel,
) -> ProtocolTurnOptions {
    options.payload["channel"] = serde_json::to_value(channel).expect("channel serializes");
    options
}
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
}
