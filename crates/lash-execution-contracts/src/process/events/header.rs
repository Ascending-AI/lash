//! Immutable event metadata. Payload release never changes this encoding.

use serde::{Deserialize, Serialize};

use super::{ProcessEvent, ProcessLifecycleFact, cancellation_replay_preimage};
use crate::process::CanonicalProcessEventAppend;
use lash_vocabulary::identity::ProcessId;

/// The immutable part of every stored process event, written at append time.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessEventHeader {
    process_id: ProcessId,
    sequence: u64,
    event_type: String,
    invocation: crate::RuntimeInvocation,
    occurred_at: u64,
}

impl ProcessEventHeader {
    pub fn of(event: &ProcessEvent) -> Self {
        Self {
            process_id: event.process_id.clone(),
            sequence: event.sequence,
            event_type: event.fact.event_type().to_owned(),
            invocation: event.invocation.clone(),
            occurred_at: event.occurred_at,
        }
    }

    /// The canonical replay identity, independent of the retained payload.
    /// Cancellation uses its existing identity encoder, which excludes clocks.
    pub fn replay_digest(&self, fact: &ProcessLifecycleFact) -> String {
        let identity = match fact {
            ProcessLifecycleFact::CancelRequested(request) => {
                cancellation_replay_preimage(&self.process_id, request)
            }
            _ => crate::identity_json::payload_leaf(&fact.replay_identity()),
        };
        crate::stable_hash::sha256_hex(&identity)
    }

    /// Match a replay against the immutable digest. Retained payloads answer
    /// the original fact; released payloads use the matching request's fact.
    pub fn replay(
        self,
        payload: Option<&str>,
        digest: &str,
        requested: &CanonicalProcessEventAppend,
        fleet_format: crate::FleetFormat,
    ) -> Result<ProcessEvent, crate::PluginError> {
        if self.event_type != requested.fact().event_type()
            || self.replay_digest(requested.fact()) != digest
        {
            return Err(crate::durable_identity_conflict(format!(
                "process `{}` event replay key conflicts with event {}",
                self.process_id, self.sequence
            )));
        }
        let fact = match payload {
            Some(payload) => ProcessLifecycleFact::decode(
                &self.event_type,
                serde_json::from_str(payload).map_err(|error| {
                    crate::PluginError::Session(format!(
                        "failed to decode process event payload: {error}"
                    ))
                })?,
                fleet_format,
            )?,
            None => requested.fact().clone(),
        };
        Ok(ProcessEvent {
            process_id: self.process_id,
            sequence: self.sequence,
            fact,
            invocation: self.invocation,
            occurred_at: self.occurred_at,
        })
    }
}
