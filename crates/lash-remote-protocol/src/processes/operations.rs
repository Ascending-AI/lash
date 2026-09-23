use super::*;
use lash_core::ProcessEventPageTokenStoreExt;
use lash_sansio::ProcessId;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessCancelRequest {
    pub process_id: ProcessId,
    pub incarnation: u64,
    pub requester: String,
}

impl RemoteProcessCancelRequest {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteProcessCancelRequest", "process_id", &self.process_id)?;
        RemoteProcessRef {
            process_id: self.process_id.clone(),
            incarnation: self.incarnation,
        }
        .validate("RemoteProcessCancelRequest")?;
        require_non_empty("RemoteProcessCancelRequest", "requester", &self.requester)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessCancelReceipt {
    pub origin: lash_sansio::CancelOrigin,
    pub process_id: ProcessId,
    pub incarnation: u64,
    pub status: RemoteProcessStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<RemoteProcessRecord>,
}

impl RemoteProcessCancelReceipt {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteProcessCancelReceipt", "process_id", &self.process_id)?;
        RemoteProcessRef {
            process_id: self.process_id.clone(),
            incarnation: self.incarnation,
        }
        .validate("RemoteProcessCancelReceipt")?;
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
    pub incarnation: u64,
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
        RemoteProcessRef {
            process_id: self.process_id.clone(),
            incarnation: self.incarnation,
        }
        .validate("RemoteProcessSignalRequest")?;
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
    pub incarnation: u64,
}

impl RemoteProcessAwaitRequest {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        RemoteProcessRef {
            process_id: self.process_id.clone(),
            incarnation: self.incarnation,
        }
        .validate("RemoteProcessAwaitRequest")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessAwaitOutcome {
    pub process_id: ProcessId,
    pub incarnation: u64,
    pub output: RemoteProcessAwaitOutput,
}

impl RemoteProcessAwaitOutcome {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteProcessAwaitOutcome", "process_id", &self.process_id)?;
        RemoteProcessRef {
            process_id: self.process_id.clone(),
            incarnation: self.incarnation,
        }
        .validate("RemoteProcessAwaitOutcome")?;
        self.output.validate("RemoteProcessAwaitOutcome")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessEventsRequest {
    pub process_id: ProcessId,
    pub incarnation: u64,
    pub limit: std::num::NonZeroUsize,
    pub mode: lash_core::ProcessEventQueryMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub continuation: Option<lash_core::ProcessEventPageToken>,
}

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
        RemoteProcessRef {
            process_id: self.process_id.clone(),
            incarnation: self.incarnation,
        }
        .validate("RemoteProcessEventsRequest")?;
        if let Some(token) = &self.continuation
            && (token.process_id() != self.process_id
                || token.process_incarnation().registration_sequence() != self.incarnation
                || token.mode() != self.mode)
        {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteProcessEventsRequest",
                message: "continuation belongs to another process incarnation or mode".to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessEventsResponse {
    pub process_id: ProcessId,
    pub incarnation: u64,
    pub outcome: lash_core::ProcessEventReadOutcome<
        lash_core::ProcessEventPage<RemoteProcessEvent, lash_core::ProcessEventLite>,
    >,
}

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
        RemoteProcessRef {
            process_id: self.process_id.clone(),
            incarnation: self.incarnation,
        }
        .validate("RemoteProcessEventsResponse")?;
        if let lash_core::ProcessEventReadOutcome::Retained(page) = &self.outcome {
            let mode = match &page.events {
                lash_core::ProcessEventPageEvents::Full(_) => {
                    lash_core::ProcessEventQueryMode::Full
                }
                lash_core::ProcessEventPageEvents::Lite(_) => {
                    lash_core::ProcessEventQueryMode::Lite
                }
            };
            if let lash_core::ProcessEventPageMore::More { continuation } = &page.more
                && (continuation.process_id() != self.process_id
                    || continuation.process_incarnation().registration_sequence()
                        != self.incarnation
                    || continuation.mode() != mode)
            {
                return Err(RemoteProtocolError::InvalidEnvelope {
                    type_name: "RemoteProcessEventsResponse",
                    message: "page continuation belongs to another process incarnation or mode"
                        .to_string(),
                });
            }
            if let lash_core::ProcessEventPageEvents::Full(events) = &page.events {
                for event in events {
                    event.validate("RemoteProcessEventsResponse")?;
                    if event.process_id != self.process_id
                        || event.process_incarnation != self.incarnation
                    {
                        return Err(RemoteProtocolError::InvalidEnvelope {
                            type_name: "RemoteProcessEventsResponse",
                            message: "event belongs to another process incarnation".to_string(),
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
    pub id: ProcessId,
    pub input: RemoteProcessInput,
    pub disposition: RemoteRecoveryContract,
    pub lifecycle: Option<RemoteProcessLifecyclePolicy>,
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
        require_non_empty("RemoteProcessStartRequest", "id", &self.id)?;
        if self.max_attempts == Some(0) {
            return Err(RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteProcessStartRequest",
                message: "max_attempts must be greater than zero when provided".to_string(),
            });
        }
        self.lifecycle
            .as_ref()
            .ok_or_else(|| RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteProcessStartRequest",
                message: "lifecycle policy is required".to_string(),
            })?
            .validate("RemoteProcessStartRequest", &self.originator)?;
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
