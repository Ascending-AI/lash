//! SQL stores keep only the event facts that aren't in indexed columns here.

use serde::{Deserialize, Serialize};

use super::{ProcessEvent, ProcessLifecycleFact, cancellation_replay_preimage};
use crate::process::CanonicalProcessEventAppend;
use lash_vocabulary::identity::ProcessId;

/// The immutable event metadata stored beside its SQL coordinates.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessEventHeader {
    occurred_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replay_attribution: Option<crate::RuntimeReplayAttribution>,
}

impl ProcessEventHeader {
    pub fn of(event: &ProcessEvent) -> Self {
        Self {
            occurred_at: event.occurred_at,
            replay_attribution: event.invocation.replay_attribution().cloned(),
        }
    }
}

impl ProcessEvent {
    /// The canonical replay identity, independent of the retained payload.
    pub fn replay_digest(&self) -> String {
        replay_digest(&self.process_id, &self.fact)
    }
}

fn replay_digest(process_id: &ProcessId, fact: &ProcessLifecycleFact) -> String {
    let identity = match fact {
        ProcessLifecycleFact::CancelRequested(request) => {
            cancellation_replay_preimage(process_id, request)
        }
        _ => crate::identity_json::payload_leaf(&fact.replay_identity()),
    };
    crate::stable_hash::sha256_hex(&identity)
}

/// The full event projection, after selecting its process by SQL key.
pub struct ProcessEventRow {
    pub sequence: i64,
    pub event_type: String,
    pub replay_key: Option<String>,
    pub header: String,
    pub payload: Option<String>,
}

impl ProcessEventRow {
    pub fn sequence(&self) -> Result<u64, crate::PluginError> {
        u64::try_from(self.sequence)
            .map_err(|_| event_corrupt(format!("negative event sequence {}", self.sequence)))
    }

    /// Decode the retained payload using the row's kind and position.
    pub fn decode(
        self,
        process_id: &ProcessId,
        fleet_format: crate::FleetFormat,
    ) -> Result<ProcessEvent, crate::PluginError> {
        let fact = self.decode_fact(fleet_format)?;
        self.event(process_id, fact)
    }

    /// Match the canonical digest, then use the matching append to recover a
    /// released payload. The replay receipt keeps the original SQL position.
    pub fn replay(
        self,
        process_id: &ProcessId,
        digest: &str,
        requested: &CanonicalProcessEventAppend,
        fleet_format: crate::FleetFormat,
    ) -> Result<ProcessEvent, crate::PluginError> {
        if self.event_type != requested.fact().event_type()
            || replay_digest(process_id, requested.fact()) != digest
        {
            return Err(crate::durable_identity_conflict(format!(
                "process `{process_id}` event replay key conflicts with event {}",
                self.sequence()?
            )));
        }
        let fact = if self.payload.is_some() {
            self.decode_fact(fleet_format)?
        } else {
            requested.fact().clone()
        };
        self.event(process_id, fact)
    }

    fn decode_fact(
        &self,
        fleet_format: crate::FleetFormat,
    ) -> Result<ProcessLifecycleFact, crate::PluginError> {
        let payload = serde_json::from_str(self.payload.as_deref().unwrap_or("null"))
            .map_err(|error| event_corrupt(format!("invalid event payload: {error}")))?;
        ProcessLifecycleFact::decode(&self.event_type, payload, fleet_format)
    }

    fn event(
        self,
        process_id: &ProcessId,
        fact: ProcessLifecycleFact,
    ) -> Result<ProcessEvent, crate::PluginError> {
        let sequence = self.sequence()?;
        let header: ProcessEventHeader = serde_json::from_str(&self.header)
            .map_err(|error| event_corrupt(format!("invalid event header: {error}")))?;
        let replay = self.replay_key.map(|key| crate::RuntimeReplay {
            key,
            attribution: header.replay_attribution,
        });
        let invocation =
            crate::process_event_invocation(process_id, sequence, &self.event_type, replay);
        Ok(ProcessEvent {
            process_id: process_id.clone(),
            sequence,
            fact,
            invocation,
            occurred_at: header.occurred_at,
        })
    }
}

fn event_corrupt(message: String) -> crate::PluginError {
    crate::PluginError::StoredDataCorrupt {
        record_kind: "process_event".to_owned(),
        message,
    }
}
