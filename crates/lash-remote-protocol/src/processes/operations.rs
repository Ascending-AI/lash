use super::*;
#[cfg(any(feature = "core-conversions", test))]
use lash_sansio::ProcessId;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessCancelRequest {
    pub process_id: ProcessId,
    pub requester: String,
}

impl RemoteProcessCancelRequest {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteProcessCancelRequest", "process_id", &self.process_id)?;
        require_non_empty("RemoteProcessCancelRequest", "requester", &self.requester)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessCancelReceipt {
    pub origin: lash_sansio::CancelOrigin,
    pub process_id: ProcessId,
    pub status: RemoteProcessStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<RemoteProcessRecord>,
}

impl RemoteProcessCancelReceipt {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteProcessCancelReceipt", "process_id", &self.process_id)?;
        if let Some(record) = &self.record {
            record.validate("RemoteProcessCancelReceipt")?;
            if record.status != self.status {
                return Err(RemoteProtocolError::InvalidEnvelope {
                    type_name: "RemoteProcessCancelReceipt",
                    message: format!(
                        "cancel receipt status `{:?}` contradicts its record status `{:?}`",
                        self.status, record.status
                    ),
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessSignalRequest {
    pub process_id: ProcessId,
    pub signal_name: String,
    pub signal_id: String,
    #[serde(default)]
    pub payload: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_key: Option<String>,
}

impl RemoteProcessSignalRequest {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteProcessSignalRequest", "process_id", &self.process_id)?;
        require_non_empty(
            "RemoteProcessSignalRequest",
            "signal_name",
            &self.signal_name,
        )?;
        require_non_empty("RemoteProcessSignalRequest", "signal_id", &self.signal_id)?;
        if let Some(replay_key) = &self.replay_key {
            require_non_empty("RemoteProcessSignalRequest", "replay_key", replay_key)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessSignalReceipt {
    pub event: RemoteProcessEvent,
}

impl RemoteProcessSignalReceipt {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        self.event.validate("RemoteProcessSignalReceipt")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessAwaitRequest {
    pub process_id: ProcessId,
}

impl RemoteProcessAwaitRequest {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessAwaitOutcome {
    pub process_id: ProcessId,
    pub output: RemoteProcessAwaitOutput,
}

impl RemoteProcessAwaitOutcome {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteProcessAwaitOutcome", "process_id", &self.process_id)?;
        self.output.validate("RemoteProcessAwaitOutcome")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[cfg(any(feature = "core-conversions", test))]
pub struct RemoteProcessEventsRequest {
    pub process_id: ProcessId,
    pub limit: std::num::NonZeroUsize,
    pub mode: lash_core::ProcessEventQueryMode,
    /// The process cursor to read after; absent reads from the start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<lash_sansio::ProcessCursor>,
}

#[cfg(any(feature = "core-conversions", test))]
impl RemoteProcessEventsRequest {
    pub fn encode_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        crate::Envelope::new(self).encode_json()
    }

    pub fn decode_json(bytes: &[u8]) -> Result<Self, RemoteProtocolError> {
        let request = crate::Envelope::<Self>::decode_json(bytes)?.into_body();
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        if let Some(cursor) = &self.cursor
            && !cursor.reference().names(&self.process_id)
        {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteProcessEventsRequest",
                message: "cursor names another process".to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[cfg(any(feature = "core-conversions", test))]
pub struct RemoteProcessEventsResponse {
    pub process_id: ProcessId,
    pub outcome: lash_core::ProcessEventReadOutcome<
        lash_core::ProcessEventPage<RemoteProcessEvent, lash_core::ProcessEventLite>,
    >,
    /// The cursor after this page: its sequence is the last event returned,
    /// or the request's when the page returned none.
    pub cursor: lash_sansio::ProcessCursor,
}

#[cfg(any(feature = "core-conversions", test))]
impl RemoteProcessEventsResponse {
    pub fn encode_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        crate::Envelope::new(self).encode_json()
    }

    pub fn decode_json(bytes: &[u8]) -> Result<Self, RemoteProtocolError> {
        let response = crate::Envelope::<Self>::decode_json(bytes)?.into_body();
        response.validate()?;
        Ok(response)
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty(
            "RemoteProcessEventsResponse",
            "process_id",
            &self.process_id,
        )?;
        if !self.cursor.reference().names(&self.process_id) {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteProcessEventsResponse",
                message: "cursor names another process".to_string(),
            });
        }
        if let lash_core::ProcessEventReadOutcome::Retained(page) = &self.outcome {
            if let Some(last) = page.last_sequence(|event| event.sequence, |event| event.sequence)
                && last != self.cursor.sequence()
            {
                return Err(RemoteProtocolError::InvalidEnvelope {
                    type_name: "RemoteProcessEventsResponse",
                    message: "cursor sequence is not the page's last event".to_string(),
                });
            }
            if let lash_core::ProcessEventPageEvents::Full(events) = &page.events {
                for event in events {
                    event.validate("RemoteProcessEventsResponse")?;
                    if event.process_id != self.process_id {
                        return Err(RemoteProtocolError::InvalidEnvelope {
                            type_name: "RemoteProcessEventsResponse",
                            message: "event belongs to another process".to_string(),
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessStartRequest {
    /// The caller's idempotency key (ADR 0107): while the process started
    /// under it is retained, a retry returns that process. Absent, every
    /// request starts a new process. Never an identity: the registrar mints
    /// the process id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_key: Option<String>,
    pub input: RemoteProcessInput,
    pub disposition: RemoteRecoveryContract,
    pub lifetime: RemoteStartLifetime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_spec: Option<RemoteProcessExecutionEnvSpec>,
    pub originator: RemoteProcessOriginator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<RemoteDeclaredProcessIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observers: Vec<SessionId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub event_types: Vec<RemoteProcessEventType>,
}

impl RemoteProcessStartRequest {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        if let Some(start_key) = &self.start_key {
            require_non_empty("RemoteProcessStartRequest", "start_key", start_key)?;
        }
        if self.max_attempts == Some(0) {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteProcessStartRequest",
                message: "max_attempts must be greater than zero when provided".to_string(),
            });
        }
        self.lifetime.validate("RemoteProcessStartRequest")?;
        self.input.validate("RemoteProcessStartRequest")?;
        if let Some(env_spec) = &self.env_spec {
            env_spec.validate("RemoteProcessStartRequest")?;
        }
        if let Some(identity) = &self.identity {
            identity.validate("RemoteProcessStartRequest")?;
        }
        self.originator.validate("RemoteProcessStartRequest")?;
        if let Some(wake_session_id) = &self.wake_session_id {
            require_non_empty(
                "RemoteProcessStartRequest",
                "wake_session_id",
                wake_session_id,
            )?;
        }
        for observer in &self.observers {
            require_non_empty("RemoteProcessStartRequest", "observers", observer)?;
        }
        for event_type in &self.event_types {
            event_type.validate("RemoteProcessStartRequest")?;
        }
        Ok(())
    }
}
