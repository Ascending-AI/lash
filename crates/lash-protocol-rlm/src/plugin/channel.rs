use lash_core::PluginError;
use lash_core::plugin::PluginSessionMaterialization;

use super::RlmRecordedConfig;

/// Session-pinned transport for RLM programs; both channels use the same engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RlmChannel {
    /// Programs appear in paired dialect cells.
    Cell,
    /// Programs appear in the provider's execute_code tool call.
    NativeTool,
}
impl RlmChannel {
    /// The channel's recorded spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cell => "cell",
            Self::NativeTool => "native_tool",
        }
    }
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

/// The session's recorded channel against the host's selection: a different
/// channel is a typed conflict, and a rematerialized session that recorded
/// none is refused rather than read as whatever the host selects today.
#[expect(
    clippy::expect_used,
    reason = "RlmChannel is a crate-owned enum of strings, so serialization cannot fail"
)]
pub(super) fn validate_channel(
    recorded: Option<&RlmRecordedConfig>,
    requested: RlmChannel,
    materialization: PluginSessionMaterialization,
) -> Result<(), PluginError> {
    match recorded.and_then(|recorded| recorded.channel) {
        Some(recorded) if recorded == requested => Ok(()),
        Some(recorded) => Err(PluginError::RecordedSessionConfigConflict {
            plugin_id: super::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            field: "channel".to_string(),
            recorded: serde_json::to_string(&recorded).expect("channel serializes"),
            requested: serde_json::to_string(&requested).expect("channel serializes"),
        }),
        None => missing_pin("channel", materialization),
    }
}

/// The session's recorded dialect against the host's selection: the language
/// id of the dialect the host selected when the session materialized
/// (ADR 0096). A different id is a typed conflict, and a rematerialized
/// session that recorded none is refused.
pub(super) fn validate_dialect(
    recorded: Option<&RlmRecordedConfig>,
    selected: &'static str,
    materialization: PluginSessionMaterialization,
) -> Result<(), PluginError> {
    match recorded.and_then(|recorded| recorded.dialect.as_deref()) {
        Some(recorded) if recorded == selected => Ok(()),
        Some(recorded) => Err(PluginError::RecordedSessionConfigConflict {
            plugin_id: super::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            field: "dialect".to_string(),
            recorded: recorded.to_string(),
            requested: selected.to_string(),
        }),
        None => missing_pin("dialect", materialization),
    }
}

/// A session being created has recorded no pin yet; a rebuilt one must have.
fn missing_pin(
    field: &str,
    materialization: PluginSessionMaterialization,
) -> Result<(), PluginError> {
    match materialization {
        PluginSessionMaterialization::Rematerialization => {
            Err(PluginError::MissingRecordedSessionConfig {
                plugin_id: super::RLM_PROTOCOL_PLUGIN_ID.to_string(),
                field: field.to_string(),
            })
        }
        PluginSessionMaterialization::Creation => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recorded namespace with only its pins.
    fn pinned(channel: Option<RlmChannel>, dialect: Option<&str>) -> RlmRecordedConfig {
        RlmRecordedConfig {
            render: None,
            termination: None,
            final_answer_format: None,
            channel,
            dialect: dialect.map(str::to_string),
            behaviour: super::super::RlmProtocolPluginConfig::builder()
                .channel(RlmChannel::Cell)
                .instruction_limit(super::super::InstructionBound::unbounded())
                .memory_limit(super::super::MemoryBound::unbounded())
                .build()
                .recorded_behaviour(false),
        }
    }

    const REBUILT: PluginSessionMaterialization = PluginSessionMaterialization::Rematerialization;

    #[test]
    fn recorded_channel_refuses_substitution_and_missing_pin() {
        for channel in [RlmChannel::Cell, RlmChannel::NativeTool] {
            let recorded = pinned(Some(channel), None);
            validate_channel(Some(&recorded), channel, REBUILT).unwrap();
            let other = if channel == RlmChannel::Cell {
                RlmChannel::NativeTool
            } else {
                RlmChannel::Cell
            };
            assert!(matches!(
                validate_channel(Some(&recorded), other, REBUILT),
                Err(PluginError::RecordedSessionConfigConflict { field, .. }) if field == "channel"
            ));
        }
        for unpinned in [None, Some(&pinned(None, None))] {
            assert!(matches!(
                validate_channel(unpinned, RlmChannel::Cell, REBUILT),
                Err(PluginError::MissingRecordedSessionConfig { field, .. }) if field == "channel"
            ));
        }
    }

    #[test]
    fn recorded_dialect_refuses_substitution_and_missing_pin() {
        let recorded = pinned(None, Some("typescript"));
        validate_dialect(Some(&recorded), "typescript", REBUILT).unwrap();
        assert!(matches!(
            validate_dialect(Some(&recorded), "other-dialect", REBUILT),
            Err(PluginError::RecordedSessionConfigConflict { field, recorded, requested, .. })
                if field == "dialect" && recorded == "typescript" && requested == "other-dialect"
        ));
        assert!(matches!(
            validate_dialect(None, "typescript", REBUILT),
            Err(PluginError::MissingRecordedSessionConfig { field, .. }) if field == "dialect"
        ));
        validate_dialect(None, "typescript", PluginSessionMaterialization::Creation).unwrap();
    }
}
