//! Validated RLM events shared by every reader of one retained event.
//!
//! A protocol event's payload keeps what was decoded from it
//! ([`lash_core::ProtocolPayload::derived`]), so the transcript, the history
//! index and a restore all read one decode of each retained event, and a
//! rewritten payload, a new value, validates afresh (FIG-6443). The decoder
//! holds nothing of its own.

use std::sync::Arc;

use lash_core::{ProtocolEvent, StoredDataCorruption};
use lash_rlm_types::RlmProtocolEvent;

#[derive(Default)]
pub(crate) struct EventDecoder;

impl EventDecoder {
    pub(crate) fn decode(
        &self,
        event: &ProtocolEvent,
    ) -> Result<Option<Arc<RlmProtocolEvent>>, StoredDataCorruption> {
        super::decode_rlm_protocol_event(event)
    }
}
