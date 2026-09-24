use super::*;

/// Serialized schema version for [`TurnCheckpoint`].
///
/// Version 1 is the historical unstamped checkpoint shape. Version 2 adds the
/// host-reporting effect for tool calls refused before dispatch. Version 3
/// removes the terminal-turn scheduling state; older checkpoints are not
/// compatible because replaying them could re-enter the deleted extra turn.
/// Version 4 was the typed-failure-code representation audit. Version 5
/// (FIG-3371) carries the streamed-block event vocabulary:
/// `SessionStreamEvent` inside `Effect::Emit` gained `stream_block_started` /
/// `stream_block_completed` and required `block` identities on text and
/// reasoning deltas, so v4 checkpoints holding pending text effects cannot
/// deserialize faithfully.
/// Version 6 carries parts-only plugin messages and removes part lifecycle fields.
/// Version 7 (FIG-3435) renames failure codes by
/// author: workspace-authored codes serialize as `lash:<spelling>` where
/// earlier checkpoints wrote `adapter:`/`refusal:`, and a v6 reader would
/// recolor a `lash:` code as provider vocabulary rather than refuse it.
/// Version 8 (FIG-3515) answers each tool call with one tool-result part
/// carrying ordered text and attachment `blocks`; v7 checkpoints holding
/// text-only results or call-bound attachment parts are refused.
/// Version 9 (FIG-3586) records the replay-key grammar the iteration's
/// execution-environment sync served (`synced_cell_replay_grammar`); a v8
/// checkpoint has no stamp, and a cell resumed from it would run under no
/// grammar.
/// Version 10 (FIG-3587) drops `update_machine_config` from the pending
/// execution-environment sync: every sync, the protocol-start one included,
/// now returns the environment the iteration's model call is built from, and
/// a v9 checkpoint parked on a host-only protocol-start sync would resume
/// with its first model call built from the live registry.
pub const TURN_CHECKPOINT_SCHEMA_VERSION: u32 = 10;

const fn legacy_turn_checkpoint_schema_version() -> u32 {
    1
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum EffectDeliveryStatus {
    #[default]
    Pending,
    Delivered,
}

/// The delivery bookkeeping of the effect a waiting variant is holding:
/// the one spelling of the effect's id plus whether the machine has handed
/// the effect to the host. Serialized transparently as the bare id, so the
/// checkpoint spells it under the variant's `effect_id` key exactly as the
/// id field it replaced; the flag is runtime-only and a restored
/// checkpoint always re-delivers.
#[derive(Clone, Copy, Debug, Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(super) struct EffectDelivery {
    pub(super) id: EffectId,
    #[serde(skip)]
    pub(super) status: EffectDeliveryStatus,
}

impl EffectDelivery {
    pub(super) fn pending(id: EffectId) -> Self {
        Self {
            id,
            status: EffectDeliveryStatus::Pending,
        }
    }
}

#[derive(Debug, Serialize, serde::Deserialize)]
pub(super) enum MachineState<M: TurnProtocol = UnitTurnProtocol> {
    PreparingProtocol,
    WaitingExecutionEnvironment {
        #[serde(rename = "effect_id")]
        delivery: EffectDelivery,
    },
    PrepareIteration,
    WaitingLlm {
        #[serde(rename = "effect_id")]
        delivery: EffectDelivery,
        request: Arc<LlmRequest>,
        driver_state: Option<M::DriverState>,
    },
    WaitingTools {
        #[serde(rename = "effect_id")]
        delivery: EffectDelivery,
        calls: Vec<PendingToolCall>,
    },
    WaitingExec {
        #[serde(rename = "effect_id")]
        delivery: EffectDelivery,
        language: String,
        code: String,
        driver_state: M::DriverState,
    },
    WaitingCheckpoint {
        #[serde(rename = "effect_id")]
        delivery: EffectDelivery,
        checkpoint: CheckpointKind,
        on_empty: CheckpointResumeAction,
    },
    Finished,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnCheckpoint<M: TurnProtocol = UnitTurnProtocol> {
    #[serde(default = "legacy_turn_checkpoint_schema_version")]
    pub(super) schema_version: u32,
    pub(super) state: MachineState<M>,
    pub(super) pending_effects: Vec<Effect<M>>,
    pub(super) next_effect_id: u64,
    #[serde(default)]
    pub(super) next_synthetic_message_id: u64,
    pub(super) messages: Vec<Message>,
    pub(super) events: Vec<SessionHistoryRecord<M::Event>>,
    #[serde(default)]
    pub(super) turn_causes: Vec<TurnCause>,
    #[serde(default)]
    pub(super) progress_event_cursor: usize,
    pub(super) protocol_iteration: usize,
    pub(super) protocol_run_offset: usize,
    pub(super) cumulative_usage: TokenUsage,
    pub(super) synced_protocol_iteration: Option<usize>,
    /// The cell replay-key grammar the synced iteration's execution
    /// environment named (FIG-3586). Absent in a checkpoint a build without
    /// the field wrote, which leaves that iteration's cells unrunnable on
    /// replay — the fail-closed answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) synced_cell_replay_grammar: Option<u32>,
}

impl<M: TurnProtocol> TurnCheckpoint<M> {
    /// Returns the schema version decoded from this checkpoint.
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Decode a JSON checkpoint and refuse every non-current durable shape.
    pub fn from_json_slice(bytes: &[u8]) -> Result<Self, TurnCheckpointRestoreError>
    where
        Self: serde::de::DeserializeOwned,
    {
        let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|error| {
            TurnCheckpointRestoreError::IncompatibleFormat {
                message: error.to_string(),
            }
        })?;
        let actual = value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            .and_then(|version| u32::try_from(version).ok())
            .unwrap_or_else(legacy_turn_checkpoint_schema_version);
        if actual != TURN_CHECKPOINT_SCHEMA_VERSION {
            return Err(TurnCheckpointRestoreError::IncompatibleSchemaVersion {
                actual,
                expected: TURN_CHECKPOINT_SCHEMA_VERSION,
            });
        }
        serde_json::from_value(value).map_err(|error| {
            TurnCheckpointRestoreError::IncompatibleFormat {
                message: error.to_string(),
            }
        })
    }
}

/// Failure to decode or restore an incompatible turn checkpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnCheckpointRestoreError {
    /// The checkpoint was written by a different schema than this build reads.
    IncompatibleSchemaVersion { actual: u32, expected: u32 },
    /// The bytes do not match the current closed checkpoint shape.
    IncompatibleFormat { message: String },
}

impl std::fmt::Display for TurnCheckpointRestoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IncompatibleSchemaVersion { actual, expected } => write!(
                formatter,
                "turn checkpoint is schema version {actual}, but this build requires {expected}"
            ),
            Self::IncompatibleFormat { message } => {
                write!(
                    formatter,
                    "turn checkpoint has an incompatible format: {message}"
                )
            }
        }
    }
}

impl std::error::Error for TurnCheckpointRestoreError {}

impl<M: TurnProtocol> Clone for MachineState<M> {
    fn clone(&self) -> Self {
        match self {
            Self::PreparingProtocol => Self::PreparingProtocol,
            Self::WaitingExecutionEnvironment { delivery } => Self::WaitingExecutionEnvironment {
                delivery: *delivery,
            },
            Self::PrepareIteration => Self::PrepareIteration,
            Self::WaitingLlm {
                delivery,
                request,
                driver_state,
            } => Self::WaitingLlm {
                delivery: *delivery,
                request: Arc::clone(request),
                driver_state: driver_state.clone(),
            },
            Self::WaitingTools { delivery, calls } => Self::WaitingTools {
                delivery: *delivery,
                calls: calls.clone(),
            },
            Self::WaitingExec {
                delivery,
                language,
                code,
                driver_state,
            } => Self::WaitingExec {
                delivery: *delivery,
                language: language.clone(),
                code: code.clone(),
                driver_state: driver_state.clone(),
            },
            Self::WaitingCheckpoint {
                delivery,
                checkpoint,
                on_empty,
            } => Self::WaitingCheckpoint {
                delivery: *delivery,
                checkpoint: *checkpoint,
                on_empty: on_empty.clone(),
            },
            Self::Finished => Self::Finished,
        }
    }
}

impl<M: TurnProtocol> MachineState<M> {
    pub(super) fn schedule_outstanding_effect(&mut self) {
        match self {
            Self::WaitingExecutionEnvironment { delivery, .. }
            | Self::WaitingLlm { delivery, .. }
            | Self::WaitingTools { delivery, .. }
            | Self::WaitingExec { delivery, .. }
            | Self::WaitingCheckpoint { delivery, .. } => {
                delivery.status = EffectDeliveryStatus::Pending;
            }
            Self::PreparingProtocol | Self::PrepareIteration | Self::Finished => {}
        }
    }

    pub(super) fn poll_outstanding_effect(&mut self) -> Option<Effect<M>> {
        match self {
            Self::WaitingExecutionEnvironment { delivery }
                if delivery.status == EffectDeliveryStatus::Pending =>
            {
                delivery.status = EffectDeliveryStatus::Delivered;
                Some(Effect::SyncExecutionEnvironment { id: delivery.id })
            }
            Self::WaitingLlm {
                delivery, request, ..
            } if delivery.status == EffectDeliveryStatus::Pending => {
                delivery.status = EffectDeliveryStatus::Delivered;
                Some(Effect::LlmCall {
                    id: delivery.id,
                    request: Arc::clone(request),
                })
            }
            Self::WaitingTools { delivery, calls }
                if delivery.status == EffectDeliveryStatus::Pending =>
            {
                delivery.status = EffectDeliveryStatus::Delivered;
                Some(Effect::ToolCalls {
                    id: delivery.id,
                    calls: calls.clone(),
                })
            }
            Self::WaitingExec {
                delivery,
                language,
                code,
                ..
            } if delivery.status == EffectDeliveryStatus::Pending => {
                delivery.status = EffectDeliveryStatus::Delivered;
                Some(Effect::ExecCode {
                    id: delivery.id,
                    language: language.clone(),
                    code: code.clone(),
                })
            }
            Self::WaitingCheckpoint {
                delivery,
                checkpoint,
                ..
            } if delivery.status == EffectDeliveryStatus::Pending => {
                delivery.status = EffectDeliveryStatus::Delivered;
                Some(Effect::Checkpoint {
                    id: delivery.id,
                    checkpoint: *checkpoint,
                })
            }
            _ => None,
        }
    }
}

/// Sans-IO state machine for a single session run (multi-turn).
pub struct TurnMachine<M: TurnProtocol = UnitTurnProtocol> {
    pub(super) config: TurnMachineConfig<M>,
    pub(super) state: MachineState<M>,
    pub(super) side_effect_outbox: VecDeque<Effect<M>>,
    pub(super) next_effect_id: u64,
    pub(super) next_synthetic_message_id: u64,
    pub(super) messages: MessageSequence,
    pub(super) events: Arc<Vec<SessionHistoryRecord<M::Event>>>,
    pub(super) turn_causes: Vec<TurnCause>,
    pub(super) progress_event_cursor: usize,
    pub(super) protocol_iteration: usize,
    pub(super) protocol_run_offset: usize,
    pub(super) cumulative_usage: TokenUsage,
    pub(super) synced_protocol_iteration: Option<usize>,
    /// The cell replay-key grammar the synced iteration's execution
    /// environment named (FIG-3586); `None` until that sync lands and again
    /// from the next iteration on.
    pub(super) synced_cell_replay_grammar: Option<u32>,
    /// Cancellation evidence the host has observed for this turn, recorded
    /// before the machine is told the provider call was cancelled. Lets the
    /// machine name the request that stopped it instead of minting internal
    /// evidence.
    pub(crate) observed_cancellation: Option<crate::TurnCancellationEvidence>,
}
