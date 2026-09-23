//! Turn input envelopes: MIME-generic items, per-turn protocol options, and
//! the turn request.

use lash_sansio::SessionId;
use lash_sansio::TurnId;
use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::llm::RemoteAttachmentSource;
use crate::prompt::RemotePromptLayer;
use crate::registry_errors::{RemoteProtocolError, require_non_empty};
use crate::tools::RemoteToolGrant;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProtocolTurnOptions {
    #[serde(default = "empty_protocol_turn_payload")]
    pub payload: serde_json::Value,
}

fn empty_protocol_turn_payload() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

impl Default for RemoteProtocolTurnOptions {
    fn default() -> Self {
        Self {
            payload: empty_protocol_turn_payload(),
        }
    }
}

impl RemoteProtocolTurnOptions {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        match &self.payload {
            serde_json::Value::Object(map) => map.is_empty(),
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTurnInput {
    #[serde(default)]
    pub items: Vec<RemoteInputItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_turn_options: Option<RemoteProtocolTurnOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_layer: Option<RemotePromptLayer>,
}

impl RemoteTurnInput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            items: vec![RemoteInputItem::Text { text: text.into() }],
            protocol_turn_options: None,
            trace_turn_id: None,
            prompt_layer: None,
        }
    }

    /// Nested turn input remains a bare body.
    pub fn encode_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        crate::Envelope::new(self).encode_json()
    }

    pub fn decode_json(bytes: &[u8]) -> Result<Self, RemoteProtocolError> {
        let input = crate::Envelope::<Self>::decode_json(bytes)?.into_body();
        input.validate()?;
        Ok(input)
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        for (index, item) in self.items.iter().enumerate() {
            if let RemoteInputItem::Attachment { source } = item {
                source.validate(index)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteInputItem {
    Text { text: String },
    Attachment { source: RemoteAttachmentSource },
}

/// A request to add turn input to a session.
///
/// `session_id`, `turn_id`, `idempotency_key`, `tool_grants`, and `metadata`
/// are host-transport fields: they route and describe the request at the
/// process boundary and are consumed by the transport layer, not by the
/// `TurnInput` conversion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTurnRequest {
    /// Target session.
    pub session_id: SessionId,
    /// Stable turn identifier for the submitted input.
    ///
    /// Shared by every payload routed to this turn while it is open, including
    /// tool results, usage, and activity.
    pub turn_id: TurnId,
    /// Host-transport deduplication key.
    ///
    /// Lash does **not** deduplicate on this field: the conversion to
    /// [`lash_core::TurnInput`] drops it and no admission path reads it, so
    /// resending a request with the same key admits the turn again. Lash's
    /// turn-input idempotency key is the `source_key` the host supplies at
    /// admission; a transport that wants dedup on `idempotency_key` must
    /// enforce it before conversion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub input: RemoteTurnInput,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_grants: Vec<RemoteToolGrant>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, serde_json::Value>,
}

impl RemoteTurnRequest {
    pub fn encode_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        crate::Envelope::new(self).encode_json()
    }

    pub fn decode_json(bytes: &[u8]) -> Result<Self, RemoteProtocolError> {
        let request = crate::Envelope::<Self>::decode_json(bytes)?.into_body();
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteTurnRequest", "session_id", &self.session_id)?;
        require_non_empty("RemoteTurnRequest", "turn_id", &self.turn_id)?;
        self.input.validate()?;
        RemoteToolGrant::validate_all(&self.tool_grants)?;
        Ok(())
    }
}
