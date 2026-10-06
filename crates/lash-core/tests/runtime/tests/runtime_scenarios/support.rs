use super::*;

const SEED: u64 = 0x5_5c01;
pub(crate) use std::collections::HashMap;

pub(crate) use helpers::RecordingStore;
pub(crate) use lash_core::store::{
    AdmittedHead, CheckpointAdmission, QueuedWorkStore, RunAdmission, RunStore, SessionCommitStore,
    ShiftFence, TurnInputStore,
};
pub(crate) use lash_core::testing::RuntimeStoreTestShiftExt;
pub(crate) use lash_core::{
    LeaseOwnerIdentity, PendingTurnInput, PendingTurnInputDraft, RuntimeCommit, StoreError,
    TurnInput, TurnInputCheckpointBoundary, TurnInputIngress, TurnInputState,
};

#[derive(Clone, Debug)]
pub(crate) struct RuntimeScenario {
    pub(crate) name: &'static str,
    pub(crate) session_id: SessionId,
    pub(crate) host_behavior: RuntimeHostBehavior,
    pub(crate) phases: Vec<RuntimeScenarioPhase>,
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeHostBehavior {
    pub(crate) lease_owner_id: &'static str,
}

impl Default for RuntimeHostBehavior {
    fn default() -> Self {
        Self {
            lease_owner_id: "runtime-scenario-owner",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum RuntimeScenarioPhase {
    Ingress(RuntimeIngressPhase),
    Checkpoint(RuntimeCheckpointPhase),
    LeadingCommandRun(RuntimeLeadingCommandRunPhase),
    TurnWorkAdmission(RuntimeTurnWorkAdmissionPhase),
    NextTurnInputAdmission(RuntimeNextTurnInputAdmissionPhase),
    Lease(RuntimeLeasePhase),
    Fault(RuntimeFaultPhase),
    Commit(RuntimeCommitPhase),
}

impl RuntimeScenarioPhase {
    fn requires_live_session_lease(&self) -> bool {
        matches!(
            self,
            Self::Checkpoint(_)
                | Self::LeadingCommandRun(_)
                | Self::TurnWorkAdmission(_)
                | Self::NextTurnInputAdmission(_)
                | Self::Fault(RuntimeFaultPhase::StaleQueueCompletion)
                | Self::Commit(_)
        )
    }

    fn releases_session_lease(&self) -> bool {
        matches!(
            self,
            Self::Commit(_) | Self::Fault(RuntimeFaultPhase::CommitAfterAdvisoryLeaseRelease)
        )
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RuntimeIngressPhase {
    pub(crate) queue: Vec<RuntimeQueueIngress>,
    pub(crate) turn_inputs: Vec<RuntimeTurnInputIngress>,
    pub(crate) cancel_before_commit: Vec<&'static str>,
    pub(crate) enqueued_classes: Vec<QueuedWorkClass>,
}

impl RuntimeIngressPhase {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn enqueue(mut self, ingress: RuntimeQueueIngress) -> Self {
        self.queue.push(ingress);
        self
    }

    pub(crate) fn enqueue_turn_input(mut self, ingress: RuntimeTurnInputIngress) -> Self {
        self.turn_inputs.push(ingress);
        self
    }

    pub(crate) fn cancel_turn_input_before_commit(mut self, alias: &'static str) -> Self {
        self.cancel_before_commit.push(alias);
        self
    }

    pub(crate) fn expect_enqueued_classes(mut self, classes: Vec<QueuedWorkClass>) -> Self {
        self.enqueued_classes = classes;
        self
    }
}

impl From<RuntimeIngressPhase> for RuntimeScenarioPhase {
    fn from(phase: RuntimeIngressPhase) -> Self {
        Self::Ingress(phase)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RuntimeCheckpointPhase {
    pub(crate) turn_index: Option<usize>,
    pub(crate) defer_interrupted_turn_id: Option<&'static str>,
    pub(crate) cancel_after_deferral: Vec<&'static str>,
    pub(crate) pending_turn_inputs_after_deferral: Vec<RuntimePendingTurnInputExpectation>,
    pub(crate) no_next_turn_input_admission_after_cancellations: bool,
}

impl RuntimeCheckpointPhase {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn turn_index(mut self, turn_index: usize) -> Self {
        self.turn_index = Some(turn_index);
        self
    }

    pub(crate) fn defer_interrupted_turn_inputs(mut self, turn_id: &'static str) -> Self {
        self.defer_interrupted_turn_id = Some(turn_id);
        self
    }

    pub(crate) fn cancel_turn_input_after_deferral(mut self, alias: &'static str) -> Self {
        self.cancel_after_deferral.push(alias);
        self
    }

    pub(crate) fn expect_pending_after_deferral(
        mut self,
        expectations: Vec<RuntimePendingTurnInputExpectation>,
    ) -> Self {
        self.pending_turn_inputs_after_deferral = expectations;
        self
    }

    pub(crate) fn expect_no_next_turn_input_admission_after_cancellations(mut self) -> Self {
        self.no_next_turn_input_admission_after_cancellations = true;
        self
    }
}

impl From<RuntimeCheckpointPhase> for RuntimeScenarioPhase {
    fn from(phase: RuntimeCheckpointPhase) -> Self {
        Self::Checkpoint(phase)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RuntimeLeadingCommandRunPhase {
    pub(crate) expected_count: usize,
    pub(crate) turn_admission_blocked_by_command: Option<bool>,
}

impl RuntimeLeadingCommandRunPhase {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn expect_count(mut self, count: usize) -> Self {
        self.expected_count = count;
        self
    }

    pub(crate) fn expect_turn_work_blocked_before_command(mut self, blocked: bool) -> Self {
        self.turn_admission_blocked_by_command = Some(blocked);
        self
    }
}

impl From<RuntimeLeadingCommandRunPhase> for RuntimeScenarioPhase {
    fn from(phase: RuntimeLeadingCommandRunPhase) -> Self {
        Self::LeadingCommandRun(phase)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeTurnWorkAdmissionPhase {
    pub(crate) boundary: AdmissionBoundary,
    pub(crate) expected_count: usize,
    pub(crate) pending_turn_inputs_after_queue_admission: Vec<RuntimePendingTurnInputExpectation>,
}

impl RuntimeTurnWorkAdmissionPhase {
    pub(crate) fn at(boundary: AdmissionBoundary) -> Self {
        Self {
            boundary,
            expected_count: 0,
            pending_turn_inputs_after_queue_admission: Vec::new(),
        }
    }

    pub(crate) fn expect_count(mut self, count: usize) -> Self {
        self.expected_count = count;
        self
    }

    pub(crate) fn expect_pending_turn_inputs_after_admission(
        mut self,
        expectations: Vec<RuntimePendingTurnInputExpectation>,
    ) -> Self {
        self.pending_turn_inputs_after_queue_admission = expectations;
        self
    }
}

impl From<RuntimeTurnWorkAdmissionPhase> for RuntimeScenarioPhase {
    fn from(phase: RuntimeTurnWorkAdmissionPhase) -> Self {
        Self::TurnWorkAdmission(phase)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RuntimeNextTurnInputAdmissionPhase {
    pub(crate) expected_aliases: Vec<&'static str>,
    pub(crate) expected_texts: Vec<&'static str>,
    pub(crate) verify_pending_turn_inputs_held_after_admission: bool,
}

impl RuntimeNextTurnInputAdmissionPhase {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn expect_inputs(
        mut self,
        aliases: Vec<&'static str>,
        texts: Vec<&'static str>,
    ) -> Self {
        self.expected_aliases = aliases;
        self.expected_texts = texts;
        self
    }

    pub(crate) fn expect_pending_held_after_admission(mut self) -> Self {
        self.verify_pending_turn_inputs_held_after_admission = true;
        self
    }
}

impl From<RuntimeNextTurnInputAdmissionPhase> for RuntimeScenarioPhase {
    fn from(phase: RuntimeNextTurnInputAdmissionPhase) -> Self {
        Self::NextTurnInputAdmission(phase)
    }
}

#[derive(Clone, Debug)]
pub(crate) enum RuntimeLeasePhase {
    ExpireStaleHolder { assert_successor_busy: bool },
}

impl RuntimeLeasePhase {
    pub(crate) fn expire_stale_holder() -> Self {
        Self::ExpireStaleHolder {
            assert_successor_busy: true,
        }
    }
}

impl From<RuntimeLeasePhase> for RuntimeScenarioPhase {
    fn from(phase: RuntimeLeasePhase) -> Self {
        Self::Lease(phase)
    }
}

#[derive(Clone, Debug)]
pub(crate) enum RuntimeFaultPhase {
    StaleQueueCompletion,
    CommitAfterAdvisoryLeaseRelease,
}

impl From<RuntimeFaultPhase> for RuntimeScenarioPhase {
    fn from(phase: RuntimeFaultPhase) -> Self {
        Self::Fault(phase)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RuntimeCommitPhase {
    pub(crate) pending_turn_inputs_empty_after_commit: bool,
    pub(crate) checkpoint_turn_index: Option<usize>,
}

impl RuntimeCommitPhase {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn expect_pending_turn_inputs_empty(mut self) -> Self {
        self.pending_turn_inputs_empty_after_commit = true;
        self
    }

    pub(crate) fn expect_checkpoint_turn_index(mut self, turn_index: usize) -> Self {
        self.checkpoint_turn_index = Some(turn_index);
        self
    }
}

impl From<RuntimeCommitPhase> for RuntimeScenarioPhase {
    fn from(phase: RuntimeCommitPhase) -> Self {
        Self::Commit(phase)
    }
}

#[derive(Clone, Debug)]
pub(crate) enum RuntimeQueueIngress {
    RefreshToolCatalog { reason: &'static str },
    ProcessWake { text: &'static str },
}

impl RuntimeQueueIngress {
    pub(crate) fn batch_draft(&self, session_id: &SessionId) -> QueuedWorkBatchDraft {
        match self {
            Self::RefreshToolCatalog { reason } => QueuedWorkBatchDraft::new(
                session_id,
                DeliveryPolicy::EarliestSafeBoundary,
                SessionCommand::RefreshToolCatalog {
                    reason: (*reason).to_string(),
                },
            ),
            Self::ProcessWake { text } => {
                lash_core::testing::runtime_internals::process_wake_batch_draft(
                    ProcessWakeDelivery {
                        version: lash_core::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
                        target_session_id: SessionId::fixture(session_id.to_string()),
                        process_id: lash_core::ProcessId::fixture(&format!("process:{text}")),
                        sequence: 1,
                        event_type: "process.wake".to_string(),
                        process_caused_by: None,
                        authority: lash_core::QueuedWorkAuthority::default(),
                        input: (*text).to_string(),
                        created_at_ms: 1,
                        trace_cause: Default::default(),
                    },
                )
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum RuntimeTurnInputIngress {
    NextTurn {
        alias: &'static str,
        text: &'static str,
        source_key: Option<&'static str>,
    },
    ReplayNextTurn {
        alias: &'static str,
        text: &'static str,
        source_key: &'static str,
        expected_alias: &'static str,
        expected_text: &'static str,
    },
    ConflictNextTurnReplay {
        text: &'static str,
        source_key: &'static str,
        expected_alias: &'static str,
    },
    NextTurnForSession {
        session_id: SessionId,
        text: &'static str,
    },
    ActiveTurn {
        alias: &'static str,
        turn_id: &'static str,
        min_boundary: TurnInputCheckpointBoundary,
        text: &'static str,
    },
    /// Start the scenario's run, headed by a next-turn input, so input may
    /// address its turn while it runs (ADR 0101 §5.1). The final commit
    /// settles it.
    StartScenarioRun {
        alias: &'static str,
        text: &'static str,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimePendingTurnInputExpectation {
    pub(crate) alias: &'static str,
    pub(crate) ingress: RuntimePendingTurnInputIngressExpectation,
}

#[derive(Clone, Debug)]
pub(crate) enum RuntimePendingTurnInputIngressExpectation {
    /// Pending for the next turn in `state`.
    NextTurn(TurnInputState),
    /// Pending in the state and with the delivery it was submitted with.
    AsSubmitted,
}

pub(crate) fn lease_owner(owner_id: &str) -> LeaseOwnerIdentity {
    LeaseOwnerIdentity::opaque(owner_id, format!("{owner_id}:incarnation"))
}
