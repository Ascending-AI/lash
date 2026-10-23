use lash_core::SessionAppendNode;
use serde_json::Value;

/// Ordinary writable variables for a fresh RLM frame.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct RlmSeed {
    pub globals: serde_json::Map<String, Value>,
}
impl RlmSeed {
    pub fn from_tool_args(args: &Value) -> Result<Self, String> {
        args.get("seed")
            .map_or_else(|| Ok(Self::default()), Self::from_seed_value)
    }
    pub fn from_seed_value(seed: &Value) -> Result<Self, String> {
        match seed {
            Value::Null => Ok(Self::default()),
            Value::Object(globals) => Ok(Self {
                globals: globals.clone(),
            }),
            _ => Err("`seed` must be a record/dict".into()),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.globals.is_empty()
    }
    pub fn into_event_body(self) -> lash_rlm_types::RlmSeedPluginBody {
        lash_rlm_types::RlmSeedPluginBody {
            globals: self.globals,
        }
    }
}
pub fn rlm_seed_initial_nodes(
    seed: RlmSeed,
    fleet: lash_core::FleetFormat,
) -> Vec<SessionAppendNode> {
    if seed.is_empty() {
        return Vec::new();
    }
    vec![SessionAppendNode::protocol_event(
        super::context::rlm_protocol_event(
            lash_rlm_types::RlmProtocolEvent::RlmSeed(seed.into_event_body()),
            fleet.writer_version(lash_core::surface_format!(
                crate::RLM_PROTOCOL_EVENT_VERSION
            )),
        ),
    )]
}
