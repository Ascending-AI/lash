use super::*;

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
    /// The effect the machine waits on, if it waits on one.
    pub(super) fn waiting_effect(&self) -> Option<EffectId> {
        match self {
            Self::Waiting { effect_id, .. } => Some(*effect_id),
            _ => None,
        }
    }

    /// Deliver the effect the machine waits on again at the next poll: a
    /// replayed machine's host has not seen it.
    pub(super) fn redeliver(&mut self) {
        if let Self::Waiting { delivery, .. } = self {
            *delivery = EffectDeliveryStatus::Pending;
        }
    }

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
    pub(super) projection_refusal: Option<lash_protocol_types::StoredDataCorruption>,
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
    pub(super) consecutive_unproductive: usize,
    pub(super) cumulative_usage: LlmUsage,
    /// The usage the turn's last completed model call reported.
    pub(super) last_call_usage: Option<LlmUsage>,
    /// The one home of the turn's execution environment: the last recorded
    /// sync, with the protocol iteration it was synced for.
    pub(super) environment: Option<Arc<SyncedEnvironment>>,
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
    /// The inputs the machine was handed since its host started journaling
    /// them, and not yet committed ([`TurnMachine::journal_inputs`]).
    pub(super) inputs: Option<super::InputJournal<M::IntentOutcome>>,
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
