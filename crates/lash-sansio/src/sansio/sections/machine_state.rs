#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum EffectDeliveryStatus {
    #[default]
    Pending,
    Delivered,
}

#[derive(Debug, Serialize, serde::Deserialize)]
enum MachineState<M: TurnProtocol = UnitTurnProtocol> {
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
    state: MachineState<M>,
    pending_effects: Vec<Effect<M>>,
    next_effect_id: u64,
    #[serde(default)]
    next_synthetic_message_id: u64,
    messages: Vec<Message>,
    events: Vec<SessionHistoryRecord<M::Event>>,
    #[serde(default)]
    turn_causes: Vec<TurnCause>,
    #[serde(default)]
    progress_event_cursor: usize,
    protocol_iteration: usize,
    protocol_run_offset: usize,
    cumulative_usage: TokenUsage,
    termination: TurnTerminationPolicyState,
    synced_protocol_iteration: Option<usize>,
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
    fn schedule_outstanding_effect(&mut self) {
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

    fn poll_outstanding_effect(&mut self) -> Option<Effect<M>> {
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
    config: TurnMachineConfig<M>,
    state: MachineState<M>,
    side_effect_outbox: VecDeque<Effect<M>>,
    next_effect_id: u64,
    next_synthetic_message_id: u64,
    messages: MessageSequence,
    events: Arc<Vec<SessionHistoryRecord<M::Event>>>,
    turn_causes: Vec<TurnCause>,
    progress_event_cursor: usize,
    protocol_iteration: usize,
    protocol_run_offset: usize,
    cumulative_usage: TokenUsage,
    termination: TurnTerminationPolicyState,
    synced_protocol_iteration: Option<usize>,
    /// Cancellation evidence the host has observed for this turn, recorded
    /// before the machine is told the provider call was cancelled. Lets the
    /// machine name the request that stopped it instead of minting internal
    /// evidence.
    pub(crate) observed_cancellation: Option<crate::TurnCancellationEvidence>,
}
