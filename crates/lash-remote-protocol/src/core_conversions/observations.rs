use lash_sansio::sync::{LockResultExt, MutexExt};
impl RemoteTurnActivity {
    pub fn from_core(
        sequence: u64,
        activity: lash_core::TurnActivity,
    ) -> Result<Self, RemoteProtocolError> {
        let lash_core::TurnActivity {
            id: lash_core::TurnActivityId(id),
            correlation_id: lash_core::TurnActivityId(correlation_id),
            event,
        } = activity;
        Ok(Self {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            sequence,
            id: id.to_string(),
            correlation_id: correlation_id.to_string(),
            event: RemoteTurnEvent::try_from(event)?,
        })
    }
}

impl From<&lash_core::SessionCursor> for RemoteSessionCursor {
    fn from(value: &lash_core::SessionCursor) -> Self {
        Self::new(value.to_string())
    }
}

impl From<lash_core::SessionCursor> for RemoteSessionCursor {
    fn from(value: lash_core::SessionCursor) -> Self {
        Self::from(&value)
    }
}

impl TryFrom<RemoteSessionCursor> for lash_core::SessionCursor {
    type Error = RemoteProtocolError;

    fn try_from(value: RemoteSessionCursor) -> Result<Self, Self::Error> {
        value.validate()?;
        serde_json::from_value(serde_json::Value::String(value.cursor)).map_err(|err| {
            RemoteProtocolError::InvalidEnvelope {
                type_name: "RemoteSessionCursor",
                message: format!("invalid cursor payload: {err}"),
            }
        })
    }
}

impl RemoteSessionObservation {
    pub fn from_core(observation: lash_core::facade_support::SessionObservation) -> Self {
        observation.into()
    }
}

impl From<lash_core::facade_support::SessionObservation> for RemoteSessionObservation {
    fn from(value: lash_core::facade_support::SessionObservation) -> Self {
        let lash_core::facade_support::SessionObservation { read_view, cursor } = value;
        Self {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            session_id: read_view.session_id().to_string(),
            cursor: cursor.to_string(),
            turn_index: read_view.turn_index() as u64,
            usage: read_view.token_usage().clone().into(),
        }
    }
}

impl From<lash_core::SessionQueueEventKind> for RemoteSessionQueueEventKind {
    fn from(value: lash_core::SessionQueueEventKind) -> Self {
        match value {
            lash_core::SessionQueueEventKind::Enqueued => Self::Enqueued,
            lash_core::SessionQueueEventKind::Cancelled => Self::Cancelled,
        }
    }
}

impl From<lash_core::SessionProcessEventKind> for RemoteSessionProcessEventKind {
    fn from(value: lash_core::SessionProcessEventKind) -> Self {
        match value {
            lash_core::SessionProcessEventKind::Started => Self::Started,
            lash_core::SessionProcessEventKind::Cancelled => Self::Cancelled,
        }
    }
}

impl From<lash_core::CheckpointKind> for RemoteTurnInputCheckpoint {
    fn from(value: lash_core::CheckpointKind) -> Self {
        match value {
            lash_core::CheckpointKind::AfterWork => Self::AfterWork,
            lash_core::CheckpointKind::BeforeCompletion => Self::BeforeCompletion,
        }
    }
}

impl From<&lash_core::TurnInputApplication> for RemoteTurnInputApplication {
    fn from(value: &lash_core::TurnInputApplication) -> Self {
        Self {
            input_id: value.input_id.clone(),
            source_key: value.source_key.clone(),
            turn_id: value.turn_id.to_string(),
            committed_message_id: value.committed_message_id.clone(),
            checkpoint: value.checkpoint.map(Into::into),
        }
    }
}

impl RemoteSessionObservationEvent {
    pub fn from_core(
        sequence: u64,
        event: Arc<lash_core::SessionObservationEvent>,
    ) -> Result<Self, RemoteProtocolError> {
        let lash_core::SessionObservationEvent {
            session_id,
            replay_incarnation_id,
            turn_id,
            revision,
            cursor,
            payload,
        } = event.as_ref();
        let payload = match payload {
            lash_core::SessionObservationEventPayload::TurnActivity(activity) => {
                RemoteSessionObservationEventPayload::TurnActivity {
                    activity: Box::new(RemoteTurnActivity::from_core(sequence, activity.clone())?),
                }
            }
            // The committed read view is a local handle; only the commit
            // signal itself crosses the wire.
            lash_core::SessionObservationEventPayload::Committed { read_view: _ } => {
                RemoteSessionObservationEventPayload::Committed
            }
            // Resident replacements are also signal-only; the authoritative
            // read view remains a local handle and must be refetched by the peer.
            lash_core::SessionObservationEventPayload::ResidentChanged { read_view: _ } => {
                RemoteSessionObservationEventPayload::ResidentChanged
            }
            lash_core::SessionObservationEventPayload::AgentFrameSwitched { frame_id } => {
                RemoteSessionObservationEventPayload::AgentFrameSwitched {
                    frame_id: frame_id.clone(),
                }
            }
            lash_core::SessionObservationEventPayload::QueueChanged { kind, batch_ids } => {
                RemoteSessionObservationEventPayload::QueueChanged {
                    kind: (*kind).into(),
                    batch_ids: batch_ids.clone(),
                }
            }
            lash_core::SessionObservationEventPayload::ProcessChanged { kind, process_ids } => {
                RemoteSessionObservationEventPayload::ProcessChanged {
                    kind: (*kind).into(),
                    process_ids: process_ids.clone(),
                }
            }
        };
        Ok(Self {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            session_id: session_id.clone(),
            replay_incarnation_id: replay_incarnation_id.clone(),
            turn_id: turn_id.clone(),
            revision: revision.as_u64(),
            cursor: cursor.to_string(),
            event: payload,
        })
    }
}

impl From<lash_core::LiveReplayGapReason> for RemoteLiveReplayGapReason {
    fn from(value: lash_core::LiveReplayGapReason) -> Self {
        match value {
            lash_core::LiveReplayGapReason::Trimmed => Self::Trimmed,
            lash_core::LiveReplayGapReason::Unavailable => Self::Unavailable,
        }
    }
}

impl From<lash_core::facade_support::LiveReplayGap> for RemoteLiveReplayGap {
    fn from(value: lash_core::facade_support::LiveReplayGap) -> Self {
        let lash_core::facade_support::LiveReplayGap {
            session_id,
            requested_cursor,
            latest_cursor,
            latest_revision,
            reason,
        } = value;
        Self {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            session_id,
            requested_cursor: requested_cursor.to_string(),
            latest_cursor: latest_cursor.to_string(),
            latest_revision: latest_revision.as_u64(),
            reason: reason.into(),
        }
    }
}

impl TryFrom<lash_core::TurnEvent> for RemoteTurnEvent {
    type Error = RemoteProtocolError;

    fn try_from(value: lash_core::TurnEvent) -> Result<Self, RemoteProtocolError> {
        match value {
            lash_core::TurnEvent::QueuedWorkStarted {
                boundary,
                batch_ids,
                causes,
            } => Ok(Self::RuntimeDiagnostic {
                kind: "queued_work_started".to_string(),
                data: serde_json::json!({
                    "boundary": boundary,
                    "batch_ids": batch_ids,
                    "causes": causes,
                }),
            }),
            lash_core::TurnEvent::ModelRequestStarted { protocol_iteration } => {
                Ok(Self::ModelRequestStarted { protocol_iteration })
            }
            lash_core::TurnEvent::AssistantProseDelta { text } => {
                Ok(Self::AssistantProseDelta {
                    text: text.to_string(),
                })
            }
            lash_core::TurnEvent::ReasoningDelta { text } => Ok(Self::ReasoningDelta {
                text: text.to_string(),
            }),
            lash_core::TurnEvent::ModelAttemptReset {
                assistant_prose_correlation_ids,
                reasoning_correlation_ids,
            } => Ok(Self::ModelAttemptReset {
                assistant_prose_correlation_ids: assistant_prose_correlation_ids
                    .into_iter()
                    .map(|id| id.0.to_string())
                    .collect(),
                reasoning_correlation_ids: reasoning_correlation_ids
                    .into_iter()
                    .map(|id| id.0.to_string())
                    .collect(),
            }),
            lash_core::TurnEvent::ModelCallRecorded { record } => Ok(Self::ModelCallRecorded {
                record: record.into(),
            }),
            lash_core::TurnEvent::CodeBlockStarted {
                language,
                code,
                graph_key,
            } => Ok(Self::CodeBlockStarted {
                language,
                code,
                graph_key,
            }),
            lash_core::TurnEvent::CodeBlockCompleted {
                language,
                output,
                error,
                success,
                duration_ms,
                tool_call_ids,
                graph_key,
            } => Ok(Self::CodeBlockCompleted {
                language,
                output,
                error,
                success,
                duration_ms,
                tool_call_ids,
                graph_key,
            }),
            lash_core::TurnEvent::ToolCallStarted {
                call_id,
                name,
                args,
                graph_key,
                parent_call_id,
            } => Ok(Self::ToolCallStarted {
                call_id,
                name,
                args,
                graph_key,
                parent_call_id,
            }),
            lash_core::TurnEvent::ToolCallCompleted {
                call_id,
                name,
                args,
                output,
                duration_ms,
                graph_key,
                parent_call_id,
            } => Ok(Self::ToolCallCompleted {
                call_id,
                name,
                args,
                output: encode_remote_json(output, "RemoteTurnEvent", "output")?,
                duration_ms,
                graph_key,
                parent_call_id,
            }),
            lash_core::TurnEvent::ToolIntentOutcome { call_id, outcome } => {
                Ok(Self::ToolIntentOutcome {
                    call_id,
                    outcome: outcome.into(),
                })
            }
            lash_core::TurnEvent::FinalValue { value } => Ok(Self::FinalValue { value }),
            lash_core::TurnEvent::ToolValue { tool_name, value } => {
                Ok(Self::ToolValue { tool_name, value })
            }
            lash_core::TurnEvent::Usage {
                protocol_iteration,
                usage,
                cumulative,
            } => Ok(Self::Usage {
                protocol_iteration,
                usage: usage.into(),
                cumulative: cumulative.into(),
            }),
            lash_core::TurnEvent::ChildUsage {
                session_id,
                source,
                model,
                protocol_iteration,
                usage,
                cumulative,
            } => Ok(Self::ChildUsage {
                session_id,
                source,
                model,
                protocol_iteration,
                usage: usage.into(),
                cumulative: cumulative.into(),
            }),
            lash_core::TurnEvent::RetryStatus {
                wait_seconds,
                attempt,
                max_attempts,
                reason,
            } => Ok(Self::RetryStatus {
                wait_seconds,
                attempt,
                max_attempts,
                reason,
            }),
            lash_core::TurnEvent::PluginRuntime { plugin_id, event } => Ok(Self::RuntimeDiagnostic {
                kind: "plugin_runtime".to_string(),
                data: serde_json::json!({
                    "plugin_id": plugin_id,
                    "event": event,
                }),
            }),
            lash_core::TurnEvent::QueuedInputAccepted { applications } => {
                Ok(Self::TurnInputApplied {
                    applications: applications.iter().map(Into::into).collect(),
                })
            }
            lash_core::TurnEvent::QueuedMessagesCommitted {
                messages,
                checkpoint,
            } => Ok(Self::RuntimeDiagnostic {
                kind: "queued_messages_committed".to_string(),
                data: serde_json::json!({
                    "messages": messages,
                    "checkpoint": checkpoint,
                }),
            }),
            lash_core::TurnEvent::Error { message } => Ok(Self::Error { message }),
        }
    }
}

pub fn replay_collected_activities(
    activities: impl IntoIterator<Item = lash_core::TurnActivity>,
    first_sequence: u64,
) -> Result<Vec<RemoteTurnActivity>, RemoteProtocolError> {
    activities
        .into_iter()
        .enumerate()
        .map(|(idx, activity)| {
            RemoteTurnActivity::from_core(first_sequence.saturating_add(idx as u64), activity)
        })
        .collect()
}

/// Writes one remote activity as a newline-terminated JSON record and flushes
/// the writer before returning. Serialization, framing, and flushing share one
/// writer lock; write failures are retained for the host to inspect.
pub struct RemoteTurnActivitySink<W: Write + Send + 'static> {
    writer: Mutex<W>,
    next_sequence: AtomicU64,
    errors: Mutex<Vec<String>>,
}

impl<W: Write + Send + 'static> RemoteTurnActivitySink<W> {
    pub fn new(writer: W, first_sequence: u64) -> Self {
        Self {
            writer: Mutex::new(writer),
            next_sequence: AtomicU64::new(first_sequence),
            errors: Mutex::new(Vec::new()),
        }
    }

    pub fn take_errors(&self) -> Vec<String> {
        std::mem::take(&mut *self.errors.lock_recover())
    }

    pub fn into_inner(self) -> Result<W, W> {
        Ok(self.writer.into_inner().recover())
    }
}

impl<W: Write + Send + 'static> lash_core::facade_support::TurnActivitySink for RemoteTurnActivitySink<W> {
    fn emit<'life0, 'async_trait>(
        &'life0 self,
        activity: lash_core::TurnActivity,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let sequence = self.next_sequence.fetch_add(1, Ordering::SeqCst);
            let remote = match RemoteTurnActivity::from_core(sequence, activity) {
                Ok(remote) => remote,
                Err(err) => {
                    self.errors.lock_recover().push(err.to_string());
                    return;
                }
            };
            let result = {
                let mut writer = self
                    .writer
                    .lock_recover();
                serde_json::to_writer(&mut *writer, &remote)
                    .and_then(|_| {
                        writer
                            .write_all(b"\n")
                            .map_err(serde_json::Error::io)
                    })
                    .and_then(|_| writer.flush().map_err(serde_json::Error::io))
            };
            if let Err(err) = result {
                self.errors
                    .lock_recover()
                    .push(err.to_string());
            }
        })
    }
}
