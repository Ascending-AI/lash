use super::*;

/// Serialized schema version for [`TurnCheckpoint`].
///
/// Version 1 is the historical unstamped checkpoint shape. Version 2 adds the
/// host-reporting effect for tool calls refused before dispatch.
pub const TURN_CHECKPOINT_SCHEMA_VERSION: u32 = 2;

const fn legacy_turn_checkpoint_schema_version() -> u32 {
    1
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum EffectDeliveryStatus {
    #[default]
    Pending,
    Delivered,
}

#[derive(Debug, Serialize, serde::Deserialize)]
pub(super) enum MachineState<M: TurnProtocol = UnitTurnProtocol> {
    PreparingProtocol,
    WaitingExecutionEnvironment {
        effect_id: EffectId,
        update_machine_config: bool,
        #[serde(skip)]
        delivery: EffectDeliveryStatus,
    },
    PrepareIteration,
    WaitingLlm {
        effect_id: EffectId,
        request: Arc<LlmRequest>,
        driver_state: Option<M::DriverState>,
        #[serde(skip)]
        delivery: EffectDeliveryStatus,
    },
    WaitingTools {
        effect_id: EffectId,
        calls: Vec<PendingToolCall>,
        #[serde(skip)]
        delivery: EffectDeliveryStatus,
    },
    WaitingExec {
        effect_id: EffectId,
        language: String,
        code: String,
        driver_state: M::DriverState,
        #[serde(skip)]
        delivery: EffectDeliveryStatus,
    },
    WaitingCheckpoint {
        effect_id: EffectId,
        checkpoint: CheckpointKind,
        on_empty: CheckpointResumeAction,
        #[serde(skip)]
        delivery: EffectDeliveryStatus,
    },
    Finished,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
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
    pub(super) termination: TurnTerminationPolicyState,
    pub(super) synced_protocol_iteration: Option<usize>,
}

impl<M: TurnProtocol> TurnCheckpoint<M> {
    /// Returns the schema version decoded from this checkpoint.
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

impl<M: TurnProtocol> Clone for MachineState<M> {
    fn clone(&self) -> Self {
        match self {
            Self::PreparingProtocol => Self::PreparingProtocol,
            Self::WaitingExecutionEnvironment {
                effect_id,
                update_machine_config,
                delivery,
            } => Self::WaitingExecutionEnvironment {
                effect_id: *effect_id,
                update_machine_config: *update_machine_config,
                delivery: *delivery,
            },
            Self::PrepareIteration => Self::PrepareIteration,
            Self::WaitingLlm {
                effect_id,
                request,
                driver_state,
                delivery,
            } => Self::WaitingLlm {
                effect_id: *effect_id,
                request: Arc::clone(request),
                driver_state: driver_state.clone(),
                delivery: *delivery,
            },
            Self::WaitingTools {
                effect_id,
                calls,
                delivery,
            } => Self::WaitingTools {
                effect_id: *effect_id,
                calls: calls.clone(),
                delivery: *delivery,
            },
            Self::WaitingExec {
                effect_id,
                language,
                code,
                driver_state,
                delivery,
            } => Self::WaitingExec {
                effect_id: *effect_id,
                language: language.clone(),
                code: code.clone(),
                driver_state: driver_state.clone(),
                delivery: *delivery,
            },
            Self::WaitingCheckpoint {
                effect_id,
                checkpoint,
                on_empty,
                delivery,
            } => Self::WaitingCheckpoint {
                effect_id: *effect_id,
                checkpoint: *checkpoint,
                on_empty: on_empty.clone(),
                delivery: *delivery,
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
                *delivery = EffectDeliveryStatus::Pending;
            }
            Self::PreparingProtocol | Self::PrepareIteration | Self::Finished => {}
        }
    }

    pub(super) fn poll_outstanding_effect(&mut self) -> Option<Effect<M>> {
        match self {
            Self::WaitingExecutionEnvironment {
                effect_id,
                update_machine_config,
                delivery,
            } if *delivery == EffectDeliveryStatus::Pending => {
                *delivery = EffectDeliveryStatus::Delivered;
                Some(Effect::SyncExecutionEnvironment {
                    id: *effect_id,
                    update_machine_config: *update_machine_config,
                })
            }
            Self::WaitingLlm {
                effect_id,
                request,
                delivery,
                ..
            } if *delivery == EffectDeliveryStatus::Pending => {
                *delivery = EffectDeliveryStatus::Delivered;
                Some(Effect::LlmCall {
                    id: *effect_id,
                    request: Arc::clone(request),
                })
            }
            Self::WaitingTools {
                effect_id,
                calls,
                delivery,
            } if *delivery == EffectDeliveryStatus::Pending => {
                *delivery = EffectDeliveryStatus::Delivered;
                Some(Effect::ToolCalls {
                    id: *effect_id,
                    calls: calls.clone(),
                })
            }
            Self::WaitingExec {
                effect_id,
                language,
                code,
                delivery,
                ..
            } if *delivery == EffectDeliveryStatus::Pending => {
                *delivery = EffectDeliveryStatus::Delivered;
                Some(Effect::ExecCode {
                    id: *effect_id,
                    language: language.clone(),
                    code: code.clone(),
                })
            }
            Self::WaitingCheckpoint {
                effect_id,
                checkpoint,
                delivery,
                ..
            } if *delivery == EffectDeliveryStatus::Pending => {
                *delivery = EffectDeliveryStatus::Delivered;
                Some(Effect::Checkpoint {
                    id: *effect_id,
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
    pub(super) termination: TurnTerminationPolicyState,
    pub(super) synced_protocol_iteration: Option<usize>,
    /// Cancellation evidence the host has observed for this turn, recorded
    /// before the machine is told the provider call was cancelled. Lets the
    /// machine name the request that stopped it instead of minting internal
    /// evidence.
    pub(crate) observed_cancellation: Option<crate::TurnCancellationEvidence>,
}
