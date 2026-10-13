//! Validated RLM events shared by the readers of one live runtime.
//!
//! The session plugin creates this cache with its dialect; reopening the
//! runtime creates a fresh cache. Entries are derived values, never store
//! reads or grants. The complete canonical event bytes are the key, so a
//! rewrite, including one under an unchanged event id, validates afresh.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use lash_core::{ProtocolEvent, StoredDataCorruption};
use lash_rlm_types::RlmProtocolEvent;

#[derive(Default)]
pub(crate) struct EventCache(Mutex<Entries>);

#[derive(Default)]
struct Entries {
    scratch: Vec<u8>,
    decoded: HashMap<Vec<u8>, Arc<RlmProtocolEvent>>,
}

impl EventCache {
    pub(crate) fn decode(
        &self,
        event: &ProtocolEvent,
    ) -> Result<Option<Arc<RlmProtocolEvent>>, StoredDataCorruption> {
        if event.plugin_id != crate::RLM_PROTOCOL_PLUGIN_ID {
            return Ok(None);
        }
        let mut entries = self.0.lock().unwrap_or_else(|error| error.into_inner());
        entries.scratch.clear();
        serde_json::to_writer(&mut entries.scratch, &event.payload).map_err(|error| {
            StoredDataCorruption {
                record_kind: "RLM protocol event".into(),
                message: error.to_string(),
            }
        })?;
        if let Some(decoded) = entries.decoded.get(entries.scratch.as_slice()) {
            return Ok(Some(Arc::clone(decoded)));
        }
        let decoded = super::decode_rlm_protocol_event(event)?.map(Arc::new);
        if let Some(decoded) = &decoded {
            let key = entries.scratch.clone();
            entries.decoded.insert(key, Arc::clone(decoded));
        }
        Ok(decoded)
    }

    pub(crate) fn clear(&self) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = Entries::default();
    }
}
