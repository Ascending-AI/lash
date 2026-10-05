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
    /// Cancel one process on behalf of `requester`.
    pub fn new(process_id: ProcessId, requester: impl Into<String>) -> Self {
        Self {
            process_id,
            requester: requester.into(),
        }
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
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
        if let Some(record) = &self.record {
            record.validate("RemoteProcessCancelReceipt")?;
            if record.status() != self.status {
                return Err(RemoteProtocolError::InvalidEnvelope {
                    type_name: "RemoteProcessCancelReceipt",
                    message: format!(
                        "cancel receipt status `{:?}` contradicts its record status `{:?}`",
                        self.status,
                        record.status()
                    ),
                });
            }
        }
        Ok(())
    }
}

/// One signal to one process. `signal_id` is the sender's id for this one
/// signal: with the process and the signal name it is the signal's whole
/// identity, and the append that delivers it is deduplicated by that
/// identity alone (FIG-4299). There is no separate replay key to supply; a
/// request that carries one is refused, not silently re-keyed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteProcessSignalRequest {
    pub process_id: ProcessId,
    pub signal_name: String,
    pub signal_id: String,
    #[serde(default)]
    pub payload: serde_json::Value,
    /// What caused the signal: its producer's trace context. No part of the
    /// signal's identity.
    #[serde(default, skip_serializing_if = "lash_trace::TraceCause::is_root")]
    pub trace_cause: lash_trace::TraceCause,
}

impl RemoteProcessSignalRequest {
    /// One identified signal with no trace cause. Set a cause only when one
    /// was captured by the producer; a root cause is omitted on the wire.
    pub fn new(
        process_id: ProcessId,
        signal_name: impl Into<String>,
        signal_id: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            process_id,
            signal_name: signal_name.into(),
            signal_id: signal_id.into(),
            payload,
            trace_cause: Default::default(),
        }
    }
    /// Attach the producer's optional trace ancestry.
    pub fn with_trace_cause(mut self, trace_cause: lash_trace::TraceCause) -> Self {
        self.trace_cause = trace_cause;
        self
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty(
            "RemoteProcessSignalRequest",
            "signal_name",
            &self.signal_name,
        )?;
        require_non_empty("RemoteProcessSignalRequest", "signal_id", &self.signal_id)?;
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
    /// Await the terminal of one exact process lifetime.
    pub fn new(process_id: ProcessId) -> Self {
        Self { process_id }
    }

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
    /// Read a bounded process-event page from the beginning. Assign `cursor`
    /// to resume a previous page.
    pub fn new(
        process_id: ProcessId,
        limit: std::num::NonZeroUsize,
        mode: lash_core::ProcessEventQueryMode,
    ) -> Self {
        Self {
            process_id,
            limit,
            mode,
            cursor: None,
        }
    }

    pub fn encode_json(
        &self,
        negotiated: &crate::Negotiated,
    ) -> Result<Vec<u8>, serde_json::Error> {
        crate::Envelope::at(negotiated, self).encode_json()
    }

    pub fn decode_json(bytes: &[u8]) -> Result<Self, RemoteProtocolError> {
        let request =
            crate::Envelope::<Self>::decode_json(bytes, crate::REMOTE_PROTOCOL)?.into_body();
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
    pub fn encode_json(
        &self,
        negotiated: &crate::Negotiated,
    ) -> Result<Vec<u8>, serde_json::Error> {
        crate::Envelope::at(negotiated, self).encode_json()
    }

    pub fn decode_json(bytes: &[u8]) -> Result<Self, RemoteProtocolError> {
        let response =
            crate::Envelope::<Self>::decode_json(bytes, crate::REMOTE_PROTOCOL)?.into_body();
        response.validate()?;
        Ok(response)
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
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
#[serde(deny_unknown_fields)]
pub struct RemoteProcessStartRequest {
    /// The caller's idempotency key (ADR 0107): while the process started
    /// under it is retained, a retry returns that process. Absent, every
    /// request starts a new process. Never an identity: the registrar mints
    /// the process id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_key: Option<String>,
    pub input: RemoteProcessStartTarget,
    pub lifetime: RemoteStartLifetime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_ref: Option<RemoteProcessExecutionEnvRef>,
    pub originator: RemoteProcessOriginator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<RemoteDeclaredProcessIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observers: Vec<SessionId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub event_types: Vec<RemoteProcessEventType>,
    /// What caused the start: the caller's trace context. No part of what
    /// the start key fences.
    #[serde(default, skip_serializing_if = "lash_trace::TraceCause::is_root")]
    pub trace_cause: lash_trace::TraceCause,
}

impl RemoteProcessStartRequest {
    /// A process start with the wire's defaults for optional fields. The
    /// registrar supplies identity; set `start_key` to make retries idempotent.
    pub fn new(
        input: RemoteProcessStartTarget,
        lifetime: RemoteStartLifetime,
        originator: RemoteProcessOriginator,
    ) -> Self {
        Self {
            input,
            lifetime,
            originator,
            start_key: None,
            env_ref: None,
            identity: None,
            wake_session_id: None,
            observers: Vec::new(),
            event_types: Vec::new(),
            trace_cause: Default::default(),
        }
    }
    /// Attach the caller's optional trace ancestry without changing identity.
    pub fn with_trace_cause(mut self, trace_cause: lash_trace::TraceCause) -> Self {
        self.trace_cause = trace_cause;
        self
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        if let Some(start_key) = &self.start_key {
            require_non_empty("RemoteProcessStartRequest", "start_key", start_key)?;
        }
        self.input.validate("RemoteProcessStartRequest")?;
        if let Some(env_ref) = &self.env_ref {
            env_ref.validate("RemoteProcessStartRequest")?;
        }
        if let Some(identity) = &self.identity {
            identity.validate("RemoteProcessStartRequest")?;
        }
        self.originator.validate("RemoteProcessStartRequest")?;
        for event_type in &self.event_types {
            event_type.validate("RemoteProcessStartRequest")?;
        }
        Ok(())
    }
}
