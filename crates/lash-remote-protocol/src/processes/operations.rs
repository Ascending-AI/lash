use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessCancelRequest {
    pub process_id: String,
    pub incarnation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl RemoteProcessCancelRequest {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteProcessCancelRequest", "process_id", &self.process_id)?;
        RemoteProcessRef {
            process_id: self.process_id.clone(),
            incarnation: self.incarnation,
        }
        .validate("RemoteProcessCancelRequest")?;
        if let Some(reason) = &self.reason {
            require_non_empty("RemoteProcessCancelRequest", "reason", reason)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessCancelReceipt {
    pub process_id: String,
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
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessSignalRequest {
    pub process_id: String,
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
    pub process_id: String,
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
    pub process_id: String,
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
pub struct RemoteProcessEventsRequest {
    pub process_id: String,
    pub incarnation: u64,
    #[serde(default)]
    pub after_sequence: u64,
}

impl RemoteProcessEventsRequest {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        RemoteProcessRef {
            process_id: self.process_id.clone(),
            incarnation: self.incarnation,
        }
        .validate("RemoteProcessEventsRequest")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteProcessEventsResponse {
    pub process_id: String,
    pub incarnation: u64,
    #[serde(default)]
    pub events: Vec<RemoteProcessEvent>,
}

impl RemoteProcessEventsResponse {
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
        for event in &self.events {
            event.validate("RemoteProcessEventsResponse")?;
        }
        Ok(())
    }
}
