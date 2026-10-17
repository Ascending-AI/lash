use crate::*;
use serde::{Deserialize, Serialize};
/// A prepared registration as a store-local start's rows carry it: what a
/// dialect applies inside the commit that records the start's outcome.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagedRegistration {
    /// The registration, its trace span derived.
    pub registration: ProcessRegistration,
    /// The sessions that observe the process from its creation.
    pub observers: Vec<SessionId>,
    /// The process id the registrar minted.
    pub process_id: crate::ProcessId,
    /// When the registration was prepared: the row's creation instant.
    pub prepared_at_ms: u64,
}

impl StagedRegistration {
    /// The record its rows register once they commit.
    ///
    /// # Errors
    ///
    /// A registration its admission refuses.
    pub fn record(&self) -> Result<ProcessRecord, crate::PluginError> {
        Ok(ProcessRecord::from_prepared_registration(
            crate::prepare_process_registration(self.registration.clone())?,
            self.process_id.clone(),
            self.prepared_at_ms,
        ))
    }

    /// The rows a store-local start commits.
    ///
    /// # Errors
    ///
    /// A registration that does not encode.
    pub fn rows(&self) -> Result<lash_durable::domain::ProcessStartRows, crate::PluginError> {
        Ok(lash_durable::domain::ProcessStartRows {
            process: self.process_id.clone(),
            registration_json: serde_json::to_string(self).map_err(|error| {
                crate::PluginError::Session(format!("failed to encode a process start: {error}"))
            })?,
        })
    }

    /// The registration `rows` carry.
    ///
    /// # Errors
    ///
    /// Rows that do not decode.
    pub fn decode(
        rows: &lash_durable::domain::ProcessStartRows,
    ) -> Result<Self, crate::PluginError> {
        serde_json::from_str(&rows.registration_json).map_err(|error| {
            crate::PluginError::StoredDataCorrupt {
                record_kind: "process start rows".to_owned(),
                message: error.to_string(),
            }
        })
    }
}
