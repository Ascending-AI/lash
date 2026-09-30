//! The identity projections of the records a request identity or an intent
//! hash covers (ADR 0115, FIG-4262).
//!
//! A request identity answers "is this the same request retried?", and a
//! retry can cross a rolling upgrade: first attempted by N, retried by N+1
//! after `lashctl finalize` moved `F`. A format-version stamp is write
//! metadata the fleet selects, not request content, so no identity preimage
//! carries one. Each projection here lists its record's fields explicitly —
//! a new field fails compilation until its place is decided — and replaces
//! every stamped value it holds with the value's content alone:
//!
//! - [`crate::ProtocolTurnOptions`] projects to its payload, never its
//!   `schema_version`;
//! - [`PersistedSessionConfig`](crate::PersistedSessionConfig),
//!   [`PersistedTurnState`](crate::PersistedTurnState) and
//!   [`SessionNodePayload`](crate::SessionNodePayload) project the options
//!   they carry that way, and every other field as it serializes.
//!
//! The serialized shapes otherwise match their records' own serde shapes, so
//! the projection moves no identity bit except the stamps it drops.

use crate::session_graph::SharedJsonValue;

/// Protocol turn options as identity content: the payload, without the
/// stamp the fleet selected for it.
#[derive(serde::Serialize)]
#[serde(transparent)]
pub(super) struct TurnOptionsIntent<'a>(&'a serde_json::Value);

impl<'a> From<&'a crate::ProtocolTurnOptions> for TurnOptionsIntent<'a> {
    fn from(options: &'a crate::ProtocolTurnOptions) -> Self {
        Self(&options.payload)
    }
}

/// [`crate::PersistedSessionConfig`] as identity content.
#[derive(serde::Serialize)]
pub(super) struct ConfigIntent<'a> {
    provider_id: &'a str,
    model: &'a crate::ModelSpec,
    turn_budget: &'a crate::TurnBudget,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<&'a crate::PromptLayer>,
    generation: &'a crate::GenerationOptions,
    tool_access: &'a crate::SessionToolAccess,
    subagent: Option<&'a crate::SubagentSessionContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol_turn_options: Option<TurnOptionsIntent<'a>>,
    config_revision: u64,
}

impl<'a> From<&'a crate::PersistedSessionConfig> for ConfigIntent<'a> {
    fn from(config: &'a crate::PersistedSessionConfig) -> Self {
        let crate::PersistedSessionConfig {
            provider_id,
            model,
            turn_budget,
            prompt,
            generation,
            tool_access,
            subagent,
            protocol_turn_options,
            config_revision,
        } = config;
        Self {
            provider_id,
            model,
            turn_budget,
            prompt: prompt.as_ref(),
            generation,
            tool_access,
            subagent: subagent.as_ref(),
            protocol_turn_options: protocol_turn_options.as_ref().map(TurnOptionsIntent::from),
            config_revision: *config_revision,
        }
    }
}

/// [`crate::PersistedTurnState`] as identity content.
#[derive(serde::Serialize)]
pub(super) struct TurnStateIntent<'a> {
    turn_index: usize,
    token_usage: &'a crate::TokenUsage,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_prompt_usage: Option<&'a crate::TokenUsage>,
    protocol_turn_options: TurnOptionsIntent<'a>,
}

impl<'a> From<&'a crate::PersistedTurnState> for TurnStateIntent<'a> {
    fn from(state: &'a crate::PersistedTurnState) -> Self {
        let crate::PersistedTurnState {
            turn_index,
            token_usage,
            last_prompt_usage,
            protocol_turn_options,
        } = state;
        Self {
            turn_index: *turn_index,
            token_usage,
            last_prompt_usage: last_prompt_usage.as_ref(),
            protocol_turn_options: TurnOptionsIntent::from(protocol_turn_options),
        }
    }
}

/// [`crate::SessionNodePayload`] as identity content.
#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum NodePayloadIntent<'a> {
    Event {
        event: &'a crate::SessionHistoryRecord,
    },
    Plugin {
        plugin_type: &'a str,
        body: &'a SharedJsonValue,
    },
    FrameOpen {
        frame_key: &'a crate::FrameKey,
        reason: &'a crate::AgentFrameReason,
        assignment: &'a crate::AgentFrameAssignment,
        protocol_turn_options: TurnOptionsIntent<'a>,
    },
}

impl<'a> From<&'a crate::SessionNodePayload> for NodePayloadIntent<'a> {
    fn from(payload: &'a crate::SessionNodePayload) -> Self {
        match payload {
            crate::SessionNodePayload::Event { event } => Self::Event { event },
            crate::SessionNodePayload::Plugin { plugin_type, body } => {
                Self::Plugin { plugin_type, body }
            }
            crate::SessionNodePayload::FrameOpen {
                frame_key,
                reason,
                assignment,
                protocol_turn_options,
            } => Self::FrameOpen {
                frame_key,
                reason,
                assignment,
                protocol_turn_options: TurnOptionsIntent::from(protocol_turn_options),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each projection serializes exactly as its record does, except that
    /// the options it carries are their payload: the only identity bit the
    /// projection moves is the stamp it drops.
    #[test]
    fn projections_match_their_records_but_for_the_turn_options_stamp() {
        let options =
            crate::ProtocolTurnOptions::from_payload(serde_json::json!({"mode": {"left": 1}}));
        let without_stamp = |mut value: serde_json::Value, pointer: &str| {
            let options = value
                .pointer_mut(pointer)
                .expect("the record carries options");
            *options = options["payload"].take();
            value
        };

        let mut config = crate::PersistedSessionConfig::new(crate::TurnBudget::bounded(4));
        config.protocol_turn_options = Some(options.clone());
        config.prompt = Some(crate::PromptLayer::new());
        assert_eq!(
            serde_json::to_value(ConfigIntent::from(&config)).expect("config intent"),
            without_stamp(
                serde_json::to_value(&config).expect("config"),
                "/protocol_turn_options"
            )
        );

        let state = crate::PersistedTurnState {
            turn_index: 3,
            protocol_turn_options: options.clone(),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(TurnStateIntent::from(&state)).expect("turn state intent"),
            without_stamp(
                serde_json::to_value(&state).expect("turn state"),
                "/protocol_turn_options"
            )
        );

        let frame_open = crate::SessionNodePayload::FrameOpen {
            frame_key: crate::FrameKey::from_caller_material("identity-frame")
                .expect("a frame key"),
            reason: crate::AgentFrameReason::initial(),
            assignment: crate::AgentFrameAssignment::from_policy(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            )),
            protocol_turn_options: options,
        };
        assert_eq!(
            serde_json::to_value(NodePayloadIntent::from(&frame_open)).expect("frame intent"),
            without_stamp(
                serde_json::to_value(&frame_open).expect("frame open"),
                "/protocol_turn_options"
            )
        );
        let plugin = crate::SessionNodePayload::Plugin {
            plugin_type: "plugin".to_string(),
            body: SharedJsonValue::new(serde_json::json!({"k": [1, 2]})),
        };
        assert_eq!(
            serde_json::to_value(NodePayloadIntent::from(&plugin)).expect("plugin intent"),
            serde_json::to_value(&plugin).expect("plugin")
        );
    }
}
