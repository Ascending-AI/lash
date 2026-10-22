use super::*;

use lash_protocol_types::TURN_CHECKPOINT_SCHEMA_VERSION;

/// The version an unstamped checkpoint reads as: older than every version
/// this build reads, so it is refused.
const fn legacy_turn_checkpoint_schema_version() -> u32 {
    0
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum EffectDeliveryStatus {
    #[default]
    Pending,
    Delivered,
}

#[derive(Debug, Serialize, serde::Deserialize)]
#[serde(bound = "")]
pub(super) enum MachineState<M: TurnProtocol = UnitTurnProtocol> {
    PreparingProtocol,
    PrepareIteration,
    /// Waiting on the host to fulfil `work`, answered under `effect_id`.
    Waiting {
        effect_id: EffectId,
        work: PendingWork<M>,
        /// Whether the machine has handed the effect to the host.
        /// Runtime-only: a restored checkpoint always re-delivers.
        #[serde(skip)]
        delivery: EffectDeliveryStatus,
    },
    Finished,
}

/// The machine state a checkpoint records. A wait on a model call names its
/// request by content digest; every other wait is held as it is.
#[derive(Debug, Serialize, serde::Deserialize)]
#[serde(bound = "")]
pub(super) enum CheckpointState<M: TurnProtocol = UnitTurnProtocol> {
    PreparingProtocol,
    PrepareIteration,
    Waiting {
        effect_id: EffectId,
        work: CheckpointWork<M>,
    },
    Finished,
}

#[derive(Debug, Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[serde(bound = "")]
pub(super) enum CheckpointWork<M: TurnProtocol = UnitTurnProtocol> {
    /// A model call: its request's frame, the request without its
    /// messages, and the chain of its messages after the leading
    /// `rendered_from_window`, which are the window's own render, are
    /// content-addressed.
    Llm {
        request: CheckpointContentRef,
        messages: CheckpointContentRef,
        rendered_from_window: usize,
        driver_state: Option<M::DriverState>,
    },
    /// Any other work, which holds no transcript.
    Work { work: PendingWork<M> },
}

impl<M: TurnProtocol> Clone for CheckpointState<M> {
    fn clone(&self) -> Self {
        match self {
            Self::PreparingProtocol => Self::PreparingProtocol,
            Self::PrepareIteration => Self::PrepareIteration,
            Self::Waiting { effect_id, work } => Self::Waiting {
                effect_id: *effect_id,
                work: match work {
                    CheckpointWork::Llm {
                        request,
                        messages,
                        rendered_from_window,
                        driver_state,
                    } => CheckpointWork::Llm {
                        request: request.clone(),
                        messages: messages.clone(),
                        rendered_from_window: *rendered_from_window,
                        driver_state: driver_state.clone(),
                    },
                    CheckpointWork::Work { work } => CheckpointWork::Work { work: work.clone() },
                },
            },
            Self::Finished => Self::Finished,
        }
    }
}

impl<M: TurnProtocol> CheckpointState<M> {
    /// Record `state`, storing a pending model request in `content` without
    /// the messages `window` renders, building on `base`, the request an
    /// earlier checkpoint stored. Answers the request it stored.
    pub(super) fn record(
        state: &MachineState<M>,
        content: &mut TurnCheckpointContent,
        window: Option<&TurnWindow<M::Event>>,
        base: Option<&StoredRequest>,
        encoded: &mut usize,
    ) -> Result<(Self, Option<StoredRequest>), lash_protocol_types::allocation::AllocationError>
    {
        let recorded = match state {
            MachineState::PreparingProtocol => Self::PreparingProtocol,
            MachineState::PrepareIteration => Self::PrepareIteration,
            MachineState::Waiting {
                effect_id, work, ..
            } => match work {
                PendingWork::Llm {
                    request,
                    driver_state,
                } => {
                    let stored = StoredRequest::record(request, window, base, content, encoded)?;
                    let recorded = Self::Waiting {
                        effect_id: *effect_id,
                        work: CheckpointWork::Llm {
                            request: stored.frame.clone(),
                            messages: stored.messages.head().clone(),
                            rendered_from_window: stored.rendered_from_window,
                            driver_state: driver_state.clone(),
                        },
                    };
                    return Ok((recorded, Some(stored)));
                }
                work => Self::Waiting {
                    effect_id: *effect_id,
                    work: CheckpointWork::Work { work: work.clone() },
                },
            },
            MachineState::Finished => Self::Finished,
        };
        Ok((recorded, None))
    }

    /// The machine state this records, its model request read back from
    /// `content` after the part `window` renders, with the request as it
    /// is stored. A restored wait is always re-delivered.
    pub(super) fn restore(
        self,
        content: &TurnCheckpointContent,
        window: Option<&TurnWindow<M::Event>>,
        policy: &lash_protocol_types::session_model::message::HistoryPresentation,
    ) -> Result<(MachineState<M>, Option<StoredRequest>), TurnCheckpointRestoreError> {
        let mut stored = None;
        let state = match self {
            Self::PreparingProtocol => MachineState::PreparingProtocol,
            Self::PrepareIteration => MachineState::PrepareIteration,
            Self::Waiting { effect_id, work } => MachineState::Waiting {
                effect_id,
                work: match work {
                    CheckpointWork::Llm {
                        request,
                        messages,
                        rendered_from_window,
                        driver_state,
                    } => {
                        let request = StoredRequest::restore(
                            &request,
                            &messages,
                            rendered_from_window,
                            content,
                            window,
                            policy,
                        )?;
                        let work = PendingWork::Llm {
                            request: Arc::clone(&request.request),
                            driver_state,
                        };
                        stored = Some(request);
                        work
                    }
                    CheckpointWork::Work {
                        work: PendingWork::Llm { .. },
                    } => {
                        return Err(TurnCheckpointRestoreError::IncompatibleFormat {
                            message: "a checkpoint holds a model request inline".to_string(),
                        });
                    }
                    CheckpointWork::Work { work } => work,
                },
                delivery: EffectDeliveryStatus::Pending,
            },
            Self::Finished => MachineState::Finished,
        };
        Ok((state, stored))
    }

    /// Whether this waits on driver work other than the environment sync.
    pub(super) fn waits_on_driver_work(&self) -> bool {
        match self {
            Self::Waiting {
                work: CheckpointWork::Llm { .. },
                ..
            } => true,
            Self::Waiting {
                work: CheckpointWork::Work { work },
                ..
            } => !matches!(work, PendingWork::SyncExecutionEnvironment),
            Self::PreparingProtocol | Self::PrepareIteration | Self::Finished => false,
        }
    }
}

/// A turn machine's bounded checkpoint: its machine state and counters, with
/// the transcript named by content digest. Its size does not grow with the
/// transcript; the content it names is [`TurnCheckpointContent`].
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
#[serde(transparent, bound = "")]
pub struct TurnCheckpoint<M: TurnProtocol = UnitTurnProtocol>(pub(super) Arc<CheckpointData<M>>);

impl<M: TurnProtocol> std::ops::Deref for TurnCheckpoint<M> {
    type Target = CheckpointData<M>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[doc(hidden)]
#[derive(Debug, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, bound = "")]
pub struct CheckpointData<M: TurnProtocol = UnitTurnProtocol> {
    #[serde(skip)]
    pub(super) allocation: lash_protocol_types::allocation::AllocationHold,
    #[serde(default = "legacy_turn_checkpoint_schema_version")]
    pub(super) schema_version: u32,
    pub(super) state: CheckpointState<M>,
    pub(super) pending_effects: Vec<Effect<M>>,
    pub(super) next_effect_id: u64,
    /// The committed window the turn started from, which a restore is
    /// handed again; `None` for a machine started without one.
    pub(super) window: Option<CheckpointWindow>,
    pub(super) messages: CheckpointMessages,
    /// The ephemeral model-call view, retained independently for resume:
    /// it names the turn's messages it shares rather than storing them
    /// again.
    pub(super) prompt_messages: CheckpointPrompt,
    /// The history records after the window's.
    pub(super) events: CheckpointContentRef,
    #[serde(default)]
    pub(super) progress_event_cursor: usize,
    pub(super) progress_boundaries: Vec<ProgressBoundary>,
    pub(super) protocol_iteration: usize,
    pub(super) protocol_run_offset: usize,
    pub(super) cumulative_usage: LlmUsage,
    /// The usage the turn's last completed model call reported; `None`
    /// before its first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) last_call_usage: Option<LlmUsage>,
    /// The environment the machine last synced, and the iteration it was
    /// synced for. `None` only before the protocol-start sync is answered.
    pub(super) environment: Option<SyncedEnvironment>,
    /// The turn's decided completion candidates, in decision order: content,
    /// as the history records are, since each holds its control.
    pub(super) decided_completions: CheckpointContentRef,
}
impl<M: TurnProtocol> Clone for CheckpointData<M> {
    fn clone(&self) -> Self {
        Self {
            allocation: self.allocation.clone(),
            schema_version: self.schema_version.clone(),
            state: self.state.clone(),
            pending_effects: self.pending_effects.clone(),
            next_effect_id: self.next_effect_id.clone(),
            window: self.window.clone(),
            messages: self.messages.clone(),
            prompt_messages: self.prompt_messages.clone(),
            events: self.events.clone(),
            progress_event_cursor: self.progress_event_cursor.clone(),
            progress_boundaries: self.progress_boundaries.clone(),
            protocol_iteration: self.protocol_iteration.clone(),
            protocol_run_offset: self.protocol_run_offset.clone(),
            cumulative_usage: self.cumulative_usage.clone(),
            last_call_usage: self.last_call_usage.clone(),
            environment: self.environment.clone(),
            decided_completions: self.decided_completions.clone(),
        }
    }
}

/// A progress boundary that delivered protocol records: how many messages
/// the machine held at it, and the span of its history the boundary's delta
/// was. The owner that resumes the turn replays each one, messages first, so
/// its commit interleaves the two streams as an uncut owner's does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProgressBoundary {
    pub(super) messages: usize,
    pub(super) events: std::ops::Range<usize>,
}

/// The window a checkpoint names: the host's pin and the window's size.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CheckpointWindow {
    pub(super) pin: TurnWindowPin,
    pub(super) messages: usize,
    pub(super) events: usize,
}

impl CheckpointWindow {
    pub(super) fn of<E: Clone>(window: &TurnWindow<E>) -> Self {
        Self {
            pin: window.pin().clone(),
            messages: window.messages().len(),
            events: window.events().len(),
        }
    }

    /// Refuse `window` unless it is the one this names.
    pub(super) fn check<E: Clone>(
        recorded: Option<&Self>,
        window: Option<&TurnWindow<E>>,
    ) -> Result<(), TurnCheckpointRestoreError> {
        match (recorded, window) {
            (None, None) => Ok(()),
            (Some(recorded), Some(window)) => {
                let handed = Self::of(window);
                if recorded.pin == handed.pin
                    && recorded.messages == handed.messages
                    && recorded.events == handed.events
                {
                    Ok(())
                } else {
                    Err(outside_window(&format!(
                        "the checkpoint names window `{}` of {} messages and {} records, not `{}` of {} and {}",
                        recorded.pin.as_str(),
                        recorded.messages,
                        recorded.events,
                        handed.pin.as_str(),
                        handed.messages,
                        handed.events
                    )))
                }
            }
            (Some(recorded), None) => Err(outside_window(&format!(
                "the checkpoint names window `{}`, and none was handed over",
                recorded.pin.as_str()
            ))),
            (None, Some(window)) => Err(outside_window(&format!(
                "the checkpoint names no window, and window `{}` was handed over",
                window.pin().as_str()
            ))),
        }
    }
}

pub(super) fn outside_window(message: &str) -> TurnCheckpointRestoreError {
    TurnCheckpointRestoreError::WindowMismatch {
        message: message.to_owned(),
    }
}

impl<M: TurnProtocol> TurnCheckpoint<M> {
    /// Returns the schema version decoded from this checkpoint.
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// The pin of the committed window the turn started from, which its
    /// restore must be handed again; `None` for a turn started without one.
    pub fn window_pin(&self) -> Option<&TurnWindowPin> {
        self.window.as_ref().map(|window| &window.pin)
    }

    /// The model request the checkpoint waits on, as it names it: the
    /// content digests of the request's frame and of the chain of its
    /// messages after the leading ones, and how many of those are the
    /// render of the window it pins. `None` when it waits on no model call.
    pub fn pending_request(&self) -> Option<(&CheckpointContentRef, &CheckpointContentRef, usize)> {
        match &self.state {
            CheckpointState::Waiting {
                work:
                    CheckpointWork::Llm {
                        request,
                        messages,
                        rendered_from_window,
                        ..
                    },
                ..
            } => Some((request, messages, *rendered_from_window)),
            _ => None,
        }
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

/// A checkpoint with the content it names: what [`TurnMachine::checkpoint`]
/// answers and [`TurnMachine::restore_from_checkpoint`] takes. A host stores
/// the checkpoint as its row and the content beside it, content-addressed.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedTurn<M: TurnProtocol = UnitTurnProtocol> {
    pub checkpoint: TurnCheckpoint<M>,
    pub content: TurnCheckpointContent,
}

impl<M: TurnProtocol> SavedTurn<M> {
    /// Read this turn's retained history without restoring or executing its machine.
    /// The committed window's earlier history is not part of this sequence.
    pub fn recorded_events(
        &self,
    ) -> Result<Vec<lash_protocol_types::SessionHistoryRecord<M::Event>>, TurnCheckpointRestoreError>
    {
        self.content
            .sequence(&self.checkpoint.events)
            .map(|(events, _)| events)
    }
}

/// Failure to decode or restore an incompatible turn checkpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnCheckpointRestoreError {
    /// The serving claim could not admit a source reconstructed by restore.
    Allocation(lash_protocol_types::allocation::AllocationError),
    /// The checkpoint was written by a different schema than this build reads.
    IncompatibleSchemaVersion { actual: u32, expected: u32 },
    /// The bytes do not match the current closed checkpoint shape.
    IncompatibleFormat { message: String },
    /// The checkpoint names content that is not present.
    MissingContent { content: String },
    /// Content present under a digest is not the bytes the digest names.
    CorruptContent { content: String },
    /// The window handed to the restore is not the one the checkpoint names.
    WindowMismatch { message: String },
    /// The protocol driver does not decode the state it parked in the work
    /// the checkpoint waits on
    /// ([`ProtocolDriverHandle::check_parked_state`]).
    UndecodableDriverState(UndecodableDriverState),
}

impl std::fmt::Display for TurnCheckpointRestoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Allocation(error) => std::fmt::Display::fmt(error, formatter),
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
            Self::MissingContent { content } => {
                write!(
                    formatter,
                    "turn checkpoint content `{content}` is not stored"
                )
            }
            Self::CorruptContent { content } => write!(
                formatter,
                "turn checkpoint content `{content}` is not the bytes its digest names"
            ),
            Self::WindowMismatch { message } => {
                write!(formatter, "turn checkpoint window: {message}")
            }
            Self::UndecodableDriverState(refusal) => write!(
                formatter,
                "turn checkpoint holds `{}` driver state this build does not decode: {}",
                refusal.driver, refusal.reason
            ),
        }
    }
}

impl std::error::Error for TurnCheckpointRestoreError {}

impl<M: TurnProtocol> Clone for MachineState<M> {
    fn clone(&self) -> Self {
        match self {
            Self::PreparingProtocol => Self::PreparingProtocol,
            Self::PrepareIteration => Self::PrepareIteration,
            Self::Waiting {
                effect_id,
                work,
                delivery,
            } => Self::Waiting {
                effect_id: *effect_id,
                work: work.clone(),
                delivery: *delivery,
            },
            Self::Finished => Self::Finished,
        }
    }
}

impl<M: TurnProtocol> MachineState<M> {
    pub(super) fn poll_outstanding_effect(&mut self) -> Option<Effect<M>> {
        match self {
            Self::Waiting {
                effect_id,
                work,
                delivery,
            } if *delivery == EffectDeliveryStatus::Pending => {
                *delivery = EffectDeliveryStatus::Delivered;
                Some(work.to_effect(*effect_id))
            }
            _ => None,
        }
    }

    /// Take the work `response` answers, paired with it, leaving the machine
    /// `Finished` until the response's handler moves it on. A response whose
    /// id or kind does not match the outstanding effect is stale: the machine
    /// keeps waiting, its delivery bookkeeping untouched.
    pub(super) fn take_waiting(
        &mut self,
        response: Response<M::IntentOutcome>,
    ) -> Option<AnsweredWork<M>> {
        let Self::Waiting { effect_id, .. } = self else {
            return None;
        };
        if *effect_id != response.effect_id() {
            return None;
        }
        let Self::Waiting {
            effect_id,
            work,
            delivery,
        } = std::mem::replace(self, Self::Finished)
        else {
            return None;
        };
        match work.answer(response) {
            Ok(answered) => Some(answered),
            Err(work) => {
                *self = Self::Waiting {
                    effect_id,
                    work: *work,
                    delivery,
                };
                None
            }
        }
    }
}

/// Sans-IO state machine for a single session run (multi-turn).
pub struct TurnMachine<M: TurnProtocol = UnitTurnProtocol> {
    pub(super) retained: MachineAdmission,

    pub(super) projection_refusal: Option<lash_protocol_types::allocation::ProjectionError>,
    pub(super) config: TurnMachineConfig<M>,
    pub(super) state: MachineState<M>,
    pub(super) side_effect_outbox: VecDeque<Effect<M>>,
    pub(super) next_effect_id: u64,
    /// The committed window the turn started from: its messages lead
    /// `messages`, its records lead `events`, and the checkpoint names it
    /// instead of holding it.
    pub(super) window: Option<TurnWindow<M::Event>>,
    pub(super) messages: MessageSequence,
    pub(super) prompt_messages: MessageSequence,
    pub(super) events: lash_protocol_types::AppendVec<SessionHistoryRecord<M::Event>>,
    pub(super) progress_event_cursor: usize,
    /// The boundaries behind the cursor that delivered protocol records.
    pub(super) progress_boundaries: Vec<ProgressBoundary>,
    pub(super) protocol_iteration: usize,
    pub(super) protocol_run_offset: usize,
    pub(super) cumulative_usage: LlmUsage,
    /// The usage the turn's last completed model call reported.
    pub(super) last_call_usage: Option<LlmUsage>,
    /// The one home of the turn's execution environment: the last recorded
    /// sync, with the protocol iteration it was synced for.
    pub(super) environment: Option<SyncedEnvironment>,
    /// Cancellation evidence the host has observed for this turn, recorded
    /// before the machine is told the provider call was cancelled. Lets the
    /// machine name the request that stopped it instead of minting internal
    /// evidence.
    pub(crate) observed_cancellation: Option<lash_protocol_types::TurnCancellationEvidence>,
    /// The work this machine starts at once it has synced, in place of the
    /// driver's first step ([`TurnMachine::resume_with`]). Runtime-only, as
    /// the observed cancellation is: the host that builds the machine hands
    /// it over again.
    pub(crate) resume_work: Option<PendingWork<M>>,
    /// A tool check's request to stop the Run, found in the results being
    /// delivered (ADR 0128). The driver still records those results; the
    /// machine then finishes instead of starting further work.
    pub(super) run_abort: Option<RunAbort>,
    /// The turn's decided completion candidates, in decision order.
    pub(super) decided_completions: Vec<lash_protocol_types::DecidedCompletion>,
    /// What the turn's last committed checkpoint stored, which the next
    /// builds on (FIG-5955). Runtime-only: a restore starts it from the
    /// content it reads.
    pub(super) content: ContentLedger,
}

#[derive(Debug, Default)]
pub(super) struct MachineAdmission {
    pub(super) adoption: lash_protocol_types::allocation::AllocationHold,
    pub(super) state: lash_protocol_types::allocation::AllocationHold,
    pub(super) resume: lash_protocol_types::allocation::AllocationHold,
    pub(super) cancellation: lash_protocol_types::allocation::AllocationHold,
    pub(super) environment: lash_protocol_types::allocation::AllocationHold,
    pub(super) growth: Vec<lash_protocol_types::allocation::AllocationHold>,
}

/// The Run control a tool result carried: the namespaced code and message
/// of the plugin check that stopped the Run.
#[derive(Clone, Debug)]
pub(super) struct RunAbort {
    pub(super) code: lash_protocol_types::FailureCode,
    pub(super) message: String,
}

impl RunAbort {
    /// The first Run abort among `outputs`, in delivery order.
    pub(super) fn first_in<'a>(
        outputs: impl IntoIterator<Item = &'a lash_protocol_types::ToolCallOutput>,
    ) -> Option<Self> {
        outputs
            .into_iter()
            .find_map(|output| match &output.control {
                Some(lash_protocol_types::ToolControl::AbortRun { code, message }) => Some(Self {
                    code: code.clone(),
                    message: message.clone(),
                }),
                _ => None,
            })
    }
}
