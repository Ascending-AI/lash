#[derive(Debug, Serialize, serde::Deserialize)]
enum MachineState<M: TurnProtocol = UnitTurnProtocol> {
    PreparingProtocol,
    WaitingExecutionEnvironment {
        effect_id: EffectId,
        update_machine_config: bool,
    },
    PrepareIteration,
    WaitingLlm {
        effect_id: EffectId,
        request: Arc<LlmRequest>,
        driver_state: Option<M::DriverState>,
    },
    WaitingTools {
        effect_id: EffectId,
        calls: Vec<PendingToolCall>,
    },
    WaitingExec {
        effect_id: EffectId,
        language: String,
        code: String,
        driver_state: M::DriverState,
    },
    WaitingCheckpoint {
        effect_id: EffectId,
        checkpoint: CheckpointKind,
        on_empty: CheckpointResumeAction,
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
            } => Self::WaitingExecutionEnvironment {
                effect_id: *effect_id,
                update_machine_config: *update_machine_config,
            },
            Self::PrepareIteration => Self::PrepareIteration,
            Self::WaitingLlm {
                effect_id,
                request,
                driver_state,
            } => Self::WaitingLlm {
                effect_id: *effect_id,
                request: Arc::clone(request),
                driver_state: driver_state.clone(),
            },
            Self::WaitingTools { effect_id, calls } => Self::WaitingTools {
                effect_id: *effect_id,
                calls: calls.clone(),
            },
            Self::WaitingExec {
                effect_id,
                language,
                code,
                driver_state,
            } => Self::WaitingExec {
                effect_id: *effect_id,
                language: language.clone(),
                code: code.clone(),
                driver_state: driver_state.clone(),
            },
            Self::WaitingCheckpoint {
                effect_id,
                checkpoint,
                on_empty,
            } => Self::WaitingCheckpoint {
                effect_id: *effect_id,
                checkpoint: *checkpoint,
                on_empty: on_empty.clone(),
            },
            Self::Finished => Self::Finished,
        }
    }
}

impl<M: TurnProtocol> MachineState<M> {
    fn outstanding_effect_id(&self) -> Option<EffectId> {
        match self {
            Self::WaitingExecutionEnvironment { effect_id, .. }
            | Self::WaitingLlm { effect_id, .. }
            | Self::WaitingTools { effect_id, .. }
            | Self::WaitingExec { effect_id, .. }
            | Self::WaitingCheckpoint { effect_id, .. } => Some(*effect_id),
            Self::PreparingProtocol | Self::PrepareIteration | Self::Finished => None,
        }
    }

    fn outstanding_effect(&self) -> Option<Effect<M>> {
        match self {
            Self::WaitingExecutionEnvironment {
                effect_id,
                update_machine_config,
            } => Some(Effect::SyncExecutionEnvironment {
                id: *effect_id,
                update_machine_config: *update_machine_config,
            }),
            Self::WaitingLlm {
                effect_id, request, ..
            } => Some(Effect::LlmCall {
                id: *effect_id,
                request: Arc::clone(request),
            }),
            Self::WaitingTools { effect_id, calls } => Some(Effect::ToolCalls {
                id: *effect_id,
                calls: calls.clone(),
            }),
            Self::WaitingExec {
                effect_id,
                language,
                code,
                ..
            } => Some(Effect::ExecCode {
                id: *effect_id,
                language: language.clone(),
                code: code.clone(),
            }),
            Self::WaitingCheckpoint {
                effect_id,
                checkpoint,
                ..
            } => Some(Effect::Checkpoint {
                id: *effect_id,
                checkpoint: *checkpoint,
            }),
            Self::PreparingProtocol | Self::PrepareIteration | Self::Finished => None,
        }
    }
}

/// Sans-IO state machine for a single session run (multi-turn).
pub struct TurnMachine<M: TurnProtocol = UnitTurnProtocol> {
    config: TurnMachineConfig<M>,
    state: MachineState<M>,
    pending_effects: VecDeque<Effect<M>>,
    active_effect_redelivery: bool,
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
