//! Turn input envelopes: MIME-generic items, per-turn protocol options, and
//! the turn request.

use lash_sansio::SessionId;
use lash_sansio::TurnId;
use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::llm::RemoteAttachmentSource;
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
    pub trace_turn_id: Option<TurnId>,
    /// The one field Phase A's synthetic N+1 adds at its newer protocol
    /// version (ADR 0115 §6). It travels only at that version: the encoder
    /// of N's version drops it, and a message at N's version never carries
    /// it.
    #[cfg(feature = "synthetic-next")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthetic_next_note: Option<String>,
}

impl RemoteTurnInput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            items: vec![RemoteInputItem::Text { text: text.into() }],
            trace_turn_id: None,
            #[cfg(feature = "synthetic-next")]
            synthetic_next_note: None,
        }
    }

    /// Nested turn input remains a bare body.
    pub fn encode_json(
        &self,
        negotiated: &crate::Negotiated,
    ) -> Result<Vec<u8>, serde_json::Error> {
        crate::Envelope::at(negotiated, self.at_version(negotiated.selected())).encode_json()
    }

    pub fn decode_json(bytes: &[u8]) -> Result<Self, RemoteProtocolError> {
        let envelope = crate::Envelope::<Self>::decode_json(bytes, crate::REMOTE_PROTOCOL)?;
        let version = envelope.protocol_version();
        let input = envelope.into_body().at_version(version).into_owned();
        input.validate()?;
        Ok(input)
    }

    /// This input as `version` carries it: the synthetic N+1's added field
    /// exists only above N's version (ADR 0115 §6), so N's encoder and
    /// decoder drop it.
    fn at_version(&self, version: u32) -> std::borrow::Cow<'_, Self> {
        #[cfg(feature = "synthetic-next")]
        if version <= crate::REMOTE_PROTOCOL_VERSION && self.synthetic_next_note.is_some() {
            return std::borrow::Cow::Owned(Self {
                synthetic_next_note: None,
                ..self.clone()
            });
        }
        let _ = version;
        std::borrow::Cow::Borrowed(self)
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
/// `session_id`, `turn_id`, `tool_grants`, and `metadata`
/// are host-transport fields: they route and describe the request at the
/// process boundary and are consumed by the transport layer, not by the
/// `TurnInput` conversion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTurnRequest {
    /// Target session.
    pub session_id: SessionId,
    /// Stable turn identifier for the submitted input: the root the input
    /// opens and its idempotency key at once. A transport sends it as
    /// `send(input).id(turn_id)`, so resending the same request answers the
    /// first acceptance instead of admitting the input again.
    ///
    /// Shared by every payload routed to this turn while it is open, including
    /// tool results, usage, and activity.
    pub turn_id: TurnId,
    pub input: RemoteTurnInput,
    /// Protocol turn options for this input's root only: the send's run
    /// spec overrides, merged over the session's options. They never become
    /// the session's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_turn_options: Option<RemoteProtocolTurnOptions>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_grants: Vec<RemoteToolGrant>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, serde_json::Value>,
}

impl RemoteTurnRequest {
    pub fn encode_json(
        &self,
        negotiated: &crate::Negotiated,
    ) -> Result<Vec<u8>, serde_json::Error> {
        crate::Envelope::at(negotiated, self.at_version(negotiated.selected())).encode_json()
    }

    pub fn decode_json(bytes: &[u8]) -> Result<Self, RemoteProtocolError> {
        let envelope = crate::Envelope::<Self>::decode_json(bytes, crate::REMOTE_PROTOCOL)?;
        let version = envelope.protocol_version();
        let request = envelope.into_body().at_version(version).into_owned();
        request.validate()?;
        Ok(request)
    }

    /// This request as `version` carries it: its input at that version.
    fn at_version(&self, version: u32) -> std::borrow::Cow<'_, Self> {
        match self.input.at_version(version) {
            std::borrow::Cow::Borrowed(_) => std::borrow::Cow::Borrowed(self),
            std::borrow::Cow::Owned(input) => std::borrow::Cow::Owned(Self {
                input,
                ..self.clone()
            }),
        }
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteTurnRequest", "session_id", &self.session_id)?;
        require_non_empty("RemoteTurnRequest", "turn_id", &self.turn_id)?;
        self.input.validate()?;
        RemoteToolGrant::validate_all(&self.tool_grants)?;
        Ok(())
    }
}
