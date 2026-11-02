//! The inputs a turn machine was handed, in the order it was handed them.
//!
//! A turn's durable state within the turn is its last commit plus a log, not
//! a checkpoint of the machine: the machine is deterministic, so a fresh
//! machine built the way the first one was and handed the same inputs in
//! the same order reaches the same state. The journal records every input
//! from the moment the host starts it ([`TurnMachine::journal_inputs`]):
//! each poll, each response, each admitted request, each usage the host
//! settles and each finish the host decides. The host commits what was
//! recorded since its last commit with each phase, and a resume replays the
//! committed inputs into a fresh machine ([`TurnMachine::replay_inputs`]).
//!
//! An admitted request is recorded against the request the machine itself
//! waits on: its envelope (without the tools when they are the machine's
//! own) and the messages past those both share, so the journal never holds
//! the transcript again.

use super::*;
use lash_protocol_types::llm::types::{LlmMessage, RequestMessages, RetainedLlmRequest};

/// One input a machine was handed.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
#[serde(bound(serialize = "I: Serialize", deserialize = "I: DeserializeOwned"))]
pub enum MachineInput<I> {
    /// The host polled the machine this many times in a row.
    Polled(u32),
    /// The host answered an effect.
    Answered {
        /// The answer.
        response: RecordedResponse<I>,
        /// Whether a usage overflow it causes fails the turn
        /// ([`TurnMachine::handle_response`]) rather than being handed back
        /// ([`TurnMachine::try_handle_response`]).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        fails_on_overflow: bool,
    },
    /// The host admitted the composed request of the model call the machine
    /// waits on.
    Admitted {
        /// The call's effect.
        id: EffectId,
        /// The request, against the one the machine waits on.
        request: AdmittedDelta,
    },
    /// [`TurnMachine::record_unanswered_call_usage`].
    UnansweredUsage(LlmUsage),
    /// [`TurnMachine::record_unknown_call_usage`].
    UnknownUsage,
    /// [`TurnMachine::settle_unknown_call_usage`].
    SettledUsage {
        /// The reported usage.
        reported: LlmUsage,
        /// Whether no other call was answered since.
        newest: bool,
    },
    /// [`TurnMachine::record_cancellation_evidence`].
    Cancellation(lash_protocol_types::TurnCancellationEvidence),
    /// [`TurnMachine::fail_turn_with_cause`].
    Failed {
        /// The failure's event.
        event: SessionStreamEvent,
        /// Its typed cause.
        cause: lash_protocol_types::TurnErrorCause,
    },
    /// [`TurnMachine::finish_with_outcome`].
    Finished(TurnOutcome),
    /// [`TurnMachine::finish_cancelled`].
    FinishedCancelled(lash_protocol_types::TurnCancellationEvidence),
}

/// A [`Response`] as the journal records it.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
#[serde(bound(serialize = "I: Serialize", deserialize = "I: DeserializeOwned"))]
pub enum RecordedResponse<I> {
    /// [`Response::ExecutionEnvironmentSynced`].
    Synced {
        id: EffectId,
        result: Result<ExecutionEnvironmentSync, ExecutionEnvironmentSyncFailure>,
    },
    /// [`Response::LlmComplete`].
    Llm {
        id: EffectId,
        result: Result<LlmResponse, LlmCallError>,
        text_streamed: bool,
    },
    /// [`Response::ToolResults`].
    Tools {
        id: EffectId,
        results: Vec<CompletedToolCall<I>>,
    },
    /// [`Response::ExecResult`].
    Exec {
        id: EffectId,
        result: Result<lash_protocol_types::ExecResponse, lash_protocol_types::ExecCodeFailure>,
    },
    /// [`Response::Checkpoint`].
    Checkpoint {
        id: EffectId,
        delivery: CheckpointDelivery,
    },
}

impl<I: Clone> RecordedResponse<I> {
    pub(super) fn of(response: &Response<I>) -> Self {
        match response {
            Response::ExecutionEnvironmentSynced { id, result } => Self::Synced {
                id: *id,
                result: result.clone(),
            },
            Response::LlmComplete {
                id,
                result,
                text_streamed,
            } => Self::Llm {
                id: *id,
                result: result.clone(),
                text_streamed: *text_streamed,
            },
            Response::ToolResults { id, results } => Self::Tools {
                id: *id,
                results: results.clone(),
            },
            Response::ExecResult { id, result } => Self::Exec {
                id: *id,
                result: result.clone(),
            },
            Response::Checkpoint { id, delivery } => Self::Checkpoint {
                id: *id,
                delivery: delivery.clone(),
            },
        }
    }

    fn into_response(self) -> Response<I> {
        match self {
            Self::Synced { id, result } => Response::ExecutionEnvironmentSynced { id, result },
            Self::Llm {
                id,
                result,
                text_streamed,
            } => Response::LlmComplete {
                id,
                result,
                text_streamed,
            },
            Self::Tools { id, results } => Response::ToolResults { id, results },
            Self::Exec { id, result } => Response::ExecResult { id, result },
            Self::Checkpoint { id, delivery } => Response::Checkpoint { id, delivery },
        }
    }
}

/// An admitted request against the request the machine waited on.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct AdmittedDelta {
    /// The admitted envelope, without its messages, and without its tools
    /// when they are the waited request's.
    envelope: LlmRequest,
    /// Whether the tools are the waited request's.
    own_tools: bool,
    /// How many leading messages the admitted request shares with the
    /// waited one.
    shared: usize,
    /// The admitted request's messages past those.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tail: Vec<LlmMessage>,
}

impl AdmittedDelta {
    fn of(waited: &RetainedLlmRequest, admitted: &RetainedLlmRequest) -> Self {
        let shared = waited
            .messages()
            .iter()
            .zip(admitted.messages().iter())
            .take_while(|(waited, admitted)| waited == admitted)
            .count();
        let mut envelope = admitted.envelope().clone();
        envelope.messages = Vec::new();
        let own_tools = envelope.tools == waited.envelope().tools;
        if own_tools {
            envelope.tools = Arc::default();
        }
        Self {
            envelope,
            own_tools,
            shared,
            tail: admitted.messages().iter_from(shared).cloned().collect(),
        }
    }

    fn apply(&self, waited: &RetainedLlmRequest) -> Option<RetainedLlmRequest> {
        if waited.messages().len() < self.shared {
            return None;
        }
        let mut envelope = self.envelope.clone();
        if self.own_tools {
            envelope.tools = Arc::clone(&waited.envelope().tools);
        }
        let messages = waited
            .messages()
            .iter()
            .take(self.shared)
            .cloned()
            .chain(self.tail.iter().cloned())
            .collect::<Vec<_>>();
        Some(RetainedLlmRequest::from_envelope(
            envelope,
            RequestMessages::from_messages(messages),
        ))
    }
}

/// Where a replay left the recorded inputs: the input that does not apply
/// to the machine the inputs before it reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayDivergence {
    /// The input's position in the journal.
    pub index: usize,
    /// What did not match.
    pub reason: String,
}

impl std::fmt::Display for ReplayDivergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "input {} of the turn's journal does not apply: {}",
            self.index, self.reason
        )
    }
}

impl std::error::Error for ReplayDivergence {}

/// The recorded inputs not yet committed, after the committed ones.
#[derive(Debug)]
pub(super) struct InputJournal<I> {
    /// How many inputs were committed and dropped from memory.
    committed: usize,
    /// The inputs recorded since.
    pending: Vec<MachineInput<I>>,
    /// Set while a replay applies inputs, which records nothing.
    replaying: bool,
    /// Set once a mark was taken: the next input does not extend the last.
    sealed: bool,
}

impl<I> InputJournal<I> {
    pub(super) fn starting_at(committed: usize) -> Self {
        Self {
            committed,
            pending: Vec::new(),
            replaying: false,
            sealed: false,
        }
    }

    fn record(&mut self, input: MachineInput<I>) {
        if self.replaying {
            return;
        }
        if std::mem::take(&mut self.sealed) {
            self.pending.push(input);
            return;
        }
        if let (MachineInput::Polled(more), Some(MachineInput::Polled(polls))) =
            (&input, self.pending.last_mut())
        {
            *polls = polls.saturating_add(*more);
            return;
        }
        self.pending.push(input);
    }
}

impl<M: TurnProtocol> TurnMachine<M> {
    /// Start journaling the machine's inputs, counting `committed` inputs
    /// already durable: a fresh turn's machine starts at zero once built, a
    /// replayed one at the inputs its replay applied.
    pub fn journal_inputs(&mut self, committed: usize) {
        self.inputs = Some(InputJournal::starting_at(committed));
    }

    /// The inputs recorded since the last [`Self::inputs_committed`], and
    /// the mark that commits them. A later poll starts an input of its own,
    /// so the mark never splits one.
    pub fn stage_inputs(&mut self) -> (&[MachineInput<M::IntentOutcome>], usize) {
        match &mut self.inputs {
            Some(journal) => {
                journal.sealed = true;
                (&journal.pending, journal.committed + journal.pending.len())
            }
            None => (&[], 0),
        }
    }

    /// The inputs up to `mark` ([`Self::stage_inputs`]) are durable.
    pub fn inputs_committed(&mut self, mark: usize) {
        let Some(journal) = &mut self.inputs else {
            return;
        };
        let durable = mark
            .saturating_sub(journal.committed)
            .min(journal.pending.len());
        journal.pending.drain(..durable);
        journal.committed += durable;
    }

    /// How many inputs the journal holds, committed or not.
    #[must_use]
    pub fn journaled_inputs(&self) -> usize {
        self.inputs
            .as_ref()
            .map_or(0, |journal| journal.committed + journal.pending.len())
    }

    /// Apply recorded `inputs` to this machine, built the way the machine
    /// that recorded them was, and then journal from after them. The
    /// effect the last poll delivered is delivered again by the next poll:
    /// the host resumes waiting on it.
    ///
    /// # Errors
    ///
    /// [`ReplayDivergence`] when an input answers or admits an effect the
    /// machine does not wait on: the machine is not the one that recorded
    /// them.
    pub fn replay_inputs(
        &mut self,
        inputs: impl IntoIterator<Item = MachineInput<M::IntentOutcome>>,
    ) -> Result<(), ReplayDivergence> {
        let mut journal = InputJournal::starting_at(0);
        journal.replaying = true;
        self.inputs = Some(journal);
        let mut applied = 0usize;
        for (index, input) in inputs.into_iter().enumerate() {
            let diverged = |reason: String| ReplayDivergence { index, reason };
            match input {
                MachineInput::Polled(polls) => {
                    for _ in 0..polls {
                        let _ = self.poll_effect();
                    }
                    if let Some(error) = self.projection_refusal.take() {
                        return Err(diverged(format!("the projection refused: {error}")));
                    }
                }
                MachineInput::Answered {
                    response,
                    fails_on_overflow,
                } => {
                    let response = response.into_response();
                    let id = response.effect_id();
                    if self.state.waiting_effect() != Some(id) {
                        return Err(diverged(format!(
                            "it answers effect {id:?}, and the machine waits on {:?}",
                            self.state.waiting_effect()
                        )));
                    }
                    if let Err(overflow) = self.handle_unrecorded(response) {
                        if !fails_on_overflow {
                            return Err(diverged(format!(
                                "usage counter `{}` overflowed",
                                overflow.counter()
                            )));
                        }
                        self.fail_on_overflow(overflow);
                    }
                }
                MachineInput::Admitted { id, request } => {
                    let Some(waited) = self.waiting_request(id) else {
                        return Err(diverged(format!(
                            "it admits model call {id:?}, which the machine does not wait on"
                        )));
                    };
                    let Some(admitted) = request.apply(&waited) else {
                        return Err(diverged(
                            "the admitted request shares more messages than the waited one has"
                                .to_owned(),
                        ));
                    };
                    self.admit_request(id, Arc::new(admitted));
                }
                MachineInput::UnansweredUsage(usage) => self
                    .record_unanswered_call_usage(&usage)
                    .map_err(|overflow| {
                    diverged(format!("usage counter `{}` overflowed", overflow.counter()))
                })?,
                MachineInput::UnknownUsage => {
                    self.record_unknown_call_usage().map_err(|overflow| {
                        diverged(format!("usage counter `{}` overflowed", overflow.counter()))
                    })?
                }
                MachineInput::SettledUsage { reported, newest } => self
                    .settle_unknown_call_usage(&reported, newest)
                    .map_err(|overflow| {
                        diverged(format!("usage counter `{}` overflowed", overflow.counter()))
                    })?,
                MachineInput::Cancellation(evidence) => {
                    self.record_cancellation_evidence(evidence);
                }
                MachineInput::Failed { event, cause } => self.fail_turn_with_cause(event, cause),
                MachineInput::Finished(outcome) => self.finish_with_outcome(outcome),
                MachineInput::FinishedCancelled(evidence) => self.finish_cancelled(evidence),
            }
            applied = index + 1;
        }
        self.state.redeliver();
        self.inputs = Some(InputJournal::starting_at(applied));
        Ok(())
    }

    /// The request the machine waits on as model call `id`.
    fn waiting_request(&self, id: EffectId) -> Option<Arc<RetainedLlmRequest>> {
        match &self.state {
            MachineState::Waiting {
                effect_id,
                work: PendingWork::Llm { request, .. },
                ..
            } if *effect_id == id => Some(Arc::clone(request)),
            _ => None,
        }
    }

    pub(super) fn record_input(&mut self, input: impl FnOnce() -> MachineInput<M::IntentOutcome>) {
        if let Some(journal) = &mut self.inputs
            && !journal.replaying
        {
            journal.record(input());
        }
    }

    pub(super) fn record_admitted(&mut self, id: EffectId, admitted: &RetainedLlmRequest) {
        let Some(waited) = self.waiting_request(id) else {
            return;
        };
        self.record_input(|| MachineInput::Admitted {
            id,
            request: AdmittedDelta::of(&waited, admitted),
        });
    }
}
