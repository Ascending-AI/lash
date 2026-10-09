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
/// Version 11 (FIG-3571) drops `synced_cell_replay_grammar`: a turn's
/// executable generation is recorded once, at its admission, and checked
/// there before any effect, so no iteration carries a grammar of its own. A
/// v10 checkpoint's cells were fenced by a per-iteration stamp this build no
/// longer reads, so it is refused.
///
/// Version 11 changed in place before the 1.0 cut (FIG-5171, ruling #68): the
/// checkpoint holds bounded machine state and names its transcript (messages,
/// prompt view, history events and a pending model request) by content
/// digest; the content travels beside it as [`TurnCheckpointContent`].
/// It changed in place again (FIG-5206): it names the committed window the
/// turn started from by the host's pin ([`TurnWindow`]), and its content
/// holds only what the turn added to it.
///
/// version_guard(
///     shapes(
///         path = "crates/lash-sansio/src/sansio/machine_state.rs",
///         path = "crates/lash-sansio/src/sansio/turn_protocol.rs",
///         cover(TurnCheckpoint, CheckpointState, CheckpointWork, Effect),
///     ),
///     roots(path = "crates/lash-sansio/src/session_model/message.rs", FlatPart, FlatPartRef),
/// )
/// version_surface = "drain"
/// format_manifest = "TurnCheckpoint"
pub const TURN_CHECKPOINT_SCHEMA_VERSION: u32 = 1;

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
pub(super) enum CheckpointWork<M: TurnProtocol = UnitTurnProtocol> {
    /// A model call: its request is content-addressed, without its leading
    /// `rendered_from_window` messages, which are the window's own render.
    Llm {
        request: CheckpointContentRef,
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
                        rendered_from_window,
                        driver_state,
                    } => CheckpointWork::Llm {
                        request: request.clone(),
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
    /// the messages `window` renders.
    pub(super) fn record(
        state: &MachineState<M>,
        content: &mut TurnCheckpointContent,
        window: Option<&TurnWindow<M::Event>>,
    ) -> Self {
        match state {
            MachineState::PreparingProtocol => Self::PreparingProtocol,
            MachineState::PrepareIteration => Self::PrepareIteration,
            MachineState::Waiting {
                effect_id, work, ..
            } => Self::Waiting {
                effect_id: *effect_id,
                work: match work {
                    PendingWork::Llm {
                        request,
                        driver_state,
                    } => {
                        let shared =
                            window.map_or(0, |window| window.shared_render(&request.messages));
                        let request = if shared == 0 {
                            content.put_value(request.as_ref())
                        } else {
                            let mut own = LlmRequest::clone(request);
                            own.messages.drain(..shared);
                            content.put_value(&own)
                        };
                        CheckpointWork::Llm {
                            request,
                            rendered_from_window: shared,
                            driver_state: driver_state.clone(),
                        }
                    }
                    work => CheckpointWork::Work { work: work.clone() },
                },
            },
            MachineState::Finished => Self::Finished,
        }
    }

    /// The machine state this records, its model request read back from
    /// `content` after the part `window` renders. A restored wait is always
    /// re-delivered.
    pub(super) fn restore(
        self,
        content: &TurnCheckpointContent,
        window: Option<&TurnWindow<M::Event>>,
    ) -> Result<MachineState<M>, TurnCheckpointRestoreError> {
        Ok(match self {
            Self::PreparingProtocol => MachineState::PreparingProtocol,
            Self::PrepareIteration => MachineState::PrepareIteration,
            Self::Waiting { effect_id, work } => MachineState::Waiting {
                effect_id,
                work: match work {
                    CheckpointWork::Llm {
                        request,
                        rendered_from_window,
                        driver_state,
                    } => {
                        let mut request: LlmRequest = content.value(&request)?;
                        if rendered_from_window > 0 {
                            let render = window.map(TurnWindow::render).ok_or_else(|| {
                                outside_window("a model request starts with the window's render")
                            })?;
                            let mut messages = render
                                .as_slice()
                                .get(..rendered_from_window)
                                .ok_or_else(|| {
                                    outside_window(
                                        "a model request starts with more of the window's render than it has",
                                    )
                                })?
                                .to_vec();
                            messages.append(&mut request.messages);
                            request.messages = messages;
                        }
                        PendingWork::Llm {
                            request: Arc::new(request),
                            driver_state,
                        }
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
        })
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
#[serde(deny_unknown_fields)]
pub struct TurnCheckpoint<M: TurnProtocol = UnitTurnProtocol> {
    #[serde(default = "legacy_turn_checkpoint_schema_version")]
    pub(super) schema_version: u32,
    pub(super) state: CheckpointState<M>,
    pub(super) pending_effects: Vec<Effect<M>>,
    pub(super) next_effect_id: u64,
    /// The committed window the turn started from, which a restore is
    /// handed again; `None` for a machine started without one.
    pub(super) window: Option<CheckpointWindow>,
    pub(super) messages: CheckpointMessages,
    /// The ephemeral model-call view, retained independently for resume.
    pub(super) prompt_messages: CheckpointMessages,
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

/// A message sequence a checkpoint names: its first `from_window` messages
/// are the window's, and the rest is content.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CheckpointMessages {
    pub(super) from_window: usize,
    pub(super) rest: CheckpointContentRef,
}

impl CheckpointMessages {
    /// Record `sequence`, storing what it does not share with `window`.
    pub(super) fn record<E: Clone>(
        sequence: &MessageSequence,
        content: &mut TurnCheckpointContent,
        window: Option<&TurnWindow<E>>,
    ) -> Self {
        let from_window = window.map_or(0, |window| window.shared_messages(sequence));
        Self {
            from_window,
            rest: content.put_sequence(sequence.iter().skip(from_window)),
        }
    }

    /// The sequence this records, its leading messages read from `window`.
    pub(super) fn restore<E: Clone>(
        &self,
        content: &TurnCheckpointContent,
        window: Option<&TurnWindow<E>>,
    ) -> Result<MessageSequence, TurnCheckpointRestoreError> {
        let rest = content.sequence(&self.rest)?;
        if self.from_window == 0 {
            return Ok(MessageSequence::from_owned(rest));
        }
        match window {
            Some(window) if self.from_window <= window.messages().len() => {
                Ok(window.messages_then(self.from_window, rest))
            }
            _ => Err(outside_window(
                "a message sequence starts with more of the window than it has",
            )),
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
    /// content digest of the request after its leading messages, and how
    /// many of those are the render of the window it pins. `None` when it
    /// waits on no model call.
    pub fn pending_request(&self) -> Option<(&CheckpointContentRef, usize)> {
        match &self.state {
            CheckpointState::Waiting {
                work:
                    CheckpointWork::Llm {
                        request,
                        rendered_from_window,
                        ..
                    },
                ..
            } => Some((request, *rendered_from_window)),
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

/// Failure to decode or restore an incompatible turn checkpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnCheckpointRestoreError {
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
                    work,
                    delivery,
                };
                None
            }
        }
    }
}

/// Sans-IO state machine for a single session run (multi-turn).
pub struct TurnMachine<M: TurnProtocol = UnitTurnProtocol> {
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
    pub(super) events: crate::AppendVec<SessionHistoryRecord<M::Event>>,
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
    pub(crate) observed_cancellation: Option<crate::TurnCancellationEvidence>,
    /// The work this machine starts at once it has synced, in place of the
    /// driver's first step ([`TurnMachine::resume_with`]). Runtime-only, as
    /// the observed cancellation is: the host that builds the machine hands
    /// it over again.
    pub(crate) resume_work: Option<PendingWork<M>>,
    /// A tool check's request to stop the Run, found in the results being
    /// delivered (ADR 0128). The driver still records those results; the
    /// machine then finishes instead of starting further work.
    pub(super) run_abort: Option<RunAbort>,
}

/// The Run control a tool result carried: the namespaced code and message
/// of the plugin check that stopped the Run.
#[derive(Clone, Debug)]
pub(super) struct RunAbort {
    pub(super) code: crate::FailureCode,
    pub(super) message: String,
}

impl RunAbort {
    /// The first Run abort among `outputs`, in delivery order.
    pub(super) fn first_in<'a>(
        outputs: impl IntoIterator<Item = &'a crate::ToolCallOutput>,
    ) -> Option<Self> {
        outputs
            .into_iter()
            .find_map(|output| match &output.control {
                Some(crate::ToolControl::AbortRun { code, message }) => Some(Self {
                    code: code.clone(),
                    message: message.clone(),
                }),
                _ => None,
            })
    }
}
